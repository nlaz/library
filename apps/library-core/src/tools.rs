//! Agent-facing tools, shared by every host (library-server HTTP routes,
//! the desktop app's in-process handler). The consumer is a ~3B on-device
//! model with a 4k-token context, which shapes everything here:
//!
//! - Results are compact JSON values, not wire types — no boxes/crops.
//! - Errors are *content* the model can act on ("page 12 is blank"), never
//!   empty strings — an empty tool result reads as evidence-of-nothing and
//!   the model confabulates over it (measured; see docs/spikes/chat-spike.md).
//! - Search runs with `complete = false` (agent queries are whole words)
//!   and reports absolute confidence — `rel` is relative-to-top, so junk
//!   hits on a miss query still score 1.0.

use std::path::Path;

use serde_json::{Value, json};

use crate::wire::read_collections;
use crate::{ChunkKey, Emb, FxHashSet, Hit, Library, MIN_REL, Readers, search};

/// Hits returned to the model per search (each ~40 tokens slim).
pub const TOOL_K: usize = 6;
/// The top hit's full page text rides along with search results so the
/// model can usually answer in one hop (snippets alone can't answer
/// paraphrase queries — ese recall is shallow there). ~1 page.
pub const TOP_PAGE_CHARS: usize = 1600;
/// `read_pages` caps: the whole read must fit the model's context with
/// room for instructions + history.
pub const MAX_TEXT_PAGES: u32 = 2;
pub const MAX_TEXT_CHARS: usize = 2500;
/// Below this many chars a page slice is "blank" (scan-only page, no OCR
/// text) and must come back as an explicit error, not as content.
pub const BLANK_CHARS: usize = 40;

/// Raw-BM25 floor below which even a full-coverage hit is dubious
/// (calibrated on this corpus via `librarian-eval retrieval`: real hits
/// run 10–22, junk tails < 6).
const BM25_WEAK: f32 = 6.0;

/// Stopword-ish tokens excluded from coverage (coverage over these would
/// let "how to keep..." score high on any page of running prose).
const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "in", "on", "to", "for", "with",
    "from", "that", "this", "how", "what", "why", "when", "where", "which",
    "is", "are", "was", "do", "does", "can", "you", "your", "my", "about",
];

/// Absolute confidence for a query's hits. Raw BM25 alone can't call a
/// miss — "quantum entanglement" scores high on Clean Code's (real!)
/// "entanglement" chapter — so the deciding signal is *coverage*: did any
/// hit match every content word of the query, or only some? Partial
/// coverage is exactly what an off-intent match looks like.
pub fn confidence(top_bm25: f32, coverage: f32) -> &'static str {
    if top_bm25 <= 0.0 {
        "none"
    } else if coverage >= 0.99 && top_bm25 >= BM25_WEAK {
        "strong"
    } else {
        "weak"
    }
}

/// Best single-hit coverage of the query's content tokens (prefix match,
/// so "dome" covers "domes"). Per-hit, not a union across hits — one book
/// mentioning "quantum" and another "entanglement" is two off-intent
/// matches, not evidence the corpus covers quantum entanglement. 1.0 when
/// the query has no content tokens — coverage is meaningless there, BM25
/// decides.
fn query_coverage(query: &str, hits: &[Hit]) -> f32 {
    let content: Vec<String> = crate::tokenize(query)
        .into_iter()
        .filter(|t| t.len() >= 3 && !STOP.contains(&t.as_str()))
        .collect();
    if content.is_empty() {
        return 1.0;
    }
    hits.iter()
        .map(|h| {
            let mut toks: FxHashSet<String> = FxHashSet::default();
            for w in &h.words {
                toks.extend(crate::tokenize(&w.t));
            }
            let matched = content
                .iter()
                .filter(|q| toks.iter().any(|t| t.starts_with(q.as_str())))
                .count();
            matched as f32 / content.len() as f32
        })
        .fold(0.0, f32::max)
}

