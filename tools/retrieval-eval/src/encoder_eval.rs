//! Encoder-level eval: exact brute-force cosine ranking, no ANN, no BM25 —
//! changes here reflect embedding quality alone.

use crate::data::Pairs;
use crate::encoders::Encoder;
use crate::metrics;
use fxhash::FxHashSet;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::time::Instant;

pub struct EvalResult {
    /// metric name → macro-averaged value, BTree for stable column order
    pub metrics: BTreeMap<String, f64>,
    pub wall_ms: u64,
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Rank all docs for each query by exact cosine and macro-average the
/// metrics. Gold for query i is doc i (see [`Pairs`]).
pub fn run(enc: &dyn Encoder, pairs: &Pairs, ks: &[usize]) -> EvalResult {
    let start = Instant::now();
    let mut docs = enc.encode(&pairs.answers);
    let mut queries = enc.encode(&pairs.questions);
    docs.iter_mut().for_each(|v| l2_normalize(v));
    queries.iter_mut().for_each(|v| l2_normalize(v));

    let max_k = ks.iter().copied().max().unwrap_or(10).max(10);
    let per_query: Vec<Vec<f64>> = queries
        .par_iter()
        .enumerate()
        .map(|(qi, q)| {
            let mut scored: Vec<(f32, usize)> = docs
                .iter()
                .enumerate()
                .map(|(di, d)| (q.iter().zip(d).map(|(a, b)| a * b).sum::<f32>(), di))
                .collect();
            let top = max_k.min(scored.len());
            if top < scored.len() {
                scored.select_nth_unstable_by(top, |a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            }
            scored.truncate(top);
            scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            let ranked: Vec<usize> = scored.into_iter().map(|(_, di)| di).collect();

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

    EvalResult {
        metrics: average(ks, &per_query),
        wall_ms: start.elapsed().as_millis() as u64,
    }
}

/// Macro-average per-query metric rows laid out as [recall@k..., mrr, ndcg].
pub fn average(ks: &[usize], per_query: &[Vec<f64>]) -> BTreeMap<String, f64> {
    let n = per_query.len().max(1) as f64;
    let mean = |col: usize| per_query.iter().map(|r| r[col]).sum::<f64>() / n;
    let mut out = BTreeMap::new();
    for (col, &k) in ks.iter().enumerate() {
        out.insert(format!("recall@{k:02}"), mean(col));
    }
    out.insert("mrr@10".to_string(), mean(ks.len()));
    out.insert("ndcg@10".to_string(), mean(ks.len() + 1));
    out
}
