//! Research-spike eval harness for the librarian chat agent.
//!
//!   librarian-eval retrieval [out.json]   query battery against /api/search
//!   librarian-eval probe [filter]         capability probes via the sidecar
//!
//! Needs library-server on :8080; probes also need the built sidecar
//! (apps/librarian/.build/release/librarian) and Apple Intelligence.
//! Results print as markdown (for the findings doc) and dump as JSON.

use anyhow::{Context, Result};
use serde_json::{Value, json};

const BASE: &str = "http://127.0.0.1:8080";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("retrieval") => retrieval(args.get(2).map(String::as_str)),
        Some("probe") => probe(args.get(2).map(String::as_str)),
        Some("regress") => regress(),
        Some("e2e") => e2e(),
        _ => {
            eprintln!(
                "usage: librarian-eval retrieval [out.json] | probe [id-filter] | regress | e2e"
            );
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// regress: hard assertions on the agent-tool behaviors the chat experience
// depends on (see docs/spikes/chat-spike.md). Exit 1 on any failure.
// ---------------------------------------------------------------------------

fn regress() -> Result<()> {
    let mut failed = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "PASS" } else { "FAIL" }, if ok { String::new() } else { format!(" — {detail}") });
        if !ok {
            failed += 1;
        }
    };

    let search = |q: &str| -> Result<Value> {
        Ok(ureq::get(&format!("{BASE}/api/search?q={}", urlenc(q))).call()?.into_json()?)
    };

    // known-item: strong confidence, page text rides along
    let r = search("geodesic dome")?;
    check("known-item is strong", r["confidence"] == "strong", r["confidence"].to_string());
    check("strong carries top_hit_page", r["top_hit_page"]["text"].is_string(), "missing".into());

    // prefix expansion must stay off for agent queries
    let r = search("Broderbund")?;
    let top = r["hits"][0]["doc"].as_str().unwrap_or("");
    check(
        "no prefix junk (Broderbund != broth)",
        top.contains("software-people"),
        format!("top hit {top:?}"),
    );

    // off-intent partial matches must not read as covered
    let r = search("quantum entanglement")?;
    check("partial match is not strong", r["confidence"] != "strong", r["confidence"].to_string());
    check("weak carries a warning note", r["note"].is_string(), "missing".into());

    // true miss: none, no page text
    let r = search("cryptocurrency blockchain")?;
    check("miss is none", r["confidence"] == "none", r["confidence"].to_string());
    check("none carries no page text", r["top_hit_page"].is_null(), "present".into());

    // blank scan pages are loud errors, never empty content
    let r: Value = ureq::get(&format!("{BASE}/api/text/software-people?from=12&to=12"))
        .call()?
        .into_json()?;
    check(
        "blank page is an explicit error",
        r["error"].as_str().is_some_and(|e| e.contains("blank")),
        r.to_string(),
    );

    // fuzzy doc ids still resolve
    let r: Value = ureq::get(&format!("{BASE}/api/text/software-people?from=13&to=13"))
        .call()?
        .into_json()?;
    check(
        "fuzzy id resolves to real text",
        r["text"].as_str().is_some_and(|t| t.contains("Br")),
        r.to_string(),
    );

    if failed > 0 {
        eprintln!("{failed} regression(s) failed");
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// e2e: canned questions through POST /api/chat, asserting the event shape
// (tool activity, then a grounded done) against the live sidecar.
// ---------------------------------------------------------------------------

fn e2e() -> Result<()> {
    use std::io::{BufRead, BufReader};
    let questions = [
        "Is there a tandoori chicken recipe in the library?",
        "What does the Whole Earth Catalog say about geodesic domes?",
        "Do I have anything about quantum entanglement?",
    ];
    let mut failed = 0;
    for (i, q) in questions.iter().enumerate() {
        let body = json!({
            "conv": format!("e2e-{i}"),
            "messages": [{ "role": "user", "content": q }],
        });
        let resp = ureq::post(&format!("{BASE}/api/chat"))
            .set("content-type", "application/json")
            .send_string(&body.to_string())?;
        let reader = BufReader::new(resp.into_reader());
        let (mut tools, mut done, mut errors) = (0, None::<Value>, 0);
        for line in reader.lines() {
            let line = line?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
            match ev["e"].as_str() {
                Some("tool") => tools += 1,
                Some("error") => errors += 1,
                Some("done") => done = Some(ev),
                _ => {}
            }
        }
        let content = done.as_ref().and_then(|d| d["content"].as_str()).unwrap_or("");
        let ok = tools > 0 && !content.is_empty() && errors == 0;
        println!(
            "{} e2e-{i} ({} tool events, {} errors): {:.100}",
            if ok { "PASS" } else { "FAIL" },
            tools,
            errors,
            content.replace('\n', " "),
        );
        if !ok {
            failed += 1;
        }
    }
    if failed > 0 {
        eprintln!("{failed} e2e failure(s)");
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// retrieval battery
// ---------------------------------------------------------------------------

/// (query, kind, note) — kind "" = default blend, "images" = figure search.
const QUERIES: &[(&str, &str, &str)] = &[
    // known-item lexical
    ("bechamel sauce", "", "known-item"),
    ("geodesic dome", "", "known-item"),
    ("tandoori chicken", "", "known-item"),
    ("clean code function arguments", "", "known-item"),
    ("Broderbund", "", "known-item proper noun"),
    ("risotto", "", "known-item"),
    // semantic paraphrase (little keyword overlap expected)
    ("how to keep bread from going stale", "", "semantic"),
    ("tools for living off the grid", "", "semantic"),
    ("why big software projects fail", "", "semantic"),
    ("cooking for a crowd on a budget", "", "semantic"),
    ("meals without meat", "", "semantic"),
    // ambiguity traps
    ("apple", "", "ambiguous fruit/computer"),
    ("shell", "", "ambiguous seafood/unix"),
    ("java", "", "ambiguous coffee/language"),
    ("menu", "", "ambiguous food/ui"),
    // cross-doc
    ("tomato sauce italian indian", "", "cross-doc"),
    ("self-sufficiency philosophy", "", "cross-doc"),
    // image search
    ("diagram of a dome", "images", "image"),
    ("photograph of bread", "images", "image"),
    ("circuit diagram", "images", "image"),
    ("map of the united states", "images", "image"),
    // expected miss — does the MIN_REL floor actually return empty?
    ("kubernetes deployment", "", "expected-miss"),
    ("quantum entanglement", "", "expected-miss"),
    ("cryptocurrency blockchain", "", "expected-miss"),
];

fn retrieval(out: Option<&str>) -> Result<()> {
    let mut dump = Vec::new();
    println!("| query | note | top hits (doc p.page rel) |");
    println!("|---|---|---|");
    for (q, kind, note) in QUERIES {
        let url = format!(
            "{BASE}/api/search?q={}&kind={}&k=6",
            urlenc(q),
            urlenc(kind)
        );
        let resp: Value = ureq::get(&url)
            .call()
            .context("GET /api/search (is library-server running?)")?
            .into_json()?;
        let hits = resp["hits"].as_array().cloned().unwrap_or_default();
        let cells: Vec<String> = hits
            .iter()
            .take(5)
            .map(|h| {
                format!(
                    "{} p.{} {}{:.2}",
                    h["doc"].as_str().unwrap_or("?"),
                    h["page"],
                    if h["kind"] == "image" { "img " } else { "" },
                    h["rel"].as_f64().unwrap_or(0.0),
                )
            })
            .collect();
        println!(
            "| {q} | {note} | {} |",
            if cells.is_empty() { "(none)".into() } else { cells.join(" · ") }
        );
        dump.push(json!({ "query": q, "kind": kind, "note": note, "response": resp }));
    }
    if let Some(path) = out {
        std::fs::write(path, serde_json::to_vec_pretty(&dump)?)?;
        eprintln!("full hit dump: {path}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// capability probes
// ---------------------------------------------------------------------------

fn sidecar() -> String {
    std::env::var("LIBRARIAN_BIN")
        .unwrap_or_else(|_| "apps/librarian/.build/release/librarian".into())
}

fn fixtures_dir() -> std::path::PathBuf {
    // works from the repo root or the crate dir
    for p in ["examples/librarian-eval/fixtures", "fixtures"] {
        let p = std::path::Path::new(p);
        if p.is_dir() {
            return p.to_owned();
        }
    }
    panic!("fixtures directory not found; run from the repo root");
}

fn probe(filter: Option<&str>) -> Result<()> {
    let dir = fixtures_dir();
    let mut files: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();

    let mut results = Vec::new();
    println!("| probe | ok | ms | tools | verdict material |");
    println!("|---|---|---|---|---|");
    for path in files {
        let id = path.file_stem().unwrap().to_string_lossy().to_string();
        if let Some(f) = filter {
            if !id.contains(f) {
                continue;
            }
        }
        let mut fx: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        resolve_context(&mut fx)?;

        // sidecar reads a fixture file; write the resolved one beside nothing
        let tmp = std::env::temp_dir().join(format!("librarian-eval-{id}.json"));
        std::fs::write(&tmp, serde_json::to_vec(&fx)?)?;

        let out = std::process::Command::new(sidecar())
            .arg("probe")
            .arg(&tmp)
            .output()
            .context("spawning librarian sidecar")?;
        // last NDJSON line with e=result carries the verdict
        let stdout = String::from_utf8_lossy(&out.stdout);
        let result: Value = stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["e"] == "result")
            .next_back()
            .unwrap_or(json!({"ok": false, "error": "no result line", "raw": stdout.trim()}));

        let ok = result["ok"].as_bool().unwrap_or(false);
        let tools: Vec<String> = result["tool_calls"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| c["name"].as_str().unwrap_or("?").to_string())
                    .collect()
            })
            .unwrap_or_default();
        let head: String = result["content"]
            .as_str()
            .or(result["error"].as_str())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect::<String>()
            .replace('\n', " ");
        println!(
            "| {id} | {} | {} | {} | {head} |",
            if ok { "yes" } else { "NO" },
            result["ms"],
            if tools.is_empty() { "-".into() } else { tools.join(",") },
        );
        results.push(json!({ "id": id, "fixture": fx, "result": result }));
    }
    let out = "probe-results.json";
    std::fs::write(out, serde_json::to_vec_pretty(&results)?)?;
    eprintln!("full transcripts: {out}");
    Ok(())
}

/// Fixture `context`: [{doc, from, to}] — fetched from /api/text and spliced
/// into the prompt at {CONTEXT}, so generation probes are isolated from
/// retrieval.
fn resolve_context(fx: &mut Value) -> Result<()> {
    let Some(specs) = fx.get("context").and_then(|c| c.as_array()).cloned() else {
        return Ok(());
    };
    let mut ctx = String::new();
    for s in specs {
        let url = format!(
            "{BASE}/api/text/{}?from={}&to={}",
            urlenc(s["doc"].as_str().unwrap_or("")),
            s["from"],
            s["to"]
        );
        let resp: Value = ureq::get(&url).call()?.into_json()?;
        ctx.push_str(&format!(
            "\n--- {} pages {}-{} ---\n{}\n",
            resp["doc"].as_str().unwrap_or("?"),
            s["from"],
            s["to"],
            resp["text"].as_str().unwrap_or("(missing)")
        ));
    }
    let prompt = fx["prompt"].as_str().unwrap_or("").replace("{CONTEXT}", &ctx);
    fx["prompt"] = Value::String(prompt);
    fx.as_object_mut().unwrap().remove("context");
    Ok(())
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
