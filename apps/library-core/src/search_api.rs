//! The blended search pipeline, shared by both hosts (library-server's HTTP
//! route and the desktop app's Tauri command). Each host owns its stores and
//! its CLIP text encoder; this function orchestrates lexical + semantic text
//! search, optional image search, the degradation cutoffs, and the blend.
//!
//! The CLIP query encoder is passed as a closure rather than a concrete type
//! so library-core need not depend on the host's embedding crate — it is only
//! invoked when image results are actually wanted.

use std::path::Path;
use std::time::Instant;

use serde::Deserialize;

use crate::perf;
use crate::wire::{self, Response, WireHit};
use crate::{ClipEmb, Emb, FxHashSet, Images, Library, MIN_REL, tokenize};

/// One result page of the blended order.
pub const K: usize = 20;
/// Doc-scoped find wants browser-find coverage, not a top-20 shortlist.
pub const K_DOC: usize = 100;

#[derive(Deserialize)]
pub struct Query {
    pub seq: u64,
    pub q: String,
    /// "instant" = lexical only (every keystroke), "full" = hybrid (debounced)
    #[serde(default)]
    pub mode: String,
    /// restrict to a collection from data/collections.json; empty = everything
    #[serde(default)]
    pub col: String,
    /// "all" | "text" | "images" (empty = "all")
    #[serde(default)]
    pub kind: String,
    /// restrict to a single doc id (reader find); takes precedence over `col`
    #[serde(default)]
    pub doc: String,
    /// blended-list offset for infinite scroll; each response is one K-sized
    /// slice of the deterministic blended order. 0 = first page. Ignored for
    /// doc-scoped find (which returns everything up to K_DOC).
    #[serde(default)]
    pub offset: u32,
}

