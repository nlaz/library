//! Output: a markdown table on stdout for eyeballs, and a labeled JSON
//! dump for diffing runs across builds (compile-time ese variants live in
//! separate binaries — the meta block is what makes their outputs
//! comparable).

use crate::encoder_eval::EvalResult;
use anyhow::Result;
use serde_json::json;
use std::process::Command;

pub struct RunMeta {
    pub subcommand: String,
    pub dataset: String,
    pub seeds: Vec<u64>,
    pub n_docs: usize,
    pub n_queries: usize,
    pub encoder: String,
    pub label: String,
}

fn git_rev() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn timestamp() -> String {
    // date -u keeps us out of chrono for one field
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn print_table(results: &[(String, EvalResult)]) {
    let Some((_, first)) = results.first() else {
        return;
    };
    let cols: Vec<&String> = first.metrics.keys().collect();
    print!("| config |");
    for c in &cols {
        print!(" {c} |");
    }
    println!(" wall_ms |");
    print!("|---|");
    for _ in &cols {
        print!("---|");
    }
    println!("---|");
    for (name, r) in results {
        print!("| {name} |");
        for c in &cols {
            print!(" {:.4} |", r.metrics[*c]);
        }
        println!(" {} |", r.wall_ms);
    }
}

/// Per-seed values of one metric, config × seed, plus the population sd —
/// the eyeball answer to "is this delta bigger than seed noise?".
pub fn print_seed_spread(metric: &str, seeds: &[u64], per_seed: &[Vec<(String, EvalResult)>]) {
    let Some(first) = per_seed.first() else {
        return;
    };
    println!("\nper-seed {metric} (seeds {seeds:?}):");
    for (ci, (name, _)) in first.iter().enumerate() {
        let vals: Vec<f64> = per_seed
            .iter()
            .filter_map(|s| s.get(ci).and_then(|(_, r)| r.metrics.get(metric)))
            .copied()
            .collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let sd = (vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n).sqrt();
        let listed: Vec<String> = vals.iter().map(|v| format!("{v:.4}")).collect();
        println!("  {name}: {} (sd {sd:.4})", listed.join(" "));
    }
}

/// Paired per-query comparison against a baseline config on reciprocal
/// rank (the mrr@10 column): W = ranks the gold strictly higher than the
/// baseline on that query, L = lower, T = tie. All configs saw the same
/// queries over the same corpus, so this is far more sensitive than
/// comparing two macro averages — a 60W/10L split is a real difference
/// even when the averages sit a fraction of a point apart.
pub fn print_paired(baseline: &str, results: &[(String, EvalResult)], mrr_col: usize) {
    let Some((_, base)) = results.iter().find(|(name, _)| name == baseline) else {
        return;
    };
    println!("\npaired per-query reciprocal-rank vs {baseline}:");
    for (name, r) in results {
        if name == baseline || r.per_query.len() != base.per_query.len() {
            continue;
        }
        let (mut w, mut l, mut t) = (0u32, 0u32, 0u32);
        for (a, b) in r.per_query.iter().zip(&base.per_query) {
            if a[mrr_col] > b[mrr_col] {
                w += 1;
            } else if a[mrr_col] < b[mrr_col] {
                l += 1;
            } else {
                t += 1;
            }
        }
        println!("  {name}: {w}W / {l}L / {t}T");
    }
}

pub fn write_json(path: &str, meta: &RunMeta, results: &[(String, EvalResult)]) -> Result<()> {
    let results: Vec<_> = results
        .iter()
        .map(|(name, r)| {
            let mut obj = serde_json::Map::new();
            obj.insert("config".into(), json!(name));
            for (k, v) in &r.metrics {
                obj.insert(k.clone(), json!(v));
            }
            obj.insert("wall_ms".into(), json!(r.wall_ms));
            serde_json::Value::Object(obj)
        })
        .collect();
    let doc = json!({
        "meta": {
            "tool": "retrieval-eval",
            "subcommand": meta.subcommand,
            "git_rev": git_rev(),
            "timestamp": timestamp(),
            "dataset": meta.dataset,
            "seeds": meta.seeds,
            "n_docs": meta.n_docs,
            "n_queries": meta.n_queries,
            "encoder": meta.encoder,
            "emb_dim": ese::DIMENSIONS,
            "label": meta.label,
        },
        "results": results,
    });
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    eprintln!("wrote {path}");
    Ok(())
}
