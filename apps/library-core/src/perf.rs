//! Always-on performance observability behind the hidden perf view: an
//! in-memory ring of recent searches (per-stage timings + per-hit ranker
//! provenance) and per-doc ingest metrics assembled from the status files.
//!
//! Search records are pushed by [`crate::answer`] on every query; both hosts
//! (library-server, library-app) expose the ring read-only. Ingest metrics
//! are written by the ingest worker going forward; [`ingest_rows`] lazily
//! backfills legibility for docs ingested before metrics existed and caches
//! the result back into `data/status/<doc>.json`.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::legibility::{NOISY_MIN, legibility, min_window};
use crate::tools::BLANK_CHARS;
use crate::{Hit, ImageHit, Word};

/// Ring capacity: enough to cover a tuning session (instant-mode keystrokes
/// included) without unbounded growth.
pub const SEARCH_LOG_CAP: usize = 200;
/// Provenance rows kept per record — one served page's worth.
pub const HITS_PER_RECORD: usize = 20;

// ---------------------------------------------------------------------------
// Search records
// ---------------------------------------------------------------------------

/// Per-hit ranker provenance: which list(s) produced the hit and where.
/// `lex_rank == None` marks a semantic-only hit — the kind that bypasses the
/// [`crate::MIN_REL`] cutoff (its `rel` defaults to 1.0).
#[derive(Debug, Clone, Serialize)]
pub struct HitProv {
    pub doc: String,
    pub page: u32,
    pub idx: u32,
    /// RRF fused score (post-MMR order is what's served; this is the fuse).
    pub rrf: f32,
    pub rel: f32,
    pub bm25: f32,
    pub lex_rank: Option<u32>,
    pub sem_rank: Option<u32>,
    pub sem_dist: Option<f32>,
}

