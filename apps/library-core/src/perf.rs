//! Always-on performance observability behind the hidden perf view: an
//! in-memory ring of recent searches (per-stage timings + per-hit ranker
//! provenance) and per-doc ingest metrics assembled from the status files.
//!
//! Search records are pushed by [`crate::answer`] on every query; both hosts
//! (library-server, library-app) expose the ring read-only. Ingest metrics
//! are written by the ingest worker going forward; [`ingest_rows`] lazily
//! backfills legibility for docs ingested before metrics existed and caches
//! the result back into the doc's row.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use fold::pipeline::terminal::search::HnswSinkStats;
use fold::stream::DbStats;

use crate::legibility::{NOISY_MIN, legibility, min_window};
use crate::meta::Ctx;
use crate::tools::BLANK_CHARS;
use crate::{Hit, ImageHit, Images, Library, Word};

/// Ring capacity: enough to cover a tuning session (instant-mode keystrokes
/// included) without unbounded growth.
pub const SEARCH_LOG_CAP: usize = 200;
/// Provenance rows kept per record — one served page's worth.
pub const HITS_PER_RECORD: usize = 20;

// ---------------------------------------------------------------------------
// Search trace
// ---------------------------------------------------------------------------

/// The text track's lane. The two tracks of a search run on separate threads
/// ([`crate::answer`] spawns the image one under `thread::scope`), so a single
/// depth ordering can't nest them — the lane keeps them apart.
pub const TRACK_TEXT: u8 = 0;
/// The image track's lane.
pub const TRACK_IMAGE: u8 = 1;

/// One measured span of a search.
///
/// `at_us` is the offset from the *search's* start rather than the track's, so
/// spans recorded on the two concurrent tracks compose into one timeline and
/// their overlap is real. `depth` nests a span under the nearest preceding
/// shallower span on the same track; the root is implicit (the record's
/// `total_us`), which is what makes unmeasured time — thread spawn and join,
/// anything between stages — legible as a gap instead of silently inflating
/// whichever span happens to enclose it.
#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub name: String,
    pub at_us: u64,
    pub us: u64,
    pub depth: u8,
    pub track: u8,
}

/// Collects [`Span`]s against a shared origin.
///
/// Both tracks build one of these from the same [`Instant`], which is the
/// whole reason their offsets can be compared. `base` lets a callee collect
/// spans without knowing how deeply the caller has already nested it — the
/// ranker marks its phases at 0 and 1 whether it is running under `answer`'s
/// text track or the agent tool's.
///
/// `Default` exists only because [`crate::RankerStats`] carries a trace
/// through an out-param; a default trace measures from its own creation,
/// which is right for a caller that owns the whole timeline.
#[derive(Debug, Clone)]
pub struct Trace {
    origin: Instant,
    track: u8,
    base: u8,
    spans: Vec<Span>,
}

impl Default for Trace {
    fn default() -> Self {
        Trace::new(Instant::now(), TRACK_TEXT)
    }
}

impl Trace {
    pub fn new(origin: Instant, track: u8) -> Self {
        Trace::at_depth(origin, track, 0)
    }

    /// A trace whose spans nest `base` levels below the timeline's root.
    pub fn at_depth(origin: Instant, track: u8, base: u8) -> Self {
        Trace {
            origin,
            track,
            base,
            spans: Vec::new(),
        }
    }

    /// The instant every `at_us` on this trace is measured from — pass it to
    /// [`Trace::new`] to open a second lane on the same timeline.
    pub fn origin(&self) -> Instant {
        self.origin
    }

    /// Close a span that began at `started`, `depth` levels below this
    /// trace's base. Callers capture `started` before the work and call this
    /// after, so the span covers exactly the work.
    pub fn mark(&mut self, name: &'static str, depth: u8, started: Instant) {
        self.spans.push(Span {
            name: name.to_owned(),
            at_us: started.saturating_duration_since(self.origin).as_micros() as u64,
            us: started.elapsed().as_micros() as u64,
            depth: self.base + depth,
            track: self.track,
        });
    }

