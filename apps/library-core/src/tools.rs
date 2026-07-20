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

use crate::legibility::{
    LEGIBLE_OK, NOISY_MIN, legibility, legible_excerpt, min_window, squash_ws,
};
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

/// Rides along with page text whose OCR has garbled stretches — the model
/// quotes tool text verbatim unless told otherwise, and quoting column-
/// interleaved salad was a top complaint from live use.
const NOISY_NOTE: &str = "Parts of this page's OCR are noisy. Paraphrase what \
                          is legible — never quote garbled text.";

/// Raw-BM25 floor below which even a full-coverage hit is dubious
/// (calibrated on this corpus via `librarian-eval retrieval`: real hits
/// run 10–22, junk tails < 6).
const BM25_WEAK: f32 = 6.0;

/// Stopword-ish tokens excluded from coverage (coverage over these would
/// let "how to keep..." score high on any page of running prose).
const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "in", "on", "to", "for", "with", "from", "that", "this",
    "how", "what", "why", "when", "where", "which", "is", "are", "was", "do", "does", "can", "you",
    "your", "my", "about",
];

/// Absolute confidence for a query's hits. Raw BM25 alone can't call a
/// miss — "quantum entanglement" scores high on a page that only really
/// covers "entanglement" — so the deciding signal is *coverage*: did any
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

/// Human-readable title derived from a doc id, for docs missing from
/// titles.json. Download filenames often carry the title before a
/// parenthetical tag (`Title (Author) (source).pdf`); kebab-case ids
/// prettify word-by-word. Opaque scanned-archive ids (e.g. `somebook00abcd`)
/// derive badly — those need real titles.json entries.
pub fn derive_title(doc: &str) -> String {
    let base = doc.split(" (").next().unwrap_or(doc).trim();
    if base.contains(' ') {
        return base.to_owned();
    }
    base.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) if w.len() > 2 => f.to_uppercase().chain(c).collect::<String>(),
                _ => w.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The model-facing title: titles.json first, derived otherwise — never
/// null, so the model always has something better than a raw id to say.
fn title_for(titles: &std::collections::BTreeMap<String, String>, doc: &str) -> String {
    titles
        .get(doc)
        .cloned()
        .unwrap_or_else(|| derive_title(doc))
}

/// Reverse doc → collection-name map (first collection wins). Hits carry
/// this so the model can't misattribute a doc to the wrong shelf.
fn collection_of(data: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (name, docs) in read_collections(data) {
        for d in docs {
            out.entry(d).or_insert_with(|| name.clone());
        }
    }
    out
}

/// Resolve a collection argument: Ok(None) for "", Ok(Some(docs)) on a
/// fuzzy match ("Field Guides" resolves to "field-guides"), Err(json) the
/// model can act on when nothing matches — never a silent search-everything.
pub fn resolve_collection(data: &Path, col: &str) -> Result<Option<FxHashSet<String>>, Value> {
    if col.is_empty() {
        return Ok(None);
    }
    let cols = read_collections(data);
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let n = norm(col);
    // exact first; else the LONGEST overlapping name — "Field Guides
    // Collection" must pick field-guides, not Guides, when both overlap
    let found = cols.iter().find(|(name, _)| norm(name) == n).or_else(|| {
        cols.iter()
            .filter(|(name, _)| {
                let m = norm(name);
                m.contains(&n) || n.contains(&m)
            })
            .max_by_key(|(name, _)| norm(name).len())
    });
    match found {
        Some((_, docs)) => Ok(Some(docs.iter().cloned().collect())),
        None => Err(json!({
            "error": format!("no collection matching {col:?}"),
            "collections": cols.keys().collect::<Vec<_>>(),
        })),
    }
}

/// "somebook00abcd-6" -> "somebook00abcd". Ids without a trailing all-digit
/// suffix pass through. Ids like "journal-1891" strip to "journal", which is
/// harmless: the copy cap in [`dedup_doc_pages`] only engages when several
/// *distinct* docs share a base.
pub(crate) fn base_id(doc: &str) -> &str {
    match doc.rfind('-') {
        Some(i) if !doc[i + 1..].is_empty() && doc[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            &doc[..i]
        }
        _ => doc,
    }
}

