//! Synthetic in-domain gold set: sample pages from the real library's
//! plain-text corpus (`data/text/*.md`, strictly read-only — no fjall store
//! is ever opened), have the librarian sidecar write one search query per
//! page (one-shot `probe`, toolless, schema-constrained), and freeze the
//! (query, page) pairs plus the sampled corpus into a JSON file.
//!
//! The frozen file is the artifact: generation is model-driven and not
//! reproducible, so regenerate deliberately, keep the file, and eval
//! against it with the `library` subcommand. This is the only workload
//! whose documents have the real corpus's text distribution — OCR noise,
//! book-length prose, domain vocabulary — where GooAQ has clean web
//! snippets.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub struct GoldDoc {
    /// "<doc-id>#p<page>" — the same (doc, page) coordinates search uses
    pub id: String,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub struct GoldQuery {
    pub q: String,
    pub gold: String,
}

#[derive(Serialize, Deserialize)]
pub struct GoldSet {
    pub docs: Vec<GoldDoc>,
    pub queries: Vec<GoldQuery>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn librarian_bin() -> PathBuf {
    std::env::var("LIBRARIAN_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("apps/librarian/.build/release/librarian"))
}

/// Split a `data/text/<doc>.md` file on its `<!-- page N -->` markers.
pub fn split_pages(md: &str) -> Vec<(u32, String)> {
    let mut pages: Vec<(u32, String)> = Vec::new();
    let mut cur: Option<(u32, String)> = None;
    for line in md.lines() {
        let marker = line
            .strip_prefix("<!-- page ")
            .and_then(|r| r.strip_suffix(" -->"))
            .and_then(|n| n.trim().parse().ok());
        if let Some(n) = marker {
            if let Some(p) = cur.take() {
                pages.push(p);
            }
            cur = Some((n, String::new()));
        } else if let Some((_, text)) = &mut cur {
            text.push_str(line);
            text.push('\n');
        }
    }
    if let Some(p) = cur.take() {
        pages.push(p);
    }
    pages
}

/// Every page of every doc under data/text/ with enough text to be a
/// sensible retrieval target, as ("<doc>#p<n>", text). Deterministic order.
fn list_pages(text_dir: &Path, min_chars: usize) -> Result<Vec<(String, String)>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(text_dir)
        .with_context(|| format!("read {}", text_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    let mut pages = Vec::new();
    for path in files {
        let doc = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("doc file stem")?
            .to_string();
        let md = std::fs::read_to_string(&path)?;
        for (n, text) in split_pages(&md) {
            let text = text.trim();
            if text.len() >= min_chars {
                pages.push((format!("{doc}#p{n}"), text.to_string()));
            }
        }
    }
    Ok(pages)
}

/// One `librarian probe` round-trip: page text in, one search query out.
/// Toolless and schema-constrained, so no server and no fjall lock; AFM
/// sessions must run sequentially (see atlas.rs).
fn gen_query(bin: &Path, tmp: &Path, id: &str, text: &str) -> Result<String> {
    // AFM's window is ~4k tokens; 2500 chars matches the app's own cap
    let excerpt: String = text.chars().take(2500).collect();
    let fixture = serde_json::json!({
        "id": id,
        "tools": false,
        "temperature": 0.4,
        "instructions": "You write the search query a reader would type into a \
            personal library's search box to find a specific page they remember. \
            Write natural queries in the reader's own words.",
        "prompt": format!(
            "Page text:\n{excerpt}\n\nWrite one search query (a question or a \
             short phrase) a reader would type to find this page. Describe the \
             page's subject in your own words — do not copy distinctive phrases \
             verbatim."
        ),
        "schema": {"name": "Query", "properties": [
            {"name": "query", "type": "string",
             "description": "one natural search query, under 15 words, no quotation marks"}]}
    });
    let path = tmp.join(format!("{}.json", id.replace(['/', '#'], "-")));
    std::fs::write(&path, serde_json::to_vec(&fixture)?)?;
    let out = std::process::Command::new(bin)
        .arg("probe")
        .arg(&path)
        .output()
        .with_context(|| format!("spawn {} probe", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["e"] == "result" {
            ensure!(
                v["ok"] == true,
                "librarian: {}",
                v["error"].as_str().unwrap_or("unknown error")
            );
            let content: serde_json::Value =
                serde_json::from_str(v["content"].as_str().context("result content")?)?;
            let q = content["query"]
                .as_str()
                .context("query field")?
                .trim()
                .to_string();
            ensure!(!q.is_empty(), "empty query");
            return Ok(q);
        }
    }
    bail!(
        "no result line from librarian probe (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Sample `corpus` pages, generate one query each for the first pages that
/// the model handles (guardrail refusals are skipped, the corpus keeps the
/// page as a distractor), stop at `n_queries`, freeze everything to `out`.
pub fn generate(corpus: usize, n_queries: usize, seed: u64, out: &Path) -> Result<()> {
    ensure!(n_queries <= corpus, "need n_queries <= corpus pages");
    let bin = librarian_bin();
    ensure!(
        bin.exists(),
        "librarian binary not found at {} — build it with:\n  swift build -c release --package-path apps/librarian",
        bin.display()
    );

    let text_dir = repo_root().join("data/text");
    let mut pages = list_pages(&text_dir, 300)?;
    eprintln!("{} usable pages under {}", pages.len(), text_dir.display());
    ensure!(
        pages.len() >= corpus,
        "only {} usable pages, need {corpus}",
        pages.len()
    );
    let mut s = seed ^ 0x243F6A8885A308D3;
    for i in (1..pages.len()).rev() {
        let j = (crate::data::splitmix64(&mut s) as usize) % (i + 1);
        pages.swap(i, j);
    }
    pages.truncate(corpus);

    let tmp = std::env::temp_dir().join(format!("retrieval-eval-gold-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let mut queries = Vec::new();
    for (id, text) in &pages {
        if queries.len() >= n_queries {
            break;
        }
        match gen_query(&bin, &tmp, id, text) {
            Ok(q) => {
                eprintln!("[{}/{n_queries}] {id}: {q}", queries.len() + 1);
                queries.push(GoldQuery {
                    q,
                    gold: id.clone(),
                });
            }
            Err(e) => eprintln!("skip {id}: {e}"),
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    ensure!(
        queries.len() == n_queries,
        "only generated {} of {n_queries} queries — not enough usable pages in the sample",
        queries.len()
    );

    let set = GoldSet {
        docs: pages
            .into_iter()
            .map(|(id, text)| GoldDoc { id, text })
            .collect(),
        queries,
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(out, serde_json::to_string_pretty(&set)?)?;
    eprintln!(
        "wrote {} ({} docs, {} queries)",
        out.display(),
        set.docs.len(),
        set.queries.len()
    );
    Ok(())
}

/// Parse a gold set into [`Pairs`] plus the page id for each answer index:
/// query i's gold page becomes answer i, remaining corpus pages follow as
/// distractors. Same alignment contract as the checked-in fixture.
pub fn parse_gold_ids(json: &str) -> Result<(crate::data::Pairs, Vec<String>)> {
    let set: GoldSet = serde_json::from_str(json).context("parse gold set json")?;
    let mut questions = Vec::new();
    let mut ordered = Vec::new();
    let mut ids = Vec::new();
    let mut taken = vec![false; set.docs.len()];
    for q in &set.queries {
        let Some(i) = set.docs.iter().position(|d| d.id == q.gold) else {
            bail!("gold id {:?} not in docs", q.gold);
        };
        ensure!(!taken[i], "doc {:?} is gold for two queries", q.gold);
        taken[i] = true;
        questions.push(q.q.clone());
        ordered.push(set.docs[i].text.clone());
        ids.push(set.docs[i].id.clone());
    }
    for (i, d) in set.docs.iter().enumerate() {
        if !taken[i] {
            ordered.push(d.text.clone());
            ids.push(d.id.clone());
        }
    }
    Ok((
        crate::data::Pairs {
            questions,
            answers: ordered,
        },
        ids,
    ))
}

#[cfg(test)]
pub fn parse_gold(json: &str) -> Result<crate::data::Pairs> {
    parse_gold_ids(json).map(|(pairs, _)| pairs)
}

pub fn load_gold_ids(path: &Path) -> Result<(crate::data::Pairs, Vec<String>)> {
    let json = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_gold_ids(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pages_on_markers() {
        let md = "# doc\n\n<!-- page 1 -->\n\nfirst page text\nmore\n<!-- page 2 -->\nsecond\n";
        let pages = split_pages(md);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].0, 1);
        assert!(pages[0].1.contains("first page text"));
        assert!(pages[0].1.contains("more"));
        assert_eq!(pages[1].0, 2);
        assert!(pages[1].1.contains("second"));
        // preamble before the first marker belongs to no page
        assert!(!pages[0].1.contains("# doc"));
    }

    #[test]
    fn parse_gold_aligns_queries_with_gold_docs() {
        let json = r#"{
            "docs": [
                {"id": "a#p1", "text": "alpha"},
                {"id": "b#p2", "text": "beta"},
                {"id": "c#p3", "text": "gamma"}
            ],
            "queries": [{"q": "find beta", "gold": "b#p2"}]
        }"#;
        let pairs = parse_gold(json).unwrap();
        assert_eq!(pairs.questions, vec!["find beta"]);
        // gold for query 0 sits at answer 0; the rest are distractors
        assert_eq!(pairs.answers[0], "beta");
        assert_eq!(pairs.answers.len(), 3);
    }

    #[test]
    fn parse_gold_rejects_dangling_and_duplicate_gold() {
        let dangling = r#"{"docs": [], "queries": [{"q": "x", "gold": "nope#p1"}]}"#;
        assert!(parse_gold(dangling).is_err());
        let dup = r#"{
            "docs": [{"id": "a#p1", "text": "alpha"}],
            "queries": [{"q": "x", "gold": "a#p1"}, {"q": "y", "gold": "a#p1"}]
        }"#;
        assert!(parse_gold(dup).is_err());
    }
}
