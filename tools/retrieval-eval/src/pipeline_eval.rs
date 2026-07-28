//! Pipeline-level eval: build a temp-dir Library from the corpus (real
//! ingest embedding path), then measure what the app's ranker actually
//! returns under each configuration. Differences against the encoder-level
//! numbers are pipeline effects: RRF fusion, the compile-time HNSW
//! candidate cap (K=40 in store.rs), MMR.

use crate::data::Pairs;
use crate::encoder_eval::EvalResult;
use crate::metrics;
use fxhash::FxHashSet;
use library_core::{ChunkKey, ChunkRec, Emb, Library, Word, open, search};
use rayon::prelude::*;
use std::time::Instant;

struct Cfg {
    name: &'static str,
    sem: bool,
    lex: bool,
    diversify: bool,
}

const CONFIGS: &[Cfg] = &[
    Cfg {
        name: "lex-only",
        sem: false,
        lex: true,
        diversify: false,
    },
    Cfg {
        name: "sem-only",
        sem: true,
        lex: false,
        diversify: false,
    },
    Cfg {
        name: "hybrid",
        sem: true,
        lex: true,
        diversify: false,
    },
    Cfg {
        name: "hybrid+mmr",
        sem: true,
        lex: true,
        diversify: true,
    },
];

fn chunk(i: usize, text: &str, emb: Emb) -> ChunkRec {
    let words = text
        .split_whitespace()
        .map(|t| Word {
            t: t.to_string(),
            x: 0.0,
            y: 0.0,
            w: 0.1,
            h: 0.1,
        })
        .collect();
    ChunkRec {
        key: ChunkKey {
            doc: format!("d{i}"),
            page: 1,
            idx: 0,
        },
        words,
        emb,
    }
}

/// One single-chunk doc per corpus answer, embedded the way real ingest
/// does it (one batched `ese::encode` call).
pub fn build_library(dir: &std::path::Path, answers: &[String]) -> Library {
    let embs = ese::encode(answers);
    let mut lib = open(dir);
    for batch in embs
        .into_iter()
        .enumerate()
        .collect::<Vec<_>>()
        .chunks(1024)
    {
        lib.wtx(|tx| {
            for (i, emb) in batch {
                let rec = chunk(*i, &answers[*i], *emb);
                tx.upsert(&rec.key, &rec);
            }
        });
    }
    lib
}

/// `ChunkKey.doc` is "d{i}" — recover the corpus index.
pub fn doc_index(key: &ChunkKey) -> usize {
    key.doc[1..].parse().expect("doc id is d<index>")
}