/// Rank-order indices to keep from ranked (doc, page) hits. The library
/// holds several physical copies of the same book (`-N` suffixed ids);
/// without this, one catalog's copies crowd out every other source. Same
/// base id + page collapses to the best-ranked copy, and a base with 2+
/// distinct copy docs contributes at most 2 hits total.
pub(crate) fn dedup_doc_pages(keys: &[(&str, u32)]) -> Vec<usize> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut docs_per_base: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (doc, _) in keys {
        docs_per_base.entry(base_id(doc)).or_default().insert(doc);
    }
    let mut seen: BTreeSet<(&str, u32)> = BTreeSet::new();
    let mut per_base: BTreeMap<&str, usize> = BTreeMap::new();
    let mut keep = Vec::new();
    for (i, (doc, page)) in keys.iter().enumerate() {
        let base = base_id(doc);
        let copies = docs_per_base[base].len() >= 2;
        if !seen.insert(if copies { (base, *page) } else { (*doc, *page) }) {
            continue;
        }
        if copies {
            let n = per_base.entry(base).or_insert(0);
            if *n >= 2 {
                continue;
            }
            *n += 1;
        }
        keep.push(i);
    }
    keep
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
    let member = match resolve_collection(data, col) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let qemb: Emb = ese::encode_single(query);
    let k = k.clamp(1, TOOL_K);
    // fetch 2x: copy dedup below may drop hits, and the extras backfill.
    // complete off (agent queries are whole words, not mid-typing); fuzzy on
    // (only unknown tokens get corrected, so real words are untouched — this
    // recovers typos and OCR garble); diversify off (the tool does its own
    // dedup_doc_pages below).
    let mut hits = search(
        r,
        query,
        Some(&qemb),
        TOOL_K * 2,
        member.as_ref(),
        false,
        true,
        false,
        |key| lib.get(key),
        None,
    );
    hits.retain(|h| h.rel >= MIN_REL);
    let keep = {
        let keys: Vec<(&str, u32)> = hits
            .iter()
            .map(|h| (h.key.doc.as_str(), h.key.page))
            .collect();
        dedup_doc_pages(&keys)
    };
    let mut i = 0;
    hits.retain(|_| {
        let keep_it = keep.binary_search(&i).is_ok();
        i += 1;
        keep_it
    });
    hits.truncate(k);

    let top_bm25 = hits.iter().map(|h| h.bm25).fold(0.0f32, f32::max);
    let coverage = query_coverage(query, &hits);
    let conf = confidence(top_bm25, coverage);
    let titles = read_titles(data);
    let cols = collection_of(data);

    let slim: Vec<Value> = hits.iter().map(|h| slim_hit(h, &titles, &cols)).collect();

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
    if conf != "none"
        && let Some(top) = hits.first()
    {
        // one hop instead of two: the top hit's whole page rides along
        if let Some(text) = page_slice(data, &top.key.doc, top.key.page, top.key.page) {
            let mut text = squash_ws(&text);
            truncate_chars(&mut text, TOP_PAGE_CHARS);
            if text.len() >= BLANK_CHARS {
                let mut thp = json!({
                    "doc": top.key.doc,
                    "page": top.key.page,
                    "text": text,
                });
                if min_window(thp["text"].as_str().unwrap_or("")) < NOISY_MIN {
                    thp["note"] = json!(NOISY_NOTE);
                }
                out["top_hit_page"] = thp;
            }
        }
    }
    out
}

fn slim_hit(
    h: &Hit,
    titles: &std::collections::BTreeMap<String, String>,
    cols: &std::collections::BTreeMap<String, String>,
) -> Value {
    let mut snippet: String = h
        .words
        .iter()
        .map(|w| w.t.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&mut snippet, 200);
    json!({
        "doc": h.key.doc,
        "title": title_for(titles, &h.key.doc),
        "col": cols.get(&h.key.doc),
        "page": h.key.page,
        "snippet": snippet,
    })
}

