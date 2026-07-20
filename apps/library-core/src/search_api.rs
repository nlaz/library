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
/// wanted (`Sync` because the call happens on the image track's thread).
pub fn answer(
    lib: &Library,
    images: &Images,
    data: &Path,
    q: &Query,
    clip_embed: impl Fn(&str) -> Option<ClipEmb> + Sync,
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

    // The two tracks share only the query and the filter, so the image track
    // (CLIP embed + ANN + spread cutoff) runs on a scoped thread while the
    // text track — the dominant cost — runs here: its ~15–20ms hides under
    // the text search instead of adding to the total.
    let (text, img) = std::thread::scope(|s| {
        let img = want_imgs.then(|| s.spawn(|| img_track(images, q, member.as_ref(), &clip_embed)));
        let text = want_text.then(|| text_track(lib, q, member.as_ref()));
        (text, img.map(|h| h.join().expect("image track panicked")))
    });

    let mut phase = "lex";
    // per-stage breakdown, recorded into the perf ring (and, in dev builds,
    // the eprintln! at the bottom)
    let mut stages: Vec<(&'static str, u128)> = Vec::new();
    let mut text_hits: Vec<WireHit> = Vec::new();
    let mut img_hits: Vec<WireHit> = Vec::new();
    let mut ranker = crate::RankerStats::default();
    let mut rel_killed = 0usize;
    let (mut img_fetched, mut img_killed) = (0usize, 0usize);
    let (mut img_top, mut img_floor) = (0.0f32, 0.0f32);
    let mut text_prov: Vec<perf::HitProv> = Vec::new();
    let mut img_prov: Vec<perf::ImgProv> = Vec::new();
    if let Some(t) = text {
        if t.hybrid {
            phase = "hybrid";
        }
        stages.extend(t.stages);
        text_hits = t.hits;
        text_prov = t.prov;
        ranker = t.ranker;
        rel_killed = t.rel_killed;
    }
    if let Some(i) = img {
        if i.embedded {
            phase = if want_text { "hybrid+img" } else { "img" };
        }
        stages.extend(i.stages);
        img_hits = i.hits;
        img_prov = i.prov;
        (img_fetched, img_killed) = (i.fetched, i.killed);
        (img_top, img_floor) = (i.top, i.floor);
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

/// What the text track (ese embed + hybrid search) produced, with its slice
/// of the stage breakdown.
struct TextTrack {
    stages: Vec<(&'static str, u128)>,
    hits: Vec<WireHit>,
    prov: Vec<perf::HitProv>,
    ranker: crate::RankerStats,
    rel_killed: usize,
    /// The query got an embedding — the record's "hybrid" phase marker.
    hybrid: bool,
}

fn text_track(lib: &Library, q: &Query, member: Option<&FxHashSet<String>>) -> TextTrack {
    let t = Instant::now();
    // "full" query, library-wide: the settled query gets semantic search,
    // fuzzy term correction, and MMR diversity. Instant (per-keystroke) and
    // doc-scoped browser-find stay lexical-only and exact.
    let full = q.mode == "full" && q.doc.is_empty();
    let qemb: Option<Emb> = full.then(|| ese::encode_single(&q.q));
    let mut stages: Vec<(&'static str, u128)> = vec![("ese_embed", t.elapsed().as_micros())];
    let qtoks = tokenize(&q.q);
    let k = if q.doc.is_empty() {
        q.offset as usize + K
    } else {
        K_DOC
    };
    let mut ranker = crate::RankerStats::default();
    let mut found = lib.rtx(|r| {
        crate::search(
            &r,
            &q.q,
            qemb.as_ref(),
            k,
            member,
            true,
            full,
            full,
            |key| lib.get(key),
            Some(&mut ranker),
        )
    });
    let mut rel_killed = 0usize;
    if q.doc.is_empty() {
        // degradation cutoff, every page — doc-scoped find is exempt
        // (browser-find coverage must not lose hits)
        let before = found.len();
        found.retain(|h| h.rel >= MIN_REL);
        rel_killed = before - found.len();
    }
    // the search's internal phases, reported instead of one opaque span
    stages.push(("term_expand", ranker.term_expand_us as u128));
    stages.push(("lex_search", ranker.lex_search_us as u128));
    if qemb.is_some() {
        stages.push(("vec_search", ranker.vec_search_us as u128));
    }
    stages.push(("fuse+resolve", ranker.fuse_us as u128));
    TextTrack {
        prov: found
            .iter()
            .take(perf::HITS_PER_RECORD)
            .map(perf::HitProv::from)
            .collect(),
        hits: found.iter().map(|h| wire::wire_hit(h, &qtoks)).collect(),
        stages,
        ranker,
        rel_killed,
        hybrid: qemb.is_some(),
    }
}

/// What the image track (CLIP embed + ANN + spread cutoff) produced.
struct ImgTrack {
    stages: Vec<(&'static str, u128)>,
    hits: Vec<WireHit>,
    prov: Vec<perf::ImgProv>,
    fetched: usize,
    killed: usize,
    top: f32,
    floor: f32,
    /// The encoder produced an embedding — the record's "img" phase marker.
    embedded: bool,
}

fn img_track(
    images: &Images,
    q: &Query,
    member: Option<&FxHashSet<String>>,
    clip_embed: impl Fn(&str) -> Option<ClipEmb>,
) -> ImgTrack {
    // library stream: every figure above the relevance cutoff joins
    // the blend (pagination doles them out)
    let k = if q.kind == "images" { K } else { usize::MAX };
    let t = Instant::now();
    let qemb: Option<ClipEmb> = clip_embed(&q.q);
    let mut out = ImgTrack {
        stages: vec![("clip_embed", t.elapsed().as_micros())],
        hits: Vec::new(),
        prov: Vec::new(),
        fetched: 0,
        killed: 0,
        top: 0.0,
        floor: 0.0,
        embedded: false,
    };
    let Some(e) = qemb else { return out };
    out.embedded = true;
    let t = Instant::now();
    let mut found = images.rtx(|r| crate::image_search(&r, &e, crate::IMG_FETCH, member));
    out.fetched = found.len();
    out.top = found.first().map(|h| h.score).unwrap_or(0.0);
    out.floor = found.last().map(|h| h.score).unwrap_or(0.0);
    if q.doc.is_empty() {
        // degradation cutoff on the top-to-noise-floor spread
        // (raw CLIP sims cluster too tightly for a plain ratio)
        let min = out.floor + crate::IMG_MIN_REL * (out.top - out.floor);
        found.retain(|h| h.score >= min);
        out.killed = out.fetched - found.len();
    }
    out.stages.push(("image_search", t.elapsed().as_micros()));
    out.prov = found
        .iter()
        .take(perf::HITS_PER_RECORD)
        .map(perf::ImgProv::from)
        .collect();
    out.hits = wire::group_image_hits(&found, k);
    out
}
