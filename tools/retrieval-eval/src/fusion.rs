//! Offline fusion sweep: pull the raw BM25 and HNSW candidate lists per
//! query — the exact inputs rank::search feeds its fusion step — and score
//! alternative fusion functions against the gold labels without touching
//! library-core. The winner gets implemented in rank.rs and confirmed by
//! the real pipeline eval.

use crate::data::Pairs;
use crate::encoder_eval::{EvalResult, average};
use crate::metrics;
use crate::pipeline_eval::{build_library, doc_index};
use fxhash::FxHashSet;
use library_core::{Emb, tokenize};
use rayon::prelude::*;
use std::time::Instant;

/// The fusion step's inputs for one query, in rank order.
struct Lists {
    /// (corpus doc index, raw BM25 score) — up to LEX_FETCH=512 deep
    lex: Vec<(usize, f32)>,
    /// (corpus doc index, cosine distance) — capped by the HNSW K=40
    sem: Vec<(usize, f32)>,
}

/// Generic weighted RRF over the two lists. `rel_weight` additionally
/// scales each lexical vote by the doc's BM25 relative to the query's top
/// hit (the `Hit::rel` signal) — full votes for confident matches, near-zero
/// for incidental word overlap.
fn wrrf(
    l: &Lists,
    w_lex: f32,
    w_sem: f32,
    k: f32,
    lex_depth: usize,
    rel_weight: bool,
) -> Vec<usize> {
    let top_bm25 = l.lex.first().map(|&(_, s)| s).unwrap_or(0.0);
    let mut scores: Vec<(f32, usize)> = Vec::new();
    let mut add = |doc: usize, s: f32| match scores.iter_mut().find(|(_, d)| *d == doc) {
        Some((v, _)) => *v += s,
        None => scores.push((s, doc)),
    };
    for (rank, &(doc, bm25)) in l.lex.iter().take(lex_depth).enumerate() {
        let conf = if rel_weight && top_bm25 > 0.0 {
            bm25 / top_bm25
        } else {
            1.0
        };
        add(doc, w_lex * conf / (k + rank as f32));
    }
    for (rank, &(doc, _)) in l.sem.iter().enumerate() {
        add(doc, w_sem / (k + rank as f32));
    }
    scores.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scores.into_iter().map(|(_, d)| d).collect()
}

/// Convex blend of per-query-normalized scores: `α·lex_rel + (1−α)·sem_rel`
/// where lex_rel = bm25/top_bm25 and sem_rel = similarity/top_similarity.
fn score_blend(l: &Lists, alpha: f32) -> Vec<usize> {
    let top_bm25 = l.lex.first().map(|&(_, s)| s).unwrap_or(0.0);
    let top_sim = l.sem.first().map(|&(_, d)| 1.0 - d).unwrap_or(0.0);
    let mut scores: Vec<(f32, usize)> = Vec::new();
    let mut add = |doc: usize, s: f32| match scores.iter_mut().find(|(_, d)| *d == doc) {
        Some((v, _)) => *v += s,
        None => scores.push((s, doc)),
    };
    for &(doc, bm25) in &l.lex {
        if top_bm25 > 0.0 {
            add(doc, alpha * bm25 / top_bm25);
        }
    }
    for &(doc, dist) in &l.sem {
        if top_sim > 0.0 {
            add(doc, (1.0 - alpha) * (1.0 - dist) / top_sim);
        }
    }
    scores.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scores.into_iter().map(|(_, d)| d).collect()
}

type Variant = (&'static str, fn(&Lists) -> Vec<usize>);

const VARIANTS: &[Variant] = &[
    ("rrf60 (pre-2026-07)", |l| {
        wrrf(l, 1.0, 1.0, 60.0, 512, false)
    }),
    ("rrf60 wlex=0.5", |l| wrrf(l, 0.5, 1.0, 60.0, 512, false)),
    ("rrf60 wlex=0.25", |l| wrrf(l, 0.25, 1.0, 60.0, 512, false)),
    ("rrf60 rel-weighted lex", |l| {
        wrrf(l, 1.0, 1.0, 60.0, 512, true)
    }),
    ("rrf60 rel-wlex=0.5", |l| wrrf(l, 0.5, 1.0, 60.0, 512, true)),
    ("rrf60 lexdepth=40", |l| wrrf(l, 1.0, 1.0, 60.0, 40, false)),
    ("rrf10", |l| wrrf(l, 1.0, 1.0, 10.0, 512, false)),
    ("score-blend α=0.5 (ships today)", |l| score_blend(l, 0.5)),
    ("score-blend α=0.4", |l| score_blend(l, 0.4)),
    ("score-blend α=0.35", |l| score_blend(l, 0.35)),
    ("score-blend α=0.3", |l| score_blend(l, 0.3)),
    ("score-blend α=0.25", |l| score_blend(l, 0.25)),
    ("score-blend α=0.2", |l| score_blend(l, 0.2)),
];

pub fn run(pairs: &Pairs, ks: &[usize]) -> Vec<(String, EvalResult)> {
    let dir = std::env::temp_dir().join(format!("retrieval-eval-fusion-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("building library ({} docs) ...", pairs.answers.len());
    let lib = build_library(&dir, &pairs.answers);
    let qembs: Vec<Emb> = pairs.questions.iter().map(ese::encode_single).collect();

    // one search per query, shared by every variant
    let start = Instant::now();
    let lists: Vec<Lists> = pairs
        .questions
        .par_iter()
        .zip(&qembs)
        .map(|(q, qemb)| {
            // mirror rank::search with complete=false, fuzzy=false: the
            // expanded query is just the normalized tokens rejoined
            let expanded = tokenize(q).join(" ");
            lib.rtx(|((lex, vec), _)| Lists {
                lex: lex
                    .search(&expanded, library_core::LEX_FETCH)
                    .into_iter()
                    .map(|h| (doc_index(&h.val), h.score as f32))
                    .collect(),
                sem: vec
                    .search(qemb)
                    .into_iter()
                    .map(|h| (doc_index(&h.val), h.score))
                    .collect(),
            })
        })
        .collect();
    let search_ms = start.elapsed().as_millis() as u64;

    let out = VARIANTS
        .iter()
        .map(|(name, fuse)| {
            let per_query: Vec<Vec<f64>> = lists
                .par_iter()
                .enumerate()
                .map(|(qi, l)| {
                    let ranked = fuse(l);
                    let gold: FxHashSet<usize> = std::iter::once(qi).collect();
                    let mut row = Vec::with_capacity(ks.len() + 2);
                    for &k in ks {
                        row.push(metrics::recall_at_k(&ranked, &gold, k));
                    }
                    row.push(metrics::mrr_at_k(&ranked, &gold, 10));
                    row.push(metrics::ndcg_at_k(&ranked, &gold, 10));
                    row
                })
                .collect();
            (
                name.to_string(),
                EvalResult {
                    metrics: average(ks, &per_query),
                    wall_ms: search_ms,
                },
            )
        })
        .collect();

    drop(lib);
    let _ = std::fs::remove_dir_all(&dir);
    out
}