    /// Absorb another trace's spans. Both must share this origin — nothing
    /// checks it, and mismatched origins put the spans in the wrong place.
    pub fn absorb(&mut self, other: Trace) {
        self.spans.extend(other.spans);
    }

    pub fn take(self) -> Vec<Span> {
        self.spans
    }
}

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
    /// The search's span tree (text track: `text_track` > ese_embed,
    /// term_expand, lex_search, vec_search, fuse+resolve > fuse, maxsim, mmr,
    /// resolve; image track: `img_track` > clip_embed, image_search; then
    /// blend — spans that didn't run are absent). Order is not significant:
    /// a span closes when its work ends, so parents trail their children.
    /// Every `at_us` is an offset from the same origin, which is what makes
    /// the tracks' concurrency readable — and why the span sum is expected to
    /// exceed `total_us`.
    pub spans: Vec<Span>,
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
// Memory provenance
// ---------------------------------------------------------------------------

/// Where the process's RAM goes, as well as Rust-side accounting can tell.
///
/// The line items are estimates and deliberately don't reconcile with
/// `rss_bytes`: the CLIP ONNX arena is invisible to us, ese's weights are
/// file-backed `.rodata` (resident only as touched pages), fjall memory-maps
/// table files, and allocator retention plus per-thread search scratch are
/// unaccounted. The signed `unaccounted_bytes` remainder carries that gap
/// instead of hiding it.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryBreakdown {
    pub now_ms: u64,
    /// Host-provided resident set size; `None` means the probe failed.
    pub rss_bytes: Option<u64>,
    pub corpus: CorpusMem,
    pub indexes: Vec<IndexMem>,
    pub caches: Vec<CacheMem>,
    pub models: Vec<ModelMem>,
    pub stores: Vec<StoreMem>,
    /// Sum of every RAM line item above (disk figures excluded).
    pub accounted_bytes: u64,
    /// `rss - accounted`; negative when capacity-based estimates exceed a
    /// partially paged-out RSS.
    pub unaccounted_bytes: Option<i64>,
    /// A corpus-sized transient is in flight (all embeddings + chunk text).
    pub atlas_building: bool,
}

/// The document corpus itself: counts plus its on-disk footprint by source.
/// Document content is streamed from disk on demand, so its resident cost
/// shows up as the indexes and caches — never as a line item here.
/// `emb_bytes` is the exact embedding payload (chunks + figures × dim × 4),
/// resident *inside* the HNSW rows and already counted there.
#[derive(Debug, Clone, Serialize)]
pub struct CorpusMem {
    pub docs: usize,
    pub chunks: usize,
    pub figures: usize,
    pub emb_bytes: u64,
    /// data/pdfs — the originals.
    pub pdf_bytes: u64,
    /// data/pages — rendered page scans (usually the biggest slice).
    pub page_bytes: u64,
    /// data/ocr + data/text + data/clean — OCR output and overlays.
    pub ocr_bytes: u64,
    /// keyed_root in library.db — the chunk records (words + embeddings).
    pub chunk_table_bytes: u64,
    /// keyed_root in images.db — the figure records.
    pub figure_table_bytes: u64,
    /// data/models — the CLIP model files fastembed loads.
    pub model_dir_bytes: u64,
}

