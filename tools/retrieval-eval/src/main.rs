//! Retrieval-quality eval harness — recall@k / MRR / NDCG for the search
//! stack. Two layers: `encoder` isolates embedding quality (exact cosine, no
//! ANN), `pipeline` measures the app's real ranker (BM25 + HNSW + RRF + MMR)
//! on a temp-dir library. `smoke` runs the offline paraphrase fixture with
//! asserted floors (exit 1 on regression).
//!
//! ```text
//! retrieval-eval encoder  [--encoder ese|potion:<hf-id>] [--docs N]
//!                         [--queries M] [--seed S] [--label STR] [--out FILE]
//! retrieval-eval pipeline [--docs N] [--queries M] [--seed S]
//!                         [--label STR] [--out FILE]
//! retrieval-eval smoke
//! ```
//!
//! GooAQ subcommands download ~500 MB of parquet into target/gooaq/ on
//! first run. Never touches data/.

mod data;
mod encoder_eval;
mod encoders;
mod fusion;
mod metrics;
mod pipeline_eval;
mod report;

use anyhow::{Context, Result, bail};
use report::RunMeta;

const KS: &[usize] = &[1, 5, 10, 20];

struct Args {
    encoder: String,
    docs: usize,
    queries: usize,
    seed: u64,
    label: String,
    out: Option<String>,
}

fn parse_args(rest: &[String]) -> Result<Args> {
    let mut args = Args {
        encoder: "ese".to_string(),
        docs: 10_000,
        queries: 1_000,
        seed: 42,
        label: String::new(),
        out: None,
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
            "--docs" => args.docs = val()?.parse().context("--docs")?,
            "--queries" => args.queries = val()?.parse().context("--queries")?,
            "--seed" => args.seed = val()?.parse().context("--seed")?,
            "--label" => args.label = val()?,
            "--out" => args.out = Some(val()?),
            _ => bail!("unknown flag {flag:?}"),
        }
    }
    Ok(args)
}

fn gooaq_pairs(args: &Args) -> Result<data::Pairs> {
    data::sample_pairs(data::load_gooaq()?, args.docs, args.queries, args.seed)
}

fn encoder_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest)?;
    let enc = encoders::make(&args.encoder)?;
    let pairs = gooaq_pairs(&args)?;
    let result = encoder_eval::run(enc.as_ref(), &pairs, KS);
    let results = vec![(enc.name(), result)];
    report::print_table(&results);
    finish(&args, "encoder", &enc.name(), &results)
}

fn pipeline_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest)?;
    let pairs = gooaq_pairs(&args)?;
    let results = pipeline_eval::run(&pairs, KS);
    report::print_table(&results);
    finish(&args, "pipeline", "ese", &results)
}

fn finish(
    args: &Args,
    subcommand: &str,
    encoder: &str,
    results: &[(String, encoder_eval::EvalResult)],
) -> Result<()> {
    if let Some(out) = &args.out {
        let meta = RunMeta {
            subcommand: subcommand.to_string(),
            dataset: "gooaq".to_string(),
            seed: args.seed,
            n_docs: args.docs,
            n_queries: args.queries,
            encoder: encoder.to_string(),
            label: args.label.clone(),
        };
        report::write_json(out, &meta, results)?;
    }
    Ok(())
}

/// Offline fusion sweep: candidate fusion functions scored on the raw
/// ranker lists, per fixture workload and (if sampled before or network is
/// available) GooAQ.
fn fusion_cmd(rest: &[String]) -> Result<()> {
    let args = parse_args(rest)?;
    let gooaq = gooaq_pairs(&args)?;
    // known-item at scale: for each gold answer, the query is its three
    // longest distinct words — a proxy for "remembered distinctive phrase".
    // Guards against a fusion that trades lexical precision away.
    let known_at_scale = data::Pairs {
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
    for (label, pairs) in [
        (
            "fixture/paraphrase",
            data::load_fixture(Some("paraphrase"))?,
        ),
        (
            "fixture/known-item",
            data::load_fixture(Some("known-item"))?,
        ),
        ("gooaq", gooaq),
        ("gooaq/known-item", known_at_scale),
    ] {
        println!("\n### {label} ({} queries)", pairs.questions.len());
        let results = fusion::run(&pairs, KS);
        report::print_table(&results);
        if let Some(out) = &args.out {
            let meta = RunMeta {
                subcommand: "fusion".to_string(),
                dataset: label.to_string(),
                seed: args.seed,
                n_docs: pairs.answers.len(),
                n_queries: pairs.questions.len(),
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
        let enc_result = encoder_eval::run(&encoders::Ese, &pairs, KS);
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
    // ndcg 0.87.
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
        Some("smoke") => smoke_cmd(),
        _ => {
            eprintln!(
                "usage: retrieval-eval encoder [--encoder ese|potion:<hf-id>] [--docs N] \
                 [--queries M] [--seed S] [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval pipeline [--docs N] [--queries M] [--seed S] \
                 [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval fusion [--docs N] [--queries M] [--seed S] \
                 [--label STR] [--out FILE]\n\
                 \x20      retrieval-eval smoke"
            );
            std::process::exit(1);
        }
    }
}