impl From<&Hit> for HitProv {
    fn from(h: &Hit) -> Self {
        HitProv {
            doc: h.key.doc.clone(),
            page: h.key.page,
            idx: h.key.idx,
            rrf: h.score,
            rel: h.rel,
            bm25: h.bm25,
            lex_rank: h.lex_rank,
            sem_rank: h.sem_rank,
            sem_dist: h.sem_dist,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImgProv {
    pub doc: String,
    pub page: u32,
    pub idx: u32,
    /// CLIP cosine similarity (higher = closer).
    pub sim: f32,
}

impl From<&ImageHit> for ImgProv {
    fn from(h: &ImageHit) -> Self {
        ImgProv {
            doc: h.key.doc.clone(),
            page: h.key.page,
            idx: h.key.idx,
            sim: h.score,
        }
    }
}

/// One answered query, as the perf view sees it.
#[derive(Debug, Clone, Serialize)]
pub struct SearchRecord {
    /// Unix millis when the query was answered.
    pub ts_ms: u64,
    pub q: String,
    pub mode: String,
    pub kind: String,
    pub col: String,
    pub doc: String,
    pub offset: u32,
    pub phase: String,
    pub total_us: u64,
    /// Per-stage µs (text track: ese_embed, term_expand, lex_search,
    /// vec_search, fuse+resolve; image track: clip_embed, image_search;
    /// then blend — stages that didn't run are absent). The two tracks run
    /// concurrently, so the stage sum can exceed `total_us`.
    pub stages: Vec<(String, u64)>,
    /// Pre-fusion ranker list sizes.
    pub lex_n: usize,
    pub sem_n: usize,
    /// Text hits discarded by the MIN_REL degradation cutoff.
    pub rel_killed: usize,
    /// Image hits fetched / discarded by the spread cutoff, and the spread
    /// itself (top and noise-floor sims at fetch depth).
    pub img_fetched: usize,
    pub img_killed: usize,
    pub img_top: f32,
    pub img_floor: f32,
    /// Hits actually served on this page of results.
    pub served: usize,
    pub zero: bool,
    pub text_hits: Vec<HitProv>,
    pub img_hits: Vec<ImgProv>,
}

static SEARCH_LOG: Mutex<VecDeque<SearchRecord>> = Mutex::new(VecDeque::new());

/// Push a record (newest first), truncating to [`SEARCH_LOG_CAP`].
pub fn record_search(r: SearchRecord) {
    let mut log = SEARCH_LOG.lock().expect("search log lock poisoned");
    log.push_front(r);
    log.truncate(SEARCH_LOG_CAP);
}

/// Snapshot of the ring, newest first.
pub fn search_log() -> Vec<SearchRecord> {
    SEARCH_LOG
        .lock()
        .expect("search log lock poisoned")
        .iter()
        .cloned()
        .collect()
}

/// Unix millis now — the timestamp stamped onto records and metrics.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The perf view's context header: every tuning constant and the live corpus
/// counts, so a screenshot carries the state needed to interpret the numbers.
pub fn meta(chunks: usize, figures: usize, docs: usize) -> Value {
    json!({
        "debug": cfg!(debug_assertions),
        "emb_dim": crate::EMB_DIM,
        "clip_dim": crate::CLIP_DIM,
        "k": crate::K,
        "k_doc": crate::K_DOC,
        "lex_fetch": crate::LEX_FETCH,
        "img_fetch": crate::IMG_FETCH,
        "min_rel": crate::MIN_REL,
        "img_min_rel": crate::IMG_MIN_REL,
        "rrf_k": 60, // the literal in crate::rrf
        "mmr_pool": crate::MMR_POOL,
        "mmr_lambda": crate::MMR_LAMBDA,
        "search_log_cap": SEARCH_LOG_CAP,
        "chunks": chunks,
        "figures": figures,
        "docs": docs,
        "now_ms": now_ms(),
    })
}

// ---------------------------------------------------------------------------
// Ingest metrics
// ---------------------------------------------------------------------------

/// Per-doc OCR quality summary (the CLI `audit` distilled): computed over
/// pages with at least [`BLANK_CHARS`] of text, from the same raw-OCR words
/// (clean overlay preferred) the audit scores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegibilitySummary {
    pub mean: f32,
    pub median: f32,
    /// Fraction of scored pages whose worst window drops below `NOISY_MIN`.
    pub noisy_pct: f32,
    pub scored: u32,
    pub pages: u32,
    /// The 3 worst (page, score) pairs.
    pub worst: Vec<(u32, f32)>,
}

/// Ingest performance for one document, persisted inside the status file.
/// Every field is optional: docs ingested before this existed have `None`s
/// ("not recorded"), which the view renders distinctly from zero.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestMetrics {
    /// Wall-clock ms per stage (ocr/clean/embed/figures/clip/commit_text/
    /// commit_figures/total).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings_ms: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// Pages by word source: (text_layer, vision, cached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<(u32, u32, u32)>,
    /// Chunks (added, removed) at commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<(u32, u32)>,
    /// Figures (added, removed) at commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub figures: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legibility: Option<LegibilitySummary>,
    /// Unix millis when these metrics were recorded.
    #[serde(default)]
    pub at: u64,
}

/// `page-NNNN.json` schema shared with library-ingest (which owns writing it).
#[derive(Deserialize)]
struct PageOcr {
    page: u32,
    words: Vec<Word>,
}

/// A doc's pages for scoring: raw OCR with the sparse `clean/` overlay
/// applied — the same view of the text the CLI audit scores.
fn read_pages(data: &Path, doc: &str) -> Option<Vec<PageOcr>> {
    let dir = data.join("ocr").join(doc);
    let mut pages: Vec<PageOcr> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension().is_none_or(|e| e != "json") {
                return None;
            }
            serde_json::from_slice(&std::fs::read(&p).ok()?).ok()
        })
        .collect();
    let clean = data.join("clean").join(doc);
    for p in &mut pages {
        let f = clean.join(format!("page-{:04}.json", p.page));
        if let Ok(bytes) = std::fs::read(&f)
            && let Ok(over) = serde_json::from_slice(&bytes)
        {
            *p = over;
        }
    }
    pages.sort_by_key(|p| p.page);
    Some(pages)
}

