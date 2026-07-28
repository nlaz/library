//! Retrieval-quality eval harness — recall@k / MRR / NDCG for the search
//! stack. Two layers: `encoder` isolates embedding quality (exact cosine, no
//! ANN), `pipeline` measures the app's real ranker (BM25 + HNSW + fusion +
//! MMR) on a temp-dir library. `smoke` runs the offline paraphrase fixture
//! with asserted floors (exit 1 on regression).
//!
//! ```text
//! retrieval-eval encoder  [--encoder ese|potion:<hf-id>]
//!                         [--doc-encoder SPEC] [--query-encoder SPEC]
//!                         [--docs N] [--queries M] [--seed S1,S2,...]
//!                         [--label STR] [--out FILE]
//! retrieval-eval pipeline [--docs N] [--queries M] [--seed S1,S2,...]
//!                         [--label STR] [--out FILE]
//! retrieval-eval fusion   [--docs N] [--queries M] [--seed S1,S2,...]
//!                         [--label STR] [--out FILE]
//! retrieval-eval smoke
//! ```
//!
//! `--seed` takes a comma list; multiple seeds resample the corpus per seed
//! and merge the per-query rows (macro average over the union), with a
//! per-seed spread printout so deltas can be judged against seed noise.
//! GooAQ subcommands download ~500 MB of parquet into target/gooaq/ on
//! first run. Never touches data/.

mod data;
mod encoder_eval;
mod encoders;
mod fusion;
mod gold;
mod metrics;
mod pipeline_eval;
mod prf;
mod report;

use anyhow::{Context, Result, bail};
use report::RunMeta;

const KS: &[usize] = &[1, 5, 10, 20];
/// column of the per-query reciprocal rank in the metric rows
const MRR_COL: usize = KS.len();

struct Args {
    encoder: String,
    doc_encoder: Option<String>,
    query_encoder: Option<String>,
    docs: usize,
    queries: usize,
    seeds: Vec<u64>,
    label: String,
    out: Option<String>,
    gold: Option<String>,
    trace: Option<String>,
    dump: Option<String>,
}

fn parse_args(rest: &[String], default_docs: usize, default_queries: usize) -> Result<Args> {
    let mut args = Args {
        encoder: "ese".to_string(),
        doc_encoder: None,
        query_encoder: None,
        docs: default_docs,
        queries: default_queries,
        seeds: vec![42],
        label: String::new(),
        out: None,
        gold: None,
        trace: None,
        dump: None,
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .with_context(|| format!("{flag} needs a value"))
                .cloned()
        };
        match flag.as_str() {
            "--encoder" => args.encoder = val()?,
            "--doc-encoder" => args.doc_encoder = Some(val()?),
            "--query-encoder" => args.query_encoder = Some(val()?),
            "--docs" => args.docs = val()?.parse().context("--docs")?,
            "--queries" => args.queries = val()?.parse().context("--queries")?,
            "--seed" => {
                args.seeds = val()?
                    .split(',')
                    .map(|s| s.trim().parse().context("--seed"))
                    .collect::<Result<Vec<u64>>>()?;
                anyhow::ensure!(!args.seeds.is_empty(), "--seed needs at least one value");
            }
            "--label" => args.label = val()?,
            "--out" => args.out = Some(val()?),
            "--gold" => args.gold = Some(val()?),
            "--trace" => args.trace = Some(val()?),
            "--dump" => args.dump = Some(val()?),
            _ => bail!("unknown flag {flag:?}"),
        }
    }
    Ok(args)
}

/// One resampled corpus per seed from a single GooAQ load.
fn gooaq_per_seed(args: &Args) -> Result<Vec<data::Pairs>> {
    let all = data::load_gooaq()?;
    args.seeds
        .iter()
        .map(|&s| data::sample_pairs(&all, args.docs, args.queries, s))
        .collect()
}