/// Run one query against the text and image stores and return a blended,
/// paginated result page. `clip_embed` encodes a query string into the shared
/// CLIP text/image space; it is called at most once, only when image hits are
/// wanted.
pub fn answer(
    lib: &Library,
    images: &Images,
    data: &Path,
    q: &Query,
    clip_embed: impl Fn(&str) -> Option<ClipEmb>,
) -> Response {
    let start = Instant::now();
    let want_text = q.kind != "images";
    // doc-scoped find is browser-find: lexical matches only. The
    // semantic/CLIP rankers always return their nearest neighbors —
    // relevant or not — which in a single doc means pure noise ticks
    // for any term the doc doesn't contain (no threshold can save a
    // nearest-neighbor list whose top is already irrelevant).
    let want_imgs =
        q.kind == "images" || (q.kind != "text" && q.mode == "full" && q.doc.is_empty());

    // doc/collection filter, pushed down into every ranker
    let member: Option<FxHashSet<String>> = if !q.doc.is_empty() {
        Some(std::iter::once(q.doc.clone()).collect())
    } else {
        (!q.col.is_empty())
            .then(|| wire::read_collections(data).remove(&q.col))
            .flatten()
            .map(|docs| docs.into_iter().collect())
    };

    let mut phase = "lex";
    let mut text_hits: Vec<WireHit> = Vec::new();
    let mut img_hits: Vec<WireHit> = Vec::new();
    // per-stage breakdown, recorded into the perf ring (and, in dev builds,
    // the eprintln! at the bottom)
    let mut stages: Vec<(&'static str, u128)> = Vec::new();
    let mut ranker = crate::RankerStats::default();
    let mut rel_killed = 0usize;
    let (mut img_fetched, mut img_killed) = (0usize, 0usize);
    let (mut img_top, mut img_floor) = (0.0f32, 0.0f32);
    let mut text_prov: Vec<perf::HitProv> = Vec::new();
    let mut img_prov: Vec<perf::ImgProv> = Vec::new();

    if want_text {
        let t = Instant::now();
        // "full" query, library-wide: the settled query gets semantic search,
        // fuzzy term correction, and MMR diversity. Instant (per-keystroke) and
        // doc-scoped browser-find stay lexical-only and exact.
        let full = q.mode == "full" && q.doc.is_empty();
        let qemb: Option<Emb> = full.then(|| ese::encode_single(&q.q));
        stages.push(("ese_embed", t.elapsed().as_micros()));
        if qemb.is_some() {
            phase = "hybrid";
        }
        let qtoks = tokenize(&q.q);
        let k = if q.doc.is_empty() {
            q.offset as usize + K
        } else {
            K_DOC
        };
        let t = Instant::now();
        let mut found = lib.rtx(|r| {
            crate::search(
                &r,
                &q.q,
                qemb.as_ref(),
                k,
                member.as_ref(),
                true,
                full,
                full,
                |key| lib.get(key),
                Some(&mut ranker),
            )
        });
        if q.doc.is_empty() {
            // degradation cutoff, every page — doc-scoped find is exempt
            // (browser-find coverage must not lose hits)
            let before = found.len();
            found.retain(|h| h.rel >= MIN_REL);
            rel_killed = before - found.len();
        }
        stages.push(("lex+rrf", t.elapsed().as_micros()));
        text_prov.extend(
            found
                .iter()
                .take(perf::HITS_PER_RECORD)
                .map(perf::HitProv::from),
        );
        text_hits.extend(found.iter().map(|h| wire::wire_hit(h, &qtoks)));
    }

    if want_imgs {
        // library stream: every figure above the relevance cutoff joins
        // the blend (pagination doles them out)
        let k = if q.kind == "images" { K } else { usize::MAX };
        let t = Instant::now();
        let qemb: Option<ClipEmb> = clip_embed(&q.q);
        stages.push(("clip_embed", t.elapsed().as_micros()));
        if let Some(e) = qemb {
            phase = if want_text { "hybrid+img" } else { "img" };
            let t = Instant::now();
            let mut found =
                images.rtx(|r| crate::image_search(&r, &e, crate::IMG_FETCH, member.as_ref()));
            img_fetched = found.len();
            img_top = found.first().map(|h| h.score).unwrap_or(0.0);
            img_floor = found.last().map(|h| h.score).unwrap_or(0.0);
            if q.doc.is_empty() {
                // degradation cutoff on the top-to-noise-floor spread
                // (raw CLIP sims cluster too tightly for a plain ratio)
                let min = img_floor + crate::IMG_MIN_REL * (img_top - img_floor);
                found.retain(|h| h.score >= min);
                img_killed = img_fetched - found.len();
            }
            stages.push(("image_search", t.elapsed().as_micros()));
            img_prov.extend(
                found
                    .iter()
                    .take(perf::HITS_PER_RECORD)
                    .map(perf::ImgProv::from),
            );
            img_hits.extend(wire::group_image_hits(&found, k));
        }
    }

    let t = Instant::now();
    let mut hits = wire::blend(text_hits, img_hits);
    if q.doc.is_empty() {
        // one page of the blended order; blend is prefix-stable (weights
        // depend only on rank-within-own-list), so slices tile cleanly
        // across continuation requests
        hits = hits.into_iter().skip(q.offset as usize).take(K).collect();
    }
    stages.push(("blend", t.elapsed().as_micros()));

    let total = start.elapsed().as_micros();
    if cfg!(debug_assertions) {
        let breakdown: String = stages
            .iter()
            .map(|(n, us)| format!("{n}={us}us"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "[perf] search {:?} mode={} phase={phase} : {breakdown} total={total}us",
            q.q, q.mode
        );
    }
    perf::record_search(perf::SearchRecord {
        ts_ms: perf::now_ms(),
        q: q.q.clone(),
        mode: q.mode.clone(),
        kind: q.kind.clone(),
        col: q.col.clone(),
        doc: q.doc.clone(),
        offset: q.offset,
        phase: phase.to_owned(),
        total_us: total as u64,
        stages: stages
            .iter()
            .map(|(n, us)| ((*n).to_owned(), *us as u64))
            .collect(),
        lex_n: ranker.lex_n,
        sem_n: ranker.sem_n,
        rel_killed,
        img_fetched,
        img_killed,
        img_top,
        img_floor,
        served: hits.len(),
        zero: hits.is_empty(),
        text_hits: text_prov,
        img_hits: img_prov,
    });
    Response {
        seq: q.seq,
        phase,
        us: total,
        hits,
    }
}