/// Figure-search results for kind="images"; `found` comes from the host's
/// `image_search` call (the CLIP query embedding lives host-side).
pub fn image_hits_for_tool(found: &[crate::ImageHit], data: &Path, k: usize) -> Value {
    let titles = read_titles(data);
    let cols = collection_of(data);
    let keep = {
        let keys: Vec<(&str, u32)> = found
            .iter()
            .map(|h| (h.key.doc.as_str(), h.key.page))
            .collect();
        dedup_doc_pages(&keys)
    };
    let hits: Vec<Value> = keep
        .iter()
        .map(|&i| &found[i])
        .take(k.clamp(1, TOOL_K))
        .map(|h| {
            json!({
                "doc": h.key.doc,
                "title": title_for(&titles, &h.key.doc),
                "col": cols.get(&h.key.doc),
                "page": h.key.page,
                "kind": "figure",
            })
        })
        .collect();
    if hits.is_empty() {
        return json!({
            "confidence": "none",
            "hits": [],
            "note": "No matching figures found.",
        });
    }
    // the 3B model won't reliably chain a read_pages verify hop after a
    // figure search (measured: fig-verify probe) — so the top figure's page
    // text rides along, same as search_tool's top_hit_page
    let mut out = json!({ "confidence": "weak", "hits": hits,
            "note": "Figure matches are visual-similarity guesses, and these scan \
                     pages have no readable text. Point the user at the page \
                     ([Title p.N]) — do not describe what a figure shows." });
    if let Some(top) = found.first()
        && let Some(text) = page_slice(data, &top.key.doc, top.key.page, top.key.page)
    {
        let mut text = squash_ws(&text);
        truncate_chars(&mut text, TOP_PAGE_CHARS);
        if text.len() >= BLANK_CHARS {
            let noisy = min_window(&text) < NOISY_MIN;
            out["top_hit_page"] = json!({
                "doc": top.key.doc,
                "page": top.key.page,
                "text": text,
            });
            let mut note = "Figure matches are visual-similarity guesses. top_hit_page \
                            is the first figure's page text — describe figures only as \
                            that text supports, and cite the page."
                .to_owned();
            if noisy {
                note.push(' ');
                note.push_str(NOISY_NOTE);
            }
            out["note"] = json!(note);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// read_pages
// ---------------------------------------------------------------------------

/// Reading-order text for a page span. Fuzzy doc-id resolution (small
/// models mangle long ids), hard caps, and *loud* errors for blank pages —
/// the model treats explicit errors correctly but confabulates over
/// silently-empty text.
pub fn read_pages_tool(data: &Path, doc: &str, from: Option<u32>, to: Option<u32>) -> Value {
    if crate::records::is_reserved(doc) {
        // reserved ids contain `/` and must never reach a path join
        return serde_json::json!({ "error": "no such document" });
    }
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
        // ids AND titles resolve: the overview and search results speak in
        // titles, and small models mangle long ids anyway — match on a
        // normalized (alphanumeric, lowercase) form of the stem and of the
        // stem's title, so "The Art of Plain Cookery" finds
        // the-art-of-plain-cookery and titles.json entries alike.
        let norm = |s: &str| {
            s.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        };
        let titles = read_titles(data);
        let needle = norm(doc);
        let mut m: Vec<&String> = if needle.is_empty() {
            Vec::new()
        } else {
            stems
                .iter()
                .filter(|s| {
                    norm(s).contains(&needle) || norm(&title_for(&titles, s)).contains(&needle)
                })
                .collect()
        };
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
    let (_, last_page) = slice_pages(&md, 0, 0);

    if from > last_page {
        return json!({
            "error": format!("{resolved:?} has pages 1..{last_page}, requested from={from}"),
            "doc": resolved,
        });
    }
    // per page so boundaries stay marked once whitespace is squashed
    // (newlines used to imply them, but the model parrots \n escapes)
    let mut parts = Vec::new();
    for p in from..=to {
        let t = squash_ws(&slice_pages(&md, p, p).0);
        if !t.is_empty() {
            parts.push(if from == to {
                t
            } else {
                format!("[p.{p}] {t}")
            });
        }
    }
    let text = parts.join(" ");
    if text.len() < BLANK_CHARS {
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
    let mut out = json!({
        "doc": resolved,
        "title": title_for(&read_titles(data), &resolved),
        "col": collection_of(data).get(&resolved),
        "from": from,
        "to": to,
        "total_pages": last_page,
        "truncated": full > MAX_TEXT_CHARS,
        "text": text,
    });
    if min_window(out["text"].as_str().unwrap_or("")) < NOISY_MIN {
        out["note"] = json!(NOISY_NOTE);
    }
    out
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
// sample_page
// ---------------------------------------------------------------------------

/// A random readable page, for browse/serendipity asks ("tell me something
/// interesting", "another one") — the app does the picking, the model
/// narrates. Serves only each page's quotable stretches (legible_excerpt):
/// browse doesn't need page fidelity, and a "don't quote the garbled bits"
/// note would just get parroted at the user. Pages with nothing quotable
/// are skipped like blanks; error-as-content on exhaustion.
///
/// `seed` is a test hook; None draws from the clock. `avoid` holds
/// "doc:page" strings of recently served pages (host-side session state,
/// never model-visible) so "another one" walks new shelves; it is ignored
/// rather than erroring when it would exhaust the candidates.
pub fn sample_page_tool(data: &Path, col: &str, seed: Option<u64>, avoid: &[String]) -> Value {
    let member = match resolve_collection(data, col) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let dir = data.join("text");
    let mut docs: Vec<String> = std::fs::read_dir(&dir)
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
    if let Some(m) = &member {
        docs.retain(|d| m.contains(d));
    }
    docs.sort();
    if docs.is_empty() {
        return json!({ "error": "no readable documents in that collection" });
    }

    let mut state = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos()) ^ d.as_secs())
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
    });
    // splitmix64: enough randomness for page-picking, no new dependency
    let mut next = move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };

    let titles = read_titles(data);
    let cols = collection_of(data);
    // best low-scoring candidate seen, served (with a note) only if the
    // draw budget never lands on a legible page
    let mut best: Option<(f32, Value)> = None;
    let mut attempt = |honor_avoid: bool, best: &mut Option<(f32, Value)>| -> Option<Value> {
        for _ in 0..10 {
            let doc = &docs[(next() % docs.len() as u64) as usize];
            let Ok(md) = std::fs::read_to_string(dir.join(format!("{doc}.md"))) else {
                continue;
            };
            let (_, last) = slice_pages(&md, 0, 0);
            if last == 0 {
                continue;
            }
            let page = (next() % u64::from(last)) as u32 + 1;
            if honor_avoid && avoid.iter().any(|a| a == &format!("{doc}:{page}")) {
                continue;
            }
            let (text, _) = slice_pages(&md, page, page);
            if text.trim().len() < BLANK_CHARS {
                continue;
            }
            let mut text = legible_excerpt(&squash_ws(&text));
            if text.len() < BLANK_CHARS {
                // nothing quotable on this page — treat like a blank
                continue;
            }
            truncate_chars(&mut text, TOP_PAGE_CHARS);
            let score = legibility(&text);
            let out = json!({
                "doc": doc,
                "title": title_for(&titles, doc),
                "col": cols.get(doc),
                "page": page,
                "total_pages": last,
                "text": text,
            });
            if score >= LEGIBLE_OK {
                return Some(out);
            }
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                *best = Some((score, out));
            }
        }
        None
    };
    if let Some(out) = attempt(true, &mut best) {
        return out;
    }
    if best.is_none()
        && !avoid.is_empty()
        && let Some(out) = attempt(false, &mut best)
    {
        return out;
    }
    if let Some((_, out)) = best {
        return out;
    }
    json!({
        "error": "could not find a readable page — the collection may be mostly image-only scans"
    })
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
                .map(|d| json!({ "doc": d, "title": title_for(&titles, d) }))
                .collect();
            (name, Value::Array(entries))
        })
        .collect();
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// library_overview
// ---------------------------------------------------------------------------