fn encoder_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 10_000, 1_000)?;
    let doc_spec = args
        .doc_encoder
        .clone()
        .unwrap_or_else(|| args.encoder.clone());
    let query_spec = args
        .query_encoder
        .clone()
        .unwrap_or_else(|| args.encoder.clone());
    let doc_enc = encoders::make(&doc_spec)?;
    // don't load the same model twice for the symmetric case
    let query_enc = if query_spec == doc_spec {
        None
    } else {
        Some(encoders::make(&query_spec)?)
    };
    let query_ref: &dyn encoders::Encoder = query_enc.as_deref().unwrap_or(doc_enc.as_ref());
    let name = if query_enc.is_none() {
        doc_enc.name()
    } else {
        format!("docs={} queries={}", doc_enc.name(), query_ref.name())
    };

    let per_seed: Vec<Vec<(String, encoder_eval::EvalResult)>> = gooaq_per_seed(&args)?
        .iter()
        .map(|pairs| {
            vec![(
                name.clone(),
                encoder_eval::run(doc_enc.as_ref(), query_ref, pairs, KS),
            )]
        })
        .collect();
    let results = encoder_eval::merge_named(KS, &per_seed);
    report::print_table(&results);
    if args.seeds.len() > 1 {
        report::print_seed_spread("ndcg@10", &args.seeds, &per_seed);
    }
    finish(&args, "encoder", "gooaq", &name, &results)
}

fn pipeline_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 10_000, 1_000)?;
    let per_seed: Vec<Vec<(String, encoder_eval::EvalResult)>> = gooaq_per_seed(&args)?
        .iter()
        .map(|pairs| pipeline_eval::run(pairs, KS))
        .collect();
    let results = encoder_eval::merge_named(KS, &per_seed);
    report::print_table(&results);
    report::print_paired("hybrid", &results, MRR_COL);
    if args.seeds.len() > 1 {
        report::print_seed_spread("ndcg@10", &args.seeds, &per_seed);
    }
    finish(&args, "pipeline", "gooaq", "ese", &results)
}

fn finish(
    args: &Args,
    subcommand: &str,
    dataset: &str,
    encoder: &str,
    results: &[(String, encoder_eval::EvalResult)],
) -> Result<()> {
    if let Some(out) = &args.out {
        let meta = RunMeta {
            subcommand: subcommand.to_string(),
            dataset: dataset.to_string(),
            seeds: args.seeds.clone(),
            n_docs: args.docs,
            n_queries: args.queries,
            encoder: encoder.to_string(),
            label: args.label.clone(),
        };
        report::write_json(out, &meta, results)?;
    }
    Ok(())
}

/// Generate the synthetic in-domain gold set from real library pages
/// (read-only) via the librarian sidecar. `--docs` = corpus pages sampled,
/// `--queries` = pages that get a generated query. The output file is the
/// frozen artifact — see gold.rs.
fn gen_gold_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 2_000, 100)?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| "target/library-gold.json".to_string());
    let seed = *args.seeds.first().expect("seeds never empty");
    gold::generate(args.docs, args.queries, seed, std::path::Path::new(&out))
}

/// Eval the frozen in-domain gold set: encoder-level plus the full
/// pipeline configs, with paired stats. This is the only workload with the
/// real corpus's text distribution — trust it over GooAQ when they
/// disagree.
fn library_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 0, 0)?;
    let path = args
        .gold
        .clone()
        .unwrap_or_else(|| "target/library-gold.json".to_string());
    let (pairs, ids, kinds) = gold::load_gold_ids(std::path::Path::new(&path))?;
    if let Some(which) = &args.trace {
        pipeline_eval::trace(&pairs, &ids, which);
        return Ok(());
    }
    if let Some(dump) = &args.dump {
        // 100 deep: enough headroom to sweep rerank pool depths offline
        return pipeline_eval::dump_hybrid_topk(&pairs, &ids, 100, dump);
    }
    println!(
        "### library gold ({} queries over {} pages)",
        pairs.questions.len(),
        pairs.answers.len()
    );
    let enc_result = encoder_eval::run(&encoders::Ese, &encoders::Ese, &pairs, KS);
    let mut results = vec![("encoder".to_string(), enc_result)];
    results.extend(pipeline_eval::run(&pairs, KS));
    report::print_table(&results);
    report::print_paired("hybrid", &results, MRR_COL);

    // per-workload breakdown (v2 gold sets tag each query's style) — the
    // blended average can hide a change that helps one workload and hurts
    // another; this makes the trade visible per config.
    let mut kind_names: Vec<String> = kinds.iter().flatten().cloned().collect();
    kind_names.sort();
    kind_names.dedup();
    for kind in &kind_names {
        let idx: Vec<usize> = kinds
            .iter()
            .enumerate()
            .filter(|(_, k)| k.as_deref() == Some(kind))
            .map(|(i, _)| i)
            .collect();
        println!("\n#### {kind} ({} queries)", idx.len());
        let sliced: Vec<(String, encoder_eval::EvalResult)> = results
            .iter()
            .map(|(name, r)| {
                let rows: Vec<Vec<f64>> = idx.iter().map(|&i| r.per_query[i].clone()).collect();
                (
                    name.clone(),
                    encoder_eval::EvalResult::from_rows(KS, rows, 0),
                )
            })
            .collect();
        report::print_table(&sliced);
    }
    if let Some(out) = &args.out {
        let meta = RunMeta {
            subcommand: "library".to_string(),
            dataset: path.clone(),
            seeds: args.seeds.clone(),
            n_docs: pairs.answers.len(),
            n_queries: pairs.questions.len(),
            encoder: "ese".to_string(),
            label: args.label.clone(),
        };
        report::write_json(out, &meta, &results)?;
    }
    Ok(())
}

