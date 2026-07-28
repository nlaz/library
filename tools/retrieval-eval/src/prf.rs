//! Offline pseudo-relevance-feedback lab: assume BM25's round-1 top hits
//! are on-topic, borrow their most distinctive terms, search again with the
//! expanded query, and fuse as usual. Measures whether the oldest query-
//! expansion trick in IR pays on this corpus — no models, no ingest
//! changes, one extra BM25 round.

use crate::data::Pairs;
use crate::encoder_eval::EvalResult;
use crate::fusion::{Lists, score_blend};
use crate::metrics;
use crate::pipeline_eval::{build_library, doc_index};
use fxhash::{FxHashMap, FxHashSet};
use library_core::{Emb, tokenize};
use rayon::prelude::*;
use std::time::Instant;

/// (feedback docs K, expansion terms E) settings under test.
const SETTINGS: &[(usize, usize)] = &[(3, 3), (3, 5), (3, 10), (5, 5)];

pub fn run(pairs: &Pairs, ks: &[usize]) -> Vec<(String, EvalResult)> {
    let dir = std::env::temp_dir().join(format!("retrieval-eval-prf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("building library ({} docs) ...", pairs.answers.len());
    let lib = build_library(&dir, &pairs.answers);
    let qembs: Vec<Emb> = pairs.questions.iter().map(ese::encode_single).collect();

    // corpus term statistics for scoring candidate expansion terms
    let n_docs = pairs.answers.len() as f32;
    let doc_tokens: Vec<Vec<String>> = pairs.answers.par_iter().map(|a| tokenize(a)).collect();
    let mut df: FxHashMap<&str, u32> = FxHashMap::default();
    for toks in &doc_tokens {
        let uniq: FxHashSet<&str> = toks.iter().map(String::as_str).collect();
        for t in uniq {
            *df.entry(t).or_insert(0) += 1;
        }
    }
    let idf = |t: &str| ((n_docs + 1.0) / (df.get(t).copied().unwrap_or(0) as f32 + 1.0)).ln();

    let search_lex = |q: &str| -> Vec<(usize, f32)> {
        lib.rtx(|((lex, _), _)| {
            lex.search(q, library_core::LEX_FETCH)
                .into_iter()
                .map(|h| (doc_index(&h.val), h.score as f32))
                .collect()
        })
    };

    let start = Instant::now();
    struct QueryRuns {
        /// [round-1 hybrid, round-1 lex, then per SETTINGS: prf lex, prf hybrid]
        rankings: Vec<Vec<usize>>,
    }
    let per_query: Vec<QueryRuns> = pairs
        .questions
        .par_iter()
        .zip(&qembs)
        .map(|(q, qemb)| {
            let expanded = tokenize(q).join(" ");
            let lex1 = search_lex(&expanded);
            let sem = lib.rtx(|((_, vec), _)| {
                vec.search(qemb)
                    .into_iter()
                    .map(|h| (doc_index(&h.val), h.score))
                    .collect::<Vec<_>>()
            });
            let l1 = Lists {
                lex: lex1.clone(),
                sem,
            };
            let mut rankings = vec![
                score_blend(&l1, 0.5),
                lex1.iter().map(|&(d, _)| d).collect(),
            ];

            let q_terms: FxHashSet<String> = tokenize(q).into_iter().collect();
            for &(k, e) in SETTINGS {
                // candidate terms from the top-K round-1 docs, scored tf·idf
                let mut tf: FxHashMap<&str, f32> = FxHashMap::default();
                for &(d, _) in lex1.iter().take(k) {
                    for t in &doc_tokens[d] {
                        *tf.entry(t.as_str()).or_insert(0.0) += 1.0;
                    }
                }
                let mut cands: Vec<(&str, f32)> = tf
                    .into_iter()
                    .filter(|(t, _)| !q_terms.contains(*t) && t.len() > 2)
                    .map(|(t, f)| (t, f * idf(t)))
                    .collect();
                cands.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
                cands.truncate(e);

                let mut q2 = expanded.clone();
                for (t, _) in &cands {
                    q2.push(' ');
                    q2.push_str(t);
                }
                let lex2 = search_lex(&q2);
                rankings.push(lex2.iter().map(|&(d, _)| d).collect());
                let l2 = Lists {
                    lex: lex2,
                    sem: l1.sem.clone(),
                };
                rankings.push(score_blend(&l2, 0.5));
            }
            QueryRuns { rankings }
        })
        .collect();
    let wall = start.elapsed().as_millis() as u64;

    let mut names = vec![
        "hybrid (round1, ships)".to_string(),
        "lex-only round1".to_string(),
    ];
    for &(k, e) in SETTINGS {
        names.push(format!("prf lex K{k} E{e}"));
        names.push(format!("prf hybrid K{k} E{e}"));
    }

    let out = names
        .iter()
        .enumerate()
        .map(|(ci, name)| {
            let rows: Vec<Vec<f64>> = per_query
                .iter()
                .enumerate()
                .map(|(qi, qr)| {
                    let ranked = &qr.rankings[ci];
                    let gold: FxHashSet<usize> = std::iter::once(qi).collect();
                    let mut row = Vec::with_capacity(ks.len() + 2);
                    for &k in ks {
                        row.push(metrics::recall_at_k(ranked, &gold, k));
                    }
                    row.push(metrics::mrr_at_k(ranked, &gold, 10));
                    row.push(metrics::ndcg_at_k(ranked, &gold, 10));
                    row
                })
                .collect();
            (name.clone(), EvalResult::from_rows(ks, rows, wall))
        })
        .collect();

    drop(lib);
    let _ = std::fs::remove_dir_all(&dir);
    out
}