/// Score a doc's OCR quality — the `audit` computation, summarized.
pub fn legibility_summary(data: &Path, doc: &str) -> Option<LegibilitySummary> {
    let pages = read_pages(data, doc)?;
    let total = pages.len() as u32;
    let mut scores: Vec<(u32, f32)> = Vec::new();
    let mut noisy = 0usize;
    for p in &pages {
        let text: String = p
            .words
            .iter()
            .map(|w| w.t.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if text.len() < BLANK_CHARS {
            continue;
        }
        scores.push((p.page, legibility(&text)));
        if min_window(&text) < NOISY_MIN {
            noisy += 1;
        }
    }
    if scores.is_empty() {
        return Some(LegibilitySummary {
            pages: total,
            ..Default::default()
        });
    }
    let mean = scores.iter().map(|(_, s)| s).sum::<f32>() / scores.len() as f32;
    let mut by_score = scores.clone();
    by_score.sort_by(|a, b| a.1.total_cmp(&b.1));
    Some(LegibilitySummary {
        mean,
        median: by_score[by_score.len() / 2].1,
        noisy_pct: noisy as f32 / scores.len() as f32,
        scored: scores.len() as u32,
        pages: total,
        worst: by_score.into_iter().take(3).collect(),
    })
}

/// Has the doc reached a state where its caches are stable enough to score?
/// Guards the lazy backfill against racing an active ingest's status writer
/// (worst case on a miss: a lost cache write, recomputed next open).
fn terminal(state: &str) -> bool {
    matches!(state, "ready" | "text_ready" | "failed")
}

/// One row per document for the perf view's ingest table: the status file
/// (state/stage/error/metrics) joined with title and page count. Docs whose
/// terminal status lacks legibility get it computed here and cached back
/// into the status file (atomic tmp+rename), so the first open backfills
/// the pre-existing library and later opens are cheap.
pub fn ingest_rows(data: &Path) -> Vec<Value> {
    let titles: BTreeMap<String, String> = std::fs::read(data.join("titles.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    let Ok(entries) = std::fs::read_dir(data.join("status")) else {
        return Vec::new();
    };
    let mut rows: Vec<Value> = Vec::new();
    for e in entries {
        let Ok(e) = e else { continue };
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(doc) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some(mut status): Option<Value> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        else {
            continue;
        };
        let state = status["state"].as_str().unwrap_or("").to_owned();
        if state == "deleted" {
            continue;
        }

        // lazy backfill: legibility (and page count) for terminal docs
        // ingested before metrics existed
        if terminal(&state)
            && status["metrics"]["legibility"].is_null()
            && let Some(leg) = legibility_summary(data, &doc)
        {
            let mut m: IngestMetrics = serde_json::from_value(status["metrics"].clone())
                .ok()
                .unwrap_or_default();
            m.pages = m.pages.or(Some(leg.pages));
            m.legibility = Some(leg);
            if m.at == 0 {
                m.at = now_ms();
            }
            if let (Some(obj), Ok(mv)) = (status.as_object_mut(), serde_json::to_value(&m)) {
                obj.insert("metrics".into(), mv);
                let tmp = path.with_extension("json.tmp");
                if let Ok(bytes) = serde_json::to_vec_pretty(&status)
                    && std::fs::write(&tmp, bytes).is_ok()
                {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }

        let pages = crate::wire::count_pages(&data.join("pages").join(&doc));
        let title = titles
            .get(&doc)
            .cloned()
            .unwrap_or_else(|| crate::tools::derive_title(&doc));
        if let Some(obj) = status.as_object_mut() {
            obj.insert("doc".into(), json!(doc));
            obj.insert("title".into(), json!(title));
            obj.insert("pages".into(), json!(pages));
        }
        rows.push(status);
    }
    rows.sort_by(|a, b| a["doc"].as_str().cmp(&b["doc"].as_str()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_caps_and_orders_newest_first() {
        for i in 0..(SEARCH_LOG_CAP + 10) {
            record_search(SearchRecord {
                ts_ms: i as u64,
                q: format!("q{i}"),
                mode: "full".into(),
                kind: "all".into(),
                col: String::new(),
                doc: String::new(),
                offset: 0,
                phase: "hybrid".into(),
                total_us: 0,
                stages: vec![],
                lex_n: 0,
                sem_n: 0,
                rel_killed: 0,
                img_fetched: 0,
                img_killed: 0,
                img_top: 0.0,
                img_floor: 0.0,
                served: 0,
                zero: true,
                text_hits: vec![],
                img_hits: vec![],
            });
        }
        let log = search_log();
        assert_eq!(log.len(), SEARCH_LOG_CAP);
        assert_eq!(log[0].q, format!("q{}", SEARCH_LOG_CAP + 9));
    }

    #[test]
    fn ingest_rows_backfills_legibility() {
        let data =
            std::env::temp_dir().join(format!("library-core-perf-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let data = data.as_path();
        std::fs::create_dir_all(data.join("status")).unwrap();
        std::fs::create_dir_all(data.join("ocr/somedoc")).unwrap();
        let words: Vec<Value> = "the quick brown fox jumps over the lazy dog and keeps running"
            .split(' ')
            .map(|t| json!({"t": t, "x": 0.1, "y": 0.1, "w": 0.05, "h": 0.01}))
            .collect();
        std::fs::write(
            data.join("ocr/somedoc/page-0001.json"),
            serde_json::to_vec(&json!({"page": 1, "words": words})).unwrap(),
        )
        .unwrap();
        std::fs::write(
            data.join("status/somedoc.json"),
            serde_json::to_vec(&json!({"state": "ready", "done": 0, "total": 0, "updated": 1}))
                .unwrap(),
        )
        .unwrap();

        let rows = ingest_rows(data);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["doc"], "somedoc");
        assert!(rows[0]["metrics"]["legibility"]["mean"].as_f64().unwrap() > 0.0);
        // cached back into the status file
        let cached: Value =
            serde_json::from_slice(&std::fs::read(data.join("status/somedoc.json")).unwrap())
                .unwrap();
        assert!(cached["metrics"]["legibility"].is_object());
        let _ = std::fs::remove_dir_all(data);
    }
}
