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
        println!(
            "{} {name}{}",
            if ok { "PASS" } else { "FAIL" },
            if ok {
                String::new()
            } else {
                format!(" — {detail}")
            }
        );
        if !ok {
            failed += 1;
        }
    };

    let search = |q: &str| -> Result<Value> {
        Ok(ureq::get(&format!("{BASE}/api/search?q={}", urlenc(q)))
            .call()?
            .into_json()?)
    };

    // known-item: strong confidence, page text rides along
    let r = search("geodesic dome")?;
    check(
        "known-item is strong",
        r["confidence"] == "strong",
        r["confidence"].to_string(),
    );
    check(
        "strong carries top_hit_page",
        r["top_hit_page"]["text"].is_string(),
        "missing".into(),
    );

    // prefix expansion must stay off for agent queries (the historical
    // junk mode: trailing-token expansion turned "micro" into "microscope")
    let r = search("micro")?;
    let microscope = regex::Regex::new(r"(?i)\bmicroscope\b")?;
    check(
        "no prefix junk (micro != microscope)",
        r["confidence"] == "strong"
            && r["hits"].as_array().is_some_and(|h| {
                !h.iter().any(|x| {
                    x["snippet"]
                        .as_str()
                        .is_some_and(|s| microscope.is_match(s))
                })
            }),
        format!("confidence {} hits {}", r["confidence"], r["hits"]),
    );

    // off-intent partial matches must not read as covered
    let r = search("quantum entanglement")?;
    check(
        "partial match is not strong",
        r["confidence"] != "strong",
        r["confidence"].to_string(),
    );
    check(
        "weak carries a warning note",
        r["note"].is_string(),
        "missing".into(),
    );

    // true miss: none, no page text
    let r = search("cryptocurrency blockchain")?;
    check(
        "miss is none",
        r["confidence"] == "none",
        r["confidence"].to_string(),
    );
    check(
        "none carries no page text",
        r["top_hit_page"].is_null(),
        "present".into(),
    );

    // blank scan pages are loud errors, never empty content
    let r: Value = ureq::get(&format!("{BASE}/api/text/journal-1891?from=4&to=4"))
        .call()?
        .into_json()?;
    check(
        "blank page is an explicit error",
        r["error"].as_str().is_some_and(|e| e.contains("blank")),
        r.to_string(),
    );

    // fuzzy doc ids still resolve
    let r: Value = ureq::get(&format!("{BASE}/api/text/Journal?from=3&to=3"))
        .call()?
        .into_json()?;
    check(
        "fuzzy id resolves to real text",
        r["doc"] == "journal-1891" && r["text"].as_str().is_some_and(|t| !t.is_empty()),
        r.to_string(),
    );
    check(
        "read_pages carries a title",
        r["title"].as_str().is_some_and(|t| !t.is_empty()),
        r["title"].to_string(),
    );

    // every model-facing hit carries a real title (never null, never a raw
    // scanned-archive id) and a collection tag
    let r = search("geodesic dome")?;
    let hits = r["hits"].as_array().cloned().unwrap_or_default();
    check("search returned hits", !hits.is_empty(), "none".into());
    check(
        "every hit has a non-id title",
        hits.iter().all(|h| {
            h["title"]
                .as_str()
                .is_some_and(|t| !t.is_empty() && !t.contains("00vol"))
        }),
        r["hits"].to_string(),
    );
    check(
        "hits carry a collection tag",
        hits.iter().any(|h| h["col"].is_string()),
        r["hits"].to_string(),
    );

    // collection scoping: exact, fuzzy, unknown-is-loud — and images too
    let cols: Value = ureq::get(&format!("{BASE}/api/collections"))
        .call()?
        .into_json()?;
    let in_col = |col: &str, doc: &str| {
        cols[col]
            .as_array()
            .is_some_and(|d| d.iter().any(|v| v == doc))
    };
    let scoped: Value = ureq::get(&format!("{BASE}/api/search?q=risotto&col=recipes"))
        .call()?
        .into_json()?;
    check(
        "col=recipes only returns recipes",
        scoped["hits"].as_array().is_some_and(|h| {
            !h.is_empty()
                && h.iter()
                    .all(|x| in_col("recipes", x["doc"].as_str().unwrap_or("?")))
        }),
        scoped["hits"].to_string(),
    );
    let fuzzy: Value = ureq::get(&format!("{BASE}/api/search?q=dome&col=Field%20Guides"))
        .call()?
        .into_json()?;
    check(
        "fuzzy col resolves (Field Guides -> field-guides)",
        fuzzy["hits"].as_array().is_some_and(|h| {
            !h.is_empty()
                && h.iter()
                    .all(|x| in_col("field-guides", x["doc"].as_str().unwrap_or("?")))
        }),
        fuzzy["hits"].to_string(),
    );
    let bogus: Value = ureq::get(&format!("{BASE}/api/search?q=dome&col=bogus"))
        .call()?
        .into_json()?;
    check(
        "unknown col is a loud error listing collections",
        bogus["error"].is_string() && bogus["collections"].is_array(),
        bogus.to_string(),
    );
    let img: Value = ureq::get(&format!(
        "{BASE}/api/search?q=dome&kind=images&col=field-guides"
    ))
    .call()?
    .into_json()?;
    check(
        "image search honors col",
        img["hits"].as_array().is_some_and(|h| {
            h.iter()
                .all(|x| in_col("field-guides", x["doc"].as_str().unwrap_or("?")))
        }),
        img["hits"].to_string(),
    );

    // sample_page: seeded draws are deterministic, scoped, and readable
    let s: Value = ureq::get(&format!("{BASE}/api/sample?col=field-guides&seed=42"))
        .call()?
        .into_json()?;
    let s2: Value = ureq::get(&format!("{BASE}/api/sample?col=field-guides&seed=42"))
        .call()?
        .into_json()?;
    check(
        "sample is seed-deterministic",
        s == s2,
        format!("{s} vs {s2}"),
    );
    check(
        "sample doc is in the collection",
        s["doc"].as_str().is_some_and(|d| in_col("field-guides", d)),
        s.to_string(),
    );
    check(
        "sample page is readable text with a title",
        s["text"].as_str().is_some_and(|t| t.len() >= 40)
            && s["title"].as_str().is_some_and(|t| !t.is_empty())
            && s["page"].as_u64() <= s["total_pages"].as_u64(),
        s.to_string(),
    );
    let sb: Value = ureq::get(&format!("{BASE}/api/sample?col=bogus"))
        .call()?
        .into_json()?;
    check(
        "sample unknown col errors",
        sb["error"].is_string(),
        sb.to_string(),
    );

    // quality gate: on the known-garbled shelf, sampled text is served as
    // legible excerpts — never silently garbled (the exact failure from
    // the live screenshot, where the model quoted column-interleaved salad)
    use library_core::legibility::{NOISY_MIN, legibility};
    let mut gate_ok = true;
    let mut gate_detail = String::new();
    for seed in 1..=10 {
        let p: Value = ureq::get(&format!("{BASE}/api/sample?col=field-guides&seed={seed}"))
            .call()?
            .into_json()?;
        let text = p["text"].as_str().unwrap_or("");
        if legibility(text) < NOISY_MIN {
            gate_ok = false;
            gate_detail = format!("seed {seed}: garbled sample {p}");
            break;
        }
    }
    check("sampled pages are legible excerpts", gate_ok, gate_detail);

    // avoid: re-sampling with the served page excluded must move on
    let served = format!(
        "{}:{}",
        s["doc"].as_str().unwrap_or("?"),
        s["page"].as_u64().unwrap_or(0)
    );
    let moved: Value = ureq::get(&format!(
        "{BASE}/api/sample?col=field-guides&seed=42&avoid={served}"
    ))
    .call()?
    .into_json()?;
    check(
        "avoid excludes the served page",
        moved["error"].is_null()
            && format!(
                "{}:{}",
                moved["doc"].as_str().unwrap_or("?"),
                moved["page"].as_u64().unwrap_or(0)
            ) != served,
        format!("served {served}, got {moved}"),
    );

    // copy dedup: the multiple catalog copies must not crowd the hit list —
    // never the same (base, page) twice, at most 2 hits per copy family
    let r: Value = ureq::get(&format!(
        "{BASE}/api/search?q={}&col=field-guides",
        urlenc("access to tools")
    ))
    .call()?
    .into_json()?;
    let base = |d: &str| -> String {
        match d.rfind('-') {
            Some(i) if d[i + 1..].chars().all(|c| c.is_ascii_digit()) && !d[i + 1..].is_empty() => {
                d[..i].to_owned()
            }
            _ => d.to_owned(),
        }
    };
    let hit_keys: Vec<(String, u64)> = r["hits"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|h| {
            (
                base(h["doc"].as_str().unwrap_or("?")),
                h["page"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    let mut counts = std::collections::BTreeMap::new();
    for (b, _) in &hit_keys {
        *counts.entry(b.clone()).or_insert(0usize) += 1;
    }
    let mut pairs = hit_keys.clone();
    pairs.sort();
    pairs.dedup();
    check(
        "search dedups copy variants",
        !hit_keys.is_empty() && pairs.len() == hit_keys.len() && counts.values().all(|&n| n <= 2),
        r["hits"].to_string(),
    );

    // whitespace: no raw newlines in any model-facing page text (the 3B
    // model parrots them back as literal \n escapes)
    let dome = search("geodesic dome")?;
    let journal: Value = ureq::get(&format!("{BASE}/api/text/journal-1891?from=3&to=3"))
        .call()?
        .into_json()?;
    check(
        "model-facing text carries no raw newlines",
        !dome["top_hit_page"]["text"]
            .as_str()
            .unwrap_or("")
            .contains('\n')
            && !s["text"].as_str().unwrap_or("").contains('\n')
            && !journal["text"].as_str().unwrap_or("").contains('\n'),
        "newline found in top_hit_page/sample/read_pages text".into(),
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

/// One POST /api/chat turn: (tool event count, error count, done content).
fn chat_turn(conv: &str, messages: &Value) -> Result<(usize, usize, String)> {
    use std::io::{BufRead, BufReader};
    let body = json!({ "conv": conv, "messages": messages });
    let resp = ureq::post(&format!("{BASE}/api/chat"))
        .set("content-type", "application/json")
        .send_string(&body.to_string())?;
    let reader = BufReader::new(resp.into_reader());
    let (mut tools, mut done, mut errors) = (0, None::<Value>, 0);
    for line in reader.lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match ev["e"].as_str() {
            Some("tool") => tools += 1,
            Some("error") => errors += 1,
            Some("done") => done = Some(ev),
            _ => {}
        }
    }
    let content = done
        .as_ref()
        .and_then(|d| d["content"].as_str())
        .unwrap_or("")
        .to_owned();
    Ok((tools, errors, content))
}

fn e2e() -> Result<()> {
    let questions = [
        "Is there a tandoori chicken recipe in the library?",
        "What do the field guides say about geodesic domes?",
        "Do I have anything about quantum entanglement?",
    ];
    let mut failed = 0;
    for (i, q) in questions.iter().enumerate() {
        let messages = json!([{ "role": "user", "content": q }]);
        let (tools, errors, content) = chat_turn(&format!("e2e-{i}"), &messages)?;
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

    // the screenshot conversation, mechanized: a vague follow-up on one conv
    // must produce a tool-backed, non-refusal, title-cited answer (this also
    // exercises serve-mode session persistence)
    let refusal = regex::Regex::new(r"(?i)can.?t (assist|help)|unable to (assist|help)")?;
    let raw_ids = regex::Regex::new(r"00vol|Document ID")?;
    let t1 = "tell me an interesting fact from the field guides";
    let t2 = "no i want another interesting fact";
    let (_, _, first) = chat_turn("e2e-mt", &json!([{ "role": "user", "content": t1 }]))?;
    let messages = json!([
        { "role": "user", "content": t1 },
        { "role": "assistant", "content": first },
        { "role": "user", "content": t2 },
    ]);
    let (tools, errors, content) = chat_turn("e2e-mt", &messages)?;
    let ok = tools > 0
        && errors == 0
        && !content.is_empty()
        && !refusal.is_match(&content)
        && !raw_ids.is_match(&content);
    println!(
        "{} e2e-mt follow-up ({} tool events, {} errors): {:.100}",
        if ok { "PASS" } else { "FAIL" },
        tools,
        errors,
        content.replace('\n', " "),
    );
    if !ok {
        failed += 1;
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
    ("sourdough starter maintenance", "", "known-item"),
    ("Prairie Press", "", "known-item proper noun"),
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
            if cells.is_empty() {
                "(none)".into()
            } else {
                cells.join(" · ")
            }
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
    for p in ["tools/librarian-eval/fixtures", "fixtures"] {
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
    let mut failed = 0;
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

        let ran = result["ok"].as_bool().unwrap_or(false);
        let expects = check_expects(&fx, &result);
        let ok = ran && expects.iter().all(|(_, pass)| *pass);
        if !ok {
            failed += 1;
        }
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
            if tools.is_empty() {
                "-".into()
            } else {
                tools.join(",")
            },
        );
        for (what, pass) in &expects {
            if !pass {
                println!("|   ↳ expect FAILED | {what} | | | |");
            }
        }
        results.push(json!({ "id": id, "fixture": fx, "result": result,
                             "expect_failures": expects.iter().filter(|(_, p)| !p).map(|(w, _)| w).collect::<Vec<_>>() }));
    }
    let out = std::env::temp_dir().join("librarian-eval-probe-results.json");
    std::fs::write(&out, serde_json::to_vec_pretty(&results)?)?;
    eprintln!("full transcripts: {}", out.display());
    if failed > 0 {
        eprintln!("{failed} probe(s) failed");
        std::process::exit(1);
    }
    Ok(())
}

/// Evaluate a fixture's `expect` checks against the probe result — hard
/// verdicts instead of eyeballed "verdict material". Kinds:
///   {"kind":"tools_include","value":"sample_page"}
///   {"kind":"tools_order","value":["search_figures","read_pages"]}
///   {"kind":"tool_arg","tool":"search_library","arg":"collection","value":"recipes"}  (fuzzy)
///   {"kind":"content_regex","value":"p\\.\\s*\\d+"}      (last turn's content)
///   {"kind":"content_not_regex","value":"Document ID"}
///   {"kind":"all_content_not_regex","value":"\\\\n"}     (every turn's content)
fn check_expects(fx: &Value, result: &Value) -> Vec<(String, bool)> {
    let Some(expects) = fx.get("expect").and_then(|e| e.as_array()) else {
        return Vec::new();
    };
    let content = result["content"].as_str().unwrap_or("");
    let calls: Vec<&Value> = result["tool_calls"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    expects
        .iter()
        .map(|ex| {
            let kind = ex["kind"].as_str().unwrap_or("");
            let val = &ex["value"];
            let pass = match kind {
                "tools_include" => calls.iter().any(|c| c["name"] == *val),
                "tools_order" => {
                    let want: Vec<&str> = val
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    let pos = |name: &str| calls.iter().position(|c| c["name"] == name);
                    want.windows(2).all(|w| match (pos(w[0]), pos(w[1])) {
                        (Some(a), Some(b)) => a < b,
                        _ => false,
                    })
                }
                "tool_arg" => {
                    let tool = ex["tool"].as_str().unwrap_or("");
                    let arg = ex["arg"].as_str().unwrap_or("");
                    let want = norm(val.as_str().unwrap_or(""));
                    calls.iter().any(|c| {
                        c["name"] == tool
                            && c["args"][arg].as_str().is_some_and(|got| {
                                let got = norm(got);
                                !got.is_empty() && (got.contains(&want) || want.contains(&got))
                            })
                    })
                }
                "content_regex" | "content_not_regex" => {
                    let re =
                        regex::Regex::new(val.as_str().unwrap_or("$^")).expect("bad expect regex");
                    re.is_match(content) == (kind == "content_regex")
                }
                // multi-turn fixtures: the check must hold on EVERY turn,
                // not just the last (probe emits a `contents` array)
                "all_content_not_regex" => {
                    let re =
                        regex::Regex::new(val.as_str().unwrap_or("$^")).expect("bad expect regex");
                    result["contents"]
                        .as_array()
                        .map(|a| a.iter().all(|c| !re.is_match(c.as_str().unwrap_or(""))))
                        .unwrap_or_else(|| !re.is_match(content))
                }
                _ => false,
            };
            (format!("{kind} {val}"), pass)
        })
        .collect()
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
    let prompt = fx["prompt"]
        .as_str()
        .unwrap_or("")
        .replace("{CONTEXT}", &ctx);
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