/// Export each query's hybrid top-k as JSON — the input a reranker spike
/// needs: the authoritative candidate lists from the real pipeline, with
/// fused scores, keyed back to page ids.
pub fn dump_hybrid_topk(pairs: &Pairs, ids: &[String], k: usize, out: &str) -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("retrieval-eval-dump-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("building library ({} docs) ...", pairs.answers.len());
    let lib = build_library(&dir, &pairs.answers);
    let rows: Vec<serde_json::Value> = pairs
        .questions
        .iter()
        .enumerate()
        .map(|(qi, q)| {
            let qemb = ese::encode_single(q);
            let hits: Vec<serde_json::Value> = lib
                .rtx(|r| {
                    search(
                        &r,
                        q,
                        Some(&qemb),
                        k,
                        None,
                        false,
                        false,
                        false,
                        |key| lib.get(key),
                        None,
                    )
                })
                .into_iter()
                .map(|h| {
                    let d = doc_index(&h.key);
                    serde_json::json!({"doc": d, "id": ids[d], "score": h.score})
                })
                .collect();
            serde_json::json!({"query": q, "gold": qi, "gold_id": ids[qi], "hits": hits})
        })
        .collect();
    std::fs::write(
        out,
        serde_json::to_string(&serde_json::json!({"k": k, "rows": rows}))?,
    )?;
    eprintln!("wrote {out}");
    drop(lib);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Explainer/debug trace over a gold-set library. `which` is `"ranks"` for
/// a per-query gold-rank table across lex/sem/hybrid, or a query index for
/// that query's detailed top-5 lists. `ids` names each answer index.
pub fn trace(pairs: &Pairs, ids: &[String], which: &str) {
    let dir = std::env::temp_dir().join(format!("retrieval-eval-trace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("building library ({} docs) ...", pairs.answers.len());
    let lib = build_library(&dir, &pairs.answers);
    let rank_of = |ranked: &[usize], gold: usize| ranked.iter().position(|&d| d == gold);
    let run_cfg = |q: &str, qemb: &Emb, sem: bool, lex: bool| -> Vec<usize> {
        if !lex {
            lib.rtx(|((_, vec), _)| vec.search(qemb))
                .into_iter()
                .map(|s| doc_index(&s.val))
                .collect()
        } else {
            lib.rtx(|r| {
                search(
                    &r,
                    q,
                    sem.then_some(qemb),
                    20,
                    None,
                    false,
                    false,
                    false,
                    |key| lib.get(key),
                    None,
                )
            })
            .into_iter()
            .map(|h| doc_index(&h.key))
            .collect()
        }
    };
    let fmt_rank = |r: Option<usize>| match r {
        Some(x) => format!("{:>4}", x + 1),
        None => "   -".to_string(),
    };
    if which == "ranks" {
        println!(" qi |  lex |  sem |  hyb | gold page / query");
        for (qi, q) in pairs.questions.iter().enumerate() {
            let qemb = ese::encode_single(q);
            let lex = rank_of(&run_cfg(q, &qemb, false, true), qi);
            let sem = rank_of(&run_cfg(q, &qemb, true, false), qi);
            let hyb = rank_of(&run_cfg(q, &qemb, true, true), qi);
            println!(
                "{qi:>3} | {} | {} | {} | {}  {q}",
                fmt_rank(lex),
                fmt_rank(sem),
                fmt_rank(hyb),
                ids[qi]
            );
        }
    } else if let Ok(qi) = which.parse::<usize>() {
        let q = &pairs.questions[qi];
        let qemb = ese::encode_single(q);
        println!("query [{qi}]: {q}\ngold: {}", ids[qi]);
        for (name, sem, lex) in [
            ("lex-only", false, true),
            ("sem-only", true, false),
            ("hybrid", true, true),
        ] {
            let ranked = run_cfg(q, &qemb, sem, lex);
            let where_gold = match rank_of(&ranked, qi) {
                Some(x) => format!("#{}", x + 1),
                None => "miss (not in list)".to_string(),
            };
            println!("\n{name} — gold at {where_gold}:");
            for (r, &d) in ranked.iter().take(5).enumerate() {
                let mark = if d == qi { "  ← gold" } else { "" };
                let excerpt: String = pairs.answers[d]
                    .chars()
                    .take(90)
                    .collect::<String>()
                    .replace('\n', " ");
                println!("  #{:<2} {:<44} {excerpt}{mark}", r + 1, ids[d]);
            }
        }
    } else {
        eprintln!("--trace wants 'ranks' or a query index");
    }
    drop(lib);
    let _ = std::fs::remove_dir_all(&dir);
}

pub fn run(pairs: &Pairs, ks: &[usize]) -> Vec<(String, EvalResult)> {
    let dir = std::env::temp_dir().join(format!("retrieval-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!("building library ({} docs) ...", pairs.answers.len());
    let lib = build_library(&dir, &pairs.answers);

    let qembs: Vec<Emb> = pairs.questions.iter().map(ese::encode_single).collect();
    let k = ks.iter().copied().max().unwrap_or(20);

    let mut out = Vec::new();
    for cfg in CONFIGS {
        let start = Instant::now();
        let per_query: Vec<Vec<f64>> = pairs
            .questions
            .par_iter()
            .zip(&qembs)
            .enumerate()
            .map(|(qi, (q, qemb))| {
                let ranked: Vec<usize> = if !cfg.lex {
                    // rank::search bails on an empty token stream, so the
                    // semantic-only list comes straight off the HNSW reader
                    // (≤ its compile-time K=40 cap — that's the point).
                    lib.rtx(|((_, vec), _)| vec.search(qemb))
                        .into_iter()
                        .map(|s| doc_index(&s.val))
                        .collect()
                } else {
                    lib.rtx(|r| {
                        search(
                            &r,
                            q,
                            cfg.sem.then_some(qemb),
                            k,
                            None,
                            false,
                            false,
                            cfg.diversify,
                            |key| lib.get(key),
                            None,
                        )
                    })
                    .into_iter()
                    .map(|h| doc_index(&h.key))
                    .collect()
                };
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
        out.push((
            cfg.name.to_string(),
            EvalResult::from_rows(ks, per_query, start.elapsed().as_millis() as u64),
        ));
    }

    drop(lib);
    let _ = std::fs::remove_dir_all(&dir);
    out
}