/// Example titles listed per collection in the overview.
const OVERVIEW_EXAMPLES: usize = 3;

/// A gestalt of the library, slim enough that a 4k-context model can afford
/// to orient with it before deciding where to look: collection names, sizes,
/// and a few example titles. Deliberately NOT [`collections_tool`]'s full
/// doc-id dump — that one serves UIs. Titles here resolve in `read_pages`
/// (see the normalized match there), so the model can go straight from the
/// overview to reading.
pub fn overview_tool(data: &Path) -> Value {
    let titles = read_titles(data);
    let cols = read_collections(data);
    let stems: Vec<String> = std::fs::read_dir(data.join("text"))
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
    let filed: FxHashSet<&String> = cols.values().flatten().collect();
    let shelves: Vec<Value> = cols
        .iter()
        .map(|(name, docs)| {
            let examples: Vec<String> = docs
                .iter()
                .take(OVERVIEW_EXAMPLES)
                .map(|d| title_for(&titles, d))
                .collect();
            json!({ "collection": name, "books": docs.len(), "examples": examples })
        })
        .collect();
    let loose = stems.iter().filter(|d| !filed.contains(d)).count();
    let mut out = json!({ "books": stems.len(), "collections": shelves });
    if loose > 0 {
        out["uncollected_books"] = json!(loose);
    }
    out
}