/// One resident HNSW index: the anny graph plus the sink's id/key maps.
#[derive(Debug, Clone, Serialize)]
pub struct IndexMem {
    pub name: String,
    /// `graph_bytes + map_bytes`.
    pub bytes: u64,
    #[serde(flatten)]
    pub stats: HnswSinkStats,
    /// Dense per-slot cost: vector + layer-0 links + node meta.
    pub per_vector_bytes: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheMem {
    pub name: String,
    pub bytes: u64,
    pub entries: u64,
    pub warmed: bool,
}

/// An embedding model. `bytes: None` marks a model opaque to Rust-side
/// accounting (the CLIP ONNX session) — visible only in RSS.
#[derive(Debug, Clone, Serialize)]
pub struct ModelMem {
    pub name: String,
    pub bytes: Option<u64>,
    pub residency: String,
}

/// One fjall store. `ram_bytes` is what the store holds resident
/// (memtables + block cache + pinned filters/index blocks); the flattened
/// [`DbStats`] carries the disk side, which is never summed into RAM.
#[derive(Debug, Clone, Serialize)]
pub struct StoreMem {
    pub name: String,
    pub ram_bytes: u64,
    #[serde(flatten)]
    pub db: DbStats,
}

fn index_mem(name: &str, stats: HnswSinkStats) -> IndexMem {
    IndexMem {
        name: name.into(),
        bytes: (stats.graph_bytes + stats.map_bytes) as u64,
        per_vector_bytes: (stats.dim * stats.dtype_bytes + stats.m0 * 4 + 8) as u32,
        stats,
    }
}

fn store_mem(name: &str, db: DbStats) -> StoreMem {
    let pinned: u64 = db
        .keyspaces
        .iter()
        .map(|k| k.pinned_filter_bytes + k.pinned_index_bytes)
        .sum();
    StoreMem {
        name: name.into(),
        ram_bytes: db.write_buffer_bytes + db.block_cache_bytes + pinned,
        db,
    }
}

/// Total file bytes under `path`, walked iteratively; missing dirs are 0.
fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if let Ok(md) = e.metadata() {
                total += md.len();
            }
        }
    }
    total
}

#[derive(Clone, Copy)]
struct DiskScan {
    at: std::time::Instant,
    docs: usize,
    pdf: u64,
    pages: u64,
    ocr: u64,
    models: u64,
}

// A full walk of data/pages costs ~1 s cold on a big library — far too much
// for a 5 s poll — and the numbers move only on ingest. One cached scan,
// refreshed on a TTL. Assumes a single data dir per process (true of both
// hosts; the library-core test uses one fixture dir).
static DISK_SCAN: Mutex<Option<DiskScan>> = Mutex::new(None);
const DISK_SCAN_TTL: std::time::Duration = std::time::Duration::from_secs(60);

fn disk_scan(data: &Path) -> DiskScan {
    let mut cached = DISK_SCAN.lock().expect("disk scan lock poisoned");
    if let Some(s) = *cached
        && s.at.elapsed() < DISK_SCAN_TTL
    {
        return s;
    }
    let s = DiskScan {
        at: std::time::Instant::now(),
        docs: std::fs::read_dir(data.join("pages"))
            .map(|d| d.filter_map(|e| e.ok()).count())
            .unwrap_or(0),
        pdf: dir_bytes(&data.join("pdfs")),
        pages: dir_bytes(&data.join("pages")),
        ocr: dir_bytes(&data.join("ocr"))
            + dir_bytes(&data.join("text"))
            + dir_bytes(&data.join("clean")),
        models: dir_bytes(&data.join("models")),
    };
    *cached = Some(s);
    s
}

/// Estimated heap held by the search ring: struct + string/vec heap per
/// record. O([`SEARCH_LOG_CAP`]) — negligible next to a poll.
fn search_log_bytes() -> (usize, usize) {
    let log = SEARCH_LOG.lock().expect("search log lock poisoned");
    let bytes = log
        .iter()
        .map(|r| {
            size_of::<SearchRecord>()
                + r.q.len()
                + r.mode.len()
                + r.kind.len()
                + r.col.len()
                + r.doc.len()
                + r.spans
                    .iter()
                    .map(|s| size_of::<Span>() + s.name.len())
                    .sum::<usize>()
                + r.text_hits
                    .iter()
                    .map(|h| size_of::<HitProv>() + h.doc.len())
                    .sum::<usize>()
                + r.img_hits
                    .iter()
                    .map(|h| size_of::<ImgProv>() + h.doc.len())
                    .sum::<usize>()
        })
        .sum();
    (log.len(), bytes)
}