/// Offline PRF experiments over the gold set: round-2 lexical search with
/// terms borrowed from round-1's top hits, alone and fused.
fn prf_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 0, 0)?;
    let path = args
        .gold
        .clone()
        .unwrap_or_else(|| "target/library-gold.json".to_string());
    let (pairs, _, _) = gold::load_gold_ids(std::path::Path::new(&path))?;
    println!(
        "### prf on library gold ({} queries over {} pages)",
        pairs.questions.len(),
        pairs.answers.len()
    );
    let results = prf::run(&pairs, KS);
    report::print_table(&results);
    report::print_paired("hybrid (round1, ships)", &results, MRR_COL);
    Ok(())
}

/// Dump ese's embeddings for a gold set (docs and queries) as JSON, so
/// offline experiments (alignment training, pooling prototypes, cross-model
/// comparisons in Python) start from the exact vectors the app produces —
/// quantization and all — rather than a reimplementation.
fn embed_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 0, 0)?;
    let path = args
        .gold
        .clone()
        .unwrap_or_else(|| "target/library-gold.json".to_string());
    let (pairs, ids, _) = gold::load_gold_ids(std::path::Path::new(&path))?;
    let docs = ese::encode(&pairs.answers);
    let queries = ese::encode(&pairs.questions);
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| "target/library-gold-ese.json".to_string());
    let doc = serde_json::json!({
        "dim": ese::DIMENSIONS,
        "gold": path,
        "ids": ids,
        "docs": docs.iter().map(|v| v.to_vec()).collect::<Vec<_>>(),
        "queries": queries.iter().map(|v| v.to_vec()).collect::<Vec<_>>(),
    });
    std::fs::write(&out, serde_json::to_string(&doc)?)?;
    eprintln!(
        "wrote {out} ({} docs, {} queries, dim {})",
        docs.len(),
        queries.len(),
        ese::DIMENSIONS
    );
    Ok(())
}

/// The shipping fusion — the paired-stats baseline in the fusion sweep.
const SHIPPING_FUSION: &str = "score-blend α=0.5 (ships today)";

/// Offline fusion sweep: candidate fusion functions scored on the raw
/// ranker lists, per fixture workload and (if sampled before or network is
/// available) GooAQ.
fn fusion_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest, 10_000, 1_000)?;
    let gooaq_seeds = gooaq_per_seed(&args)?;
    // known-item at scale: for each gold answer, the query is its three
    // longest distinct words — a proxy for "remembered distinctive phrase".
    // Guards against a fusion that trades lexical precision away.
    let known_at_scale = |gooaq: &data::Pairs| data::Pairs {
        questions: gooaq
            .answers
            .iter()
            .take(gooaq.questions.len())
            .map(|a| {
                let mut words: Vec<&str> = a
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() > 3)
                    .collect();
                words.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
                words.dedup();
                words.truncate(3);
                words.join(" ")
            })
            .collect(),
        answers: gooaq.answers.clone(),
    };
    let known_seeds: Vec<data::Pairs> = gooaq_seeds.iter().map(&known_at_scale).collect();
    let workloads: Vec<(&str, Vec<data::Pairs>)> = vec![
        // the fixture has no sampling — one run regardless of seed count
        (
            "fixture/paraphrase",
            vec![data::load_fixture(Some("paraphrase"))?],
        ),
        (
            "fixture/known-item",
            vec![data::load_fixture(Some("known-item"))?],
        ),
        ("gooaq", gooaq_seeds),
        ("gooaq/known-item", known_seeds),
    ];
    for (label, seed_pairs) in workloads {
        let n_queries: usize = seed_pairs.iter().map(|p| p.questions.len()).sum();
        println!("\n### {label} ({n_queries} queries)");
        let per_seed: Vec<Vec<(String, encoder_eval::EvalResult)>> = seed_pairs
            .iter()
            .map(|pairs| fusion::run(pairs, KS))
            .collect();
        let results = encoder_eval::merge_named(KS, &per_seed);
        report::print_table(&results);
        report::print_paired(SHIPPING_FUSION, &results, MRR_COL);
        if per_seed.len() > 1 {
            report::print_seed_spread("ndcg@10", &args.seeds, &per_seed);
        }
        if let Some(out) = &args.out {
            let meta = RunMeta {
                subcommand: "fusion".to_string(),
                dataset: label.to_string(),
                seeds: args.seeds.clone(),
                n_docs: seed_pairs.first().map(|p| p.answers.len()).unwrap_or(0),
                n_queries,
                encoder: "ese".to_string(),
                label: args.label.clone(),
            };
            report::write_json(
                &format!("{out}.{}.json", label.replace('/', "-")),
                &meta,
                &results,
            )?;
        }
    }
    Ok(())
}