/// Used by hosts to expose the same key the resolve callback uses.
pub fn chunk_key(doc: &str, page: u32, idx: u32) -> ChunkKey {
    ChunkKey {
        doc: doc.into(),
        page,
        idx,
    }
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

    #[test]
    fn derive_title_handles_id_shapes() {
        assert_eq!(
            derive_title("the-art-of-plain-cookery"),
            "The Art of Plain Cookery"
        );
        assert_eq!(
            derive_title("Field Guide to Mushrooms (A. Botanist) (source.example)"),
            "Field Guide to Mushrooms"
        );
        assert_eq!(derive_title("a-history-of-tea"), "a History of Tea");
    }

    fn hit(doc: &str, words: &[&str]) -> Hit {
        Hit {
            score: 1.0,
            rel: 1.0,
            bm25: 10.0,
            lex_rank: Some(0),
            sem_rank: None,
            sem_dist: None,
            key: crate::ChunkKey {
                doc: doc.to_string(),
                page: 1,
                idx: 0,
            },
            words: words
                .iter()
                .map(|t| crate::Word {
                    t: t.to_string(),
                    x: 0.1,
                    y: 0.2,
                    w: 0.3,
                    h: 0.4,
                })
                .collect(),
        }
    }

    #[test]
    fn query_coverage_partial_and_full() {
        // per-hit max, not a union: one book on "quantum", another on
        // "entanglement" is two half-covered hits, not full coverage
        let hits = [hit("a", &["quantum"]), hit("b", &["entanglement"])];
        assert_eq!(query_coverage("quantum entanglement", &hits), 0.5);
        let both = [hit("c", &["quantum", "entanglement", "primer"])];
        assert_eq!(query_coverage("quantum entanglement", &both), 1.0);
        // prefix match: "dome" covers "domes"
        assert_eq!(query_coverage("dome", &[hit("d", &["domes"])]), 1.0);
        // stopwords and short tokens leave no content: coverage is
        // meaningless, defined as 1.0 (BM25 decides)
        assert_eq!(query_coverage("the of an", &hits), 1.0);
        // no hits at all: nothing covers the content tokens
        assert_eq!(query_coverage("quantum", &[]), 0.0);
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        // cutting mid-'é' must back up to the char boundary, not panic
        let mut s = "ééé".to_string(); // 6 bytes
        truncate_chars(&mut s, 3);
        assert_eq!(s, "é…");
        // at or under the cap: untouched, no ellipsis
        let mut s = "short".to_string();
        truncate_chars(&mut s, 5);
        assert_eq!(s, "short");
        // over the cap: content is cut to <= max bytes, then the ellipsis
        // is appended on top (the final string may exceed max by '…')
        let mut s = "x".repeat(210);
        truncate_chars(&mut s, 200);
        assert!(s.ends_with('…'));
        assert_eq!(s.len(), 200 + '…'.len_utf8());
    }

    #[test]
    fn slim_hit_drops_word_boxes_and_truncates_snippet() {
        let words: Vec<String> = (0..60).map(|i| format!("word{i}")).collect();
        let refs: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
        let h = hit("doc-a", &refs);
        let out = slim_hit(&h, &Default::default(), &Default::default());
        // agent-facing shape: metadata + snippet only — no word boxes,
        // scores, or ranks (that's the point of "slim")
        let keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, vec!["col", "doc", "page", "snippet", "title"]);
        let snippet = out["snippet"].as_str().unwrap();
        assert!(snippet.ends_with('…'));
        assert!(snippet.len() <= 200 + '…'.len_utf8());
        // no titles.json entry: falls back to derive_title
        assert_eq!(out["title"], "Doc a");
    }

    fn sample_fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tools-test-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("text")).unwrap();
        std::fs::write(
            dir.join("text/doc-a.md"),
            "# a\n<!-- page 1 -->\nplenty of readable text here, long enough to clear the blank floor\n<!-- page 2 -->\nsecond page also has plenty of readable text to clear the floor\n",
        )
        .unwrap();
        std::fs::write(dir.join("text/doc-b.md"), "# b\n<!-- page 1 -->\n\n").unwrap();
        std::fs::write(
            dir.join("text/doc-garbled.md"),
            "# g\n<!-- page 1 -->\nGotu iicher cs Veckly sty Merkel Stece ee 7 ee ee cs Seeeetca Uriters ay ck vb 9 zz qf om Veckly Stece ee cs ee ay ck\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("collections.json"),
            r#"{"field-guides": ["doc-a"], "recipes": ["doc-b"], "Guides": ["doc-b"], "garbled": ["doc-garbled"]}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn resolve_collection_is_fuzzy_and_loud() {
        let dir = sample_fixture("resolve");
        assert!(resolve_collection(&dir, "").unwrap().is_none());
        let m = resolve_collection(&dir, "Field Guides").unwrap().unwrap();
        assert!(m.contains("doc-a"));
        let err = resolve_collection(&dir, "bogus").unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("bogus"));
        assert!(err["collections"].as_array().unwrap().len() == 4);
        // overlapping names pick the longest match, not map order:
        // "Field Guides Collection" must hit field-guides, not Guides
        let m = resolve_collection(&dir, "Field Guides Collection")
            .unwrap()
            .unwrap();
        assert!(m.contains("doc-a"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sample_page_is_seeded_and_skips_blanks() {
        let dir = sample_fixture("sample");
        let a = sample_page_tool(&dir, "field-guides", Some(42), &[]);
        let b = sample_page_tool(&dir, "field-guides", Some(42), &[]);
        assert_eq!(a, b); // seed determinism
        assert_eq!(a["doc"], "doc-a");
        assert!(a["text"].as_str().unwrap().len() >= BLANK_CHARS);
        assert!(!a["text"].as_str().unwrap().contains('\n'));
        assert_eq!(a["col"], "field-guides");
        assert!(
            a["note"].is_null(),
            "clean page must not carry a noisy note"
        );
        // doc-b's only page is blank: exhaustion is an error, not empty text
        let blank = sample_page_tool(&dir, "recipes", Some(1), &[]);
        assert!(blank["error"].as_str().unwrap().contains("readable"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sample_page_skips_garbled_pages_and_honors_avoid() {
        let dir = sample_fixture("sample-gate");
        // the garbled collection's only page has no quotable stretch: it is
        // skipped like a blank, and exhaustion stays an explicit error
        let g = sample_page_tool(&dir, "garbled", Some(7), &[]);
        assert!(
            g["error"].as_str().unwrap().contains("readable"),
            "got: {g}"
        );
        // avoid steers to the other readable page of doc-a...
        let first = sample_page_tool(&dir, "field-guides", Some(42), &[]);
        let avoid = format!("doc-a:{}", first["page"].as_u64().unwrap());
        let second = sample_page_tool(&dir, "field-guides", Some(42), std::slice::from_ref(&avoid));
        assert_eq!(second["doc"], "doc-a");
        assert_ne!(first["page"], second["page"]);
        // ...and is ignored, not fatal, when every candidate is avoided
        let both = vec!["doc-a:1".to_owned(), "doc-a:2".to_owned()];
        let anyway = sample_page_tool(&dir, "field-guides", Some(42), &both);
        assert_eq!(anyway["doc"], "doc-a");
        assert!(anyway["error"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base_id_strips_only_trailing_digit_suffixes() {
        assert_eq!(base_id("somebook00abcd-6"), "somebook00abcd");
        assert_eq!(base_id("somebook00abcd-10"), "somebook00abcd");
        assert_eq!(base_id("somebook00abcd"), "somebook00abcd");
        assert_eq!(base_id("a-history-of-tea"), "a-history-of-tea");
        assert_eq!(base_id("journal-1891"), "journal"); // harmless, see doc comment
    }

    #[test]
    fn dedup_collapses_copies_but_not_lone_docs() {
        // vol-2/vol-6 are copies (2 distinct docs, one base): same page
        // collapses, and the family is capped at 2 hits total
        let keys = vec![
            ("vol-2", 5),
            ("vol-6", 5),
            ("vol-2", 9),
            ("vol-6", 9),
            ("vol-2", 12),
            ("other", 1),
        ];
        assert_eq!(dedup_doc_pages(&keys), vec![0, 2, 5]);
        // journal-1891 strips to "journal" but is the only doc on that base:
        // no cap, only exact (doc, page) duplicates collapse
        let lone = vec![
            ("journal-1891", 3),
            ("journal-1891", 4),
            ("journal-1891", 5),
            ("journal-1891", 4),
        ];
        assert_eq!(dedup_doc_pages(&lone), vec![0, 1, 2]);
    }

    #[test]
    fn read_pages_resolves_titles_not_just_ids() {
        let dir = sample_fixture("resolve-title");
        std::fs::write(
            dir.join("titles.json"),
            r#"{"doc-a": "A Winter Cookery Primer"}"#,
        )
        .unwrap();
        // titles.json title, with different casing and punctuation
        let by_title = read_pages_tool(&dir, "winter cookery primer", Some(1), Some(1));
        assert_eq!(by_title["doc"], "doc-a", "got: {by_title}");
        // derived-title path (no titles.json entry) still resolves
        let by_derived = read_pages_tool(&dir, "Doc A", Some(1), Some(1));
        assert_eq!(by_derived["doc"], "doc-a");
        // empty needle is an error, not match-everything
        let empty = read_pages_tool(&dir, "", Some(1), Some(1));
        assert!(empty["error"].as_str().unwrap().contains("no document"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overview_is_slim_and_counts_loose_docs() {
        let dir = sample_fixture("overview");
        std::fs::write(
            dir.join("titles.json"),
            r#"{"doc-a": "A Winter Cookery Primer"}"#,
        )
        .unwrap();
        let out = overview_tool(&dir);
        assert_eq!(out["books"], 3); // doc-a, doc-b, doc-garbled
        let shelves = out["collections"].as_array().unwrap();
        assert_eq!(shelves.len(), 4);
        let fg = shelves
            .iter()
            .find(|s| s["collection"] == "field-guides")
            .expect("field-guides shelf");
        assert_eq!(fg["books"], 1);
        // examples are titles, never raw doc ids
        assert_eq!(fg["examples"][0], "A Winter Cookery Primer");
        // every doc is filed in the fixture: no uncollected count
        assert!(out["uncollected_books"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_pages_squashes_newlines_and_marks_page_boundaries() {
        let dir = sample_fixture("read-squash");
        let one = read_pages_tool(&dir, "doc-a", Some(1), Some(1));
        let t = one["text"].as_str().unwrap();
        assert!(!t.contains('\n'));
        assert!(!t.contains("[p."), "single-page reads carry no marker");
        let two = read_pages_tool(&dir, "doc-a", Some(1), Some(2));
        let t = two["text"].as_str().unwrap();
        assert!(!t.contains('\n'));
        assert!(t.contains("[p.1]") && t.contains("[p.2]"), "got: {t}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