fn read_titles(data: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read(data.join("titles.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Truncate in place to at most `max` bytes without splitting a char.
pub fn truncate_chars(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
}

// ---------------------------------------------------------------------------
// search_library
// ---------------------------------------------------------------------------

/// Text search shaped for an agent. Images are deliberately absent: for
/// figure search the host embeds the query with CLIP and calls
/// [`image_hits_for_tool`] instead (kind="images" is opt-in — image hits
/// leading a text answer are noise the model can't use).
pub fn search_tool<R: fold::stream::Readable>(
    r: &Readers<'_, R>,
    lib: &Library,
    data: &Path,
    query: &str,
    col: &str,
    k: usize,
) -> Value {
    let member: Option<FxHashSet<String>> = (!col.is_empty())
        .then(|| read_collections(data).remove(col))
        .flatten()
        .map(|docs| docs.into_iter().collect());

    let qemb: Emb = ese::encode_single(query);
    let k = k.clamp(1, TOOL_K);
    let mut hits = search(r, query, Some(&qemb), k.max(TOOL_K), member.as_ref(), false, |key| {
        lib.get(key).map(|rec| rec.words)
    });
    hits.retain(|h| h.rel >= MIN_REL);
    hits.truncate(k);

    let top_bm25 = hits.iter().map(|h| h.bm25).fold(0.0f32, f32::max);
    let coverage = query_coverage(query, &hits);
    let conf = confidence(top_bm25, coverage);
    let titles = read_titles(data);

    let slim: Vec<Value> = hits.iter().map(|h| slim_hit(h, &titles)).collect();

    let mut out = json!({
        "confidence": conf,
        "top_bm25": top_bm25,
        "coverage": coverage,
        "hits": slim,
    });
    if conf == "none" || slim.is_empty() {
        out["note"] = json!(
            "No strong matches — the library likely does not cover this. \
             Say so rather than stretching these hits."
        );
    } else if conf == "weak" {
        out["note"] = json!(
            "Only partial matches — these hits share some words with the query \
             but may not answer it. Read a page before relying on them, and say \
             so if they don't actually cover the question."
        );
    }
    if conf != "none" && let Some(top) = hits.first() {
        // one hop instead of two: the top hit's whole page rides along
        if let Some(text) = page_slice(data, &top.key.doc, top.key.page, top.key.page) {
            let mut text = text;
            truncate_chars(&mut text, TOP_PAGE_CHARS);
            if text.len() >= BLANK_CHARS {
                out["top_hit_page"] = json!({
                    "doc": top.key.doc,
                    "page": top.key.page,
                    "text": text,
                });
            }
        }
    }
    out
}

fn slim_hit(h: &Hit, titles: &std::collections::BTreeMap<String, String>) -> Value {
    let mut snippet: String =
        h.words.iter().map(|w| w.t.as_str()).collect::<Vec<_>>().join(" ");
    truncate_chars(&mut snippet, 200);
    json!({
        "doc": h.key.doc,
        "title": titles.get(&h.key.doc),
        "page": h.key.page,
        "snippet": snippet,
    })
}

/// Figure-search results for kind="images"; `found` comes from the host's
/// `image_search` call (the CLIP query embedding lives host-side).
pub fn image_hits_for_tool(
    found: &[crate::ImageHit],
    data: &Path,
    k: usize,
) -> Value {
    let titles = read_titles(data);
    let hits: Vec<Value> = found
        .iter()
        .take(k.clamp(1, TOOL_K))
        .map(|h| {
            json!({
                "doc": h.key.doc,
                "title": titles.get(&h.key.doc),
                "page": h.key.page,
                "kind": "figure",
            })
        })
        .collect();
    if hits.is_empty() {
        json!({
            "confidence": "none",
            "hits": [],
            "note": "No matching figures found.",
        })
    } else {
        json!({ "confidence": "weak", "hits": hits,
                "note": "Figure matches are visual-similarity only; verify by reading the page." })
    }
}

// ---------------------------------------------------------------------------
// read_pages
// ---------------------------------------------------------------------------

/// Reading-order text for a page span. Fuzzy doc-id resolution (small
/// models mangle long ids), hard caps, and *loud* errors for blank pages —
/// the model treats explicit errors correctly but confabulates over
/// silently-empty text.
pub fn read_pages_tool(data: &Path, doc: &str, from: Option<u32>, to: Option<u32>) -> Value {
    let dir = data.join("text");
    let stems: Vec<String> = std::fs::read_dir(&dir)
        .map(|it| {
            it.flatten()
                .filter_map(|f| {
                    let n = f.file_name();
                    let n = n.to_string_lossy();
                    n.strip_suffix(".md").map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    let resolved = if stems.iter().any(|s| s == doc) {
        doc.to_owned()
    } else {
        let needle = doc.to_lowercase();
        let mut m: Vec<&String> =
            stems.iter().filter(|s| s.to_lowercase().contains(&needle)).collect();
        m.sort_by_key(|s| s.len());
        match m.first() {
            Some(s) => (*s).clone(),
            None => {
                return json!({
                    "error": format!("no document matching {doc:?}"),
                    "available_docs": stems,
                });
            }
        }
    };

    let Ok(md) = std::fs::read_to_string(dir.join(format!("{resolved}.md"))) else {
        return json!({ "error": format!("could not read text for {resolved:?}") });
    };

    let from = from.unwrap_or(1).max(1);
    let to = to.unwrap_or(from).clamp(from, from + (MAX_TEXT_PAGES - 1));
    let (text, last_page) = slice_pages(&md, from, to);

    if from > last_page {
        return json!({
            "error": format!("{resolved:?} has pages 1..{last_page}, requested from={from}"),
            "doc": resolved,
        });
    }
    if text.trim().len() < BLANK_CHARS {
        // blank or image-only scan page: an explicit error, never empty text
        return json!({
            "error": format!(
                "pages {from}-{to} of {resolved:?} are blank or image-only (no readable text). \
                 Pick a different page — do not guess at this page's contents."
            ),
            "doc": resolved,
        });
    }
    let full = text.len();
    let mut text = text;
    truncate_chars(&mut text, MAX_TEXT_CHARS);
    json!({
        "doc": resolved,
        "from": from,
        "to": to,
        "total_pages": last_page,
        "truncated": full > MAX_TEXT_CHARS,
        "text": text.trim(),
    })
}

/// Slice `<!-- page N -->`-marked markdown to [from, to]; returns the text
/// and the last page marker seen (the doc's page count as textout wrote it).
fn slice_pages(md: &str, from: u32, to: u32) -> (String, u32) {
    let mut page = 0u32;
    let mut last = 0u32;
    let mut text = String::new();
    for line in md.lines() {
        if let Some(n) = line
            .strip_prefix("<!-- page ")
            .and_then(|r| r.strip_suffix(" -->"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            page = n;
            last = n;
            continue;
        }
        if page >= from && page <= to {
            text.push_str(line);
            text.push('\n');
        }
    }
    (text, last)
}

/// One page's text (for the search tool's one-hop ride-along); None if the
/// doc has no text file.
fn page_slice(data: &Path, doc: &str, from: u32, to: u32) -> Option<String> {
    let md = std::fs::read_to_string(data.join("text").join(format!("{doc}.md"))).ok()?;
    Some(slice_pages(&md, from, to).0)
}

// ---------------------------------------------------------------------------
// list_collections
// ---------------------------------------------------------------------------

pub fn collections_tool(data: &Path) -> Value {
    let titles = read_titles(data);
    let cols = read_collections(data);
    let out: serde_json::Map<String, Value> = cols
        .into_iter()
        .map(|(name, docs)| {
            let entries: Vec<Value> = docs
                .iter()
                .map(|d| json!({ "doc": d, "title": titles.get(d) }))
                .collect();
            (name, Value::Array(entries))
        })
        .collect();
    Value::Object(out)
}

/// Used by hosts to expose the same key the resolve callback uses.
pub fn chunk_key(doc: &str, page: u32, idx: u32) -> ChunkKey {
    ChunkKey { doc: doc.into(), page, idx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_pages_respects_markers() {
        let md = "# t\n<!-- page 1 -->\none\n<!-- page 2 -->\ntwo\n<!-- page 3 -->\nthree\n";
        let (text, last) = slice_pages(md, 2, 2);
        assert_eq!(text.trim(), "two");
        assert_eq!(last, 3);
    }

    #[test]
    fn blank_page_is_an_error_not_content() {
        let dir = std::env::temp_dir().join("tools-test-blank");
        std::fs::create_dir_all(dir.join("text")).unwrap();
        std::fs::write(
            dir.join("text/somedoc.md"),
            "# somedoc\n<!-- page 1 -->\n\n<!-- page 2 -->\nreal text on page two that is long enough to pass the blank floor\n",
        )
        .unwrap();
        let blank = read_pages_tool(&dir, "somedoc", Some(1), Some(1));
        assert!(blank["error"].as_str().unwrap().contains("blank"));
        let ok = read_pages_tool(&dir, "somedoc", Some(2), Some(2));
        assert!(ok["error"].is_null());
        assert!(ok["text"].as_str().unwrap().contains("real text"));
        // fuzzy id still resolves
        let fuzzy = read_pages_tool(&dir, "SomeDoc", Some(2), Some(2));
        assert_eq!(fuzzy["doc"], "somedoc");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn confidence_needs_full_coverage_and_bm25() {
        assert_eq!(confidence(20.0, 1.0), "strong");
        assert_eq!(confidence(20.0, 0.5), "weak"); // off-intent partial match
        assert_eq!(confidence(3.0, 1.0), "weak"); // full match, junk-tail score
        assert_eq!(confidence(0.0, 1.0), "none");
    }
}