/// Offline regression check on the fixture, per query workload. Floors sit
/// ~5 points under the values observed at introduction — they catch
/// collapses, not noise. Rebaseline deliberately when the encoder or
/// ranker changes.
fn smoke_cmd() -> Result<()> {
    let mut all_results: Vec<(String, encoder_eval::EvalResult)> = Vec::new();
    for (label, kind) in [
        ("paraphrase", Some("paraphrase")),
        ("known-item", Some("known-item")),
        ("mixed", None),
    ] {
        let pairs = data::load_fixture(kind)?;
        println!("\n### {label} ({} queries)", pairs.questions.len());
        let enc_result = encoder_eval::run(&encoders::Ese, &encoders::Ese, &pairs, KS);
        let mut results = vec![(format!("{label}/encoder"), enc_result)];
        results.extend(
            pipeline_eval::run(&pairs, KS)
                .into_iter()
                .map(|(name, r)| (format!("{label}/{name}"), r)),
        );
        report::print_table(&results);
        all_results.extend(results);
    }

    let floor = |config: &str, metric: &str, min: f64| -> bool {
        let got = all_results
            .iter()
            .find(|(name, _)| name == config)
            .and_then(|(_, r)| r.metrics.get(metric))
            .copied()
            .unwrap_or(0.0);
        let ok = got >= min;
        println!(
            "{} {config} {metric} = {got:.4} (floor {min:.2})",
            if ok { "PASS" } else { "FAIL" }
        );
        ok
    };

    // baselined 2026-07 after the score-blend fusion landed: paraphrase
    // encoder recall@5 0.92 / ndcg 0.84; paraphrase hybrid recall@5 0.84;
    // known-item lex-only and hybrid 1.00; mixed hybrid recall@5 0.92 /
    // ndcg 0.87. Re-observed 2026-07-28 after SIF weights were baked into
    // ese: everything at or above those values (paraphrase hybrid 0.88,
    // mixed hybrid 0.94/0.88) — floors deliberately left unchanged.
    println!();
    let mut ok = true;
    ok &= floor("paraphrase/encoder", "recall@05", 0.85);
    ok &= floor("paraphrase/encoder", "ndcg@10", 0.78);
    ok &= floor("paraphrase/hybrid", "recall@05", 0.78);
    ok &= floor("known-item/lex-only", "recall@05", 0.92);
    ok &= floor("known-item/hybrid", "recall@05", 0.92);
    ok &= floor("mixed/hybrid", "recall@05", 0.85);
    ok &= floor("mixed/hybrid", "ndcg@10", 0.82);
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("encoder") => encoder_cmd(&args[2..]),
        Some("pipeline") => pipeline_cmd(&args[2..]),
        Some("fusion") => fusion_cmd(&args[2..]),
        Some("gen-gold") => gen_gold_cmd(&args[2..]),
        Some("library") => library_cmd(&args[2..]),
        Some("embed") => embed_cmd(&args[2..]),
        Some("prf") => prf_cmd(&args[2..]),
        Some("smoke") => smoke_cmd(),
        _ => {
            eprintln!(
                "usage: retrieval-eval encoder [--encoder ese|potion:<hf-id>] \
                 [--doc-encoder SPEC] [--query-encoder SPEC] [--docs N] \
                 [--queries M] [--seed S1,S2,...] [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval pipeline [--docs N] [--queries M] [--seed S1,S2,...] \
                 [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval fusion [--docs N] [--queries M] [--seed S1,S2,...] \
                 [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval gen-gold [--docs PAGES] [--queries M] [--seed S] \
                 [--out FILE]\n\
                 \x20      retrieval-eval library [--gold FILE] [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval smoke"
            );
            std::process::exit(1);
        }
    }
}
