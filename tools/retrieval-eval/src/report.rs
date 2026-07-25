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
    pub seed: u64,
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
            "seed": meta.seed,
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