/// Assemble the breakdown: one read transaction per store for the sink
/// stats, the stores' own fjall numbers, and a TTL-cached walk of the
/// corpus dirs under `data`. `rss_bytes` comes from the host so this crate
/// stays platform-free.
pub fn memory(
    lib: &Library,
    images: &Images,
    data: &Path,
    rss_bytes: Option<u64>,
) -> MemoryBreakdown {
    let (lex_cache, vec_stats) = lib.rtx(|((lex, vec), _)| (lex.cache_stats(), vec.stats()));
    let img_stats = images.rtx(|(vec, _)| vec.stats());
    let (log_entries, log_bytes) = search_log_bytes();
    let scan = disk_scan(data);

    let indexes = vec![
        index_mem("text hnsw (vec)", vec_stats),
        index_mem("image hnsw (imgvec)", img_stats),
    ];
    let caches = vec![
        CacheMem {
            name: "bm25 doclen cache".into(),
            bytes: lex_cache.bytes as u64,
            entries: lex_cache.entries as u64,
            warmed: lex_cache.warmed,
        },
        CacheMem {
            name: "search log ring".into(),
            bytes: log_bytes as u64,
            entries: log_entries as u64,
            warmed: true,
        },
    ];
    let models = vec![
        ModelMem {
            name: "ese (text)".into(),
            bytes: Some(ese::MODEL_BYTES as u64),
            residency: "rodata (file-backed)".into(),
        },
        ModelMem {
            name: "clip (onnx)".into(),
            bytes: None,
            residency: "opaque (RSS only)".into(),
        },
    ];
    let stores = vec![
        store_mem("library.db", lib.db_stats()),
        store_mem("images.db", images.db_stats()),
    ];

    let table_bytes = |store: &StoreMem| {
        store
            .db
            .keyspaces
            .iter()
            .find(|k| k.name == "keyed_root")
            .map_or(0, |k| k.disk_bytes)
    };
    let corpus = CorpusMem {
        docs: scan.docs,
        chunks: vec_stats.live,
        figures: img_stats.live,
        emb_bytes: ((vec_stats.live * crate::EMB_DIM + img_stats.live * crate::CLIP_DIM) * 4)
            as u64,
        pdf_bytes: scan.pdf,
        page_bytes: scan.pages,
        ocr_bytes: scan.ocr,
        chunk_table_bytes: table_bytes(&stores[0]),
        figure_table_bytes: table_bytes(&stores[1]),
        model_dir_bytes: scan.models,
    };

    let accounted_bytes = indexes.iter().map(|i| i.bytes).sum::<u64>()
        + caches.iter().map(|c| c.bytes).sum::<u64>()
        + models.iter().filter_map(|m| m.bytes).sum::<u64>()
        + stores.iter().map(|s| s.ram_bytes).sum::<u64>();

    MemoryBreakdown {
        now_ms: now_ms(),
        rss_bytes,
        unaccounted_bytes: rss_bytes.map(|r| r as i64 - accounted_bytes as i64),
        corpus,
        indexes,
        caches,
        models,
        stores,
        accounted_bytes,
        atlas_building: crate::atlas::building().is_some(),
    }
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
pub fn ingest_rows(ctx: &Ctx) -> Vec<Value> {
    let data = &ctx.data;
    let titles = ctx.titles();

    let mut rows: Vec<Value> = Vec::new();
    for (doc, mut status) in ctx.doc_status_rows() {
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
                obj.insert("metrics".into(), mv.clone());
                // persist it: the backfill is seconds-per-doc on a big
                // library and must happen once, not on every view open
                if let Ok(text) = serde_json::to_string(&m) {
                    let _ = ctx.write(|c| {
                        c.execute(
                            "UPDATE docs SET metrics = ?1 WHERE id = ?2",
                            rusqlite::params![text, doc],
                        )?;
                        Ok(())
                    });
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
                spans: vec![],
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
        let ctx = Ctx::in_memory(data).unwrap();
        ctx.write(|c| {
            c.execute(
                "INSERT INTO docs (id, state, updated_at) VALUES ('somedoc', 'ready', 1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let rows = ingest_rows(&ctx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["doc"], "somedoc");
        assert!(rows[0]["metrics"]["legibility"]["mean"].as_f64().unwrap() > 0.0);
        // and written back, so the next view open doesn't rescore the doc
        let cached = ctx.doc_status_json("somedoc");
        assert!(cached["metrics"]["legibility"].is_object());
        let _ = std::fs::remove_dir_all(data);
    }
}
