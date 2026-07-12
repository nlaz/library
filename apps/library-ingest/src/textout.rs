//! Markdown edition of a document: OCR words -> lines -> paragraphs ->
//! `data/text/<doc>.md`. The output is the corpus in reading order — what a
//! retrieval agent reads after search points it at a page — so each page is
//! preceded by an HTML comment marker that cites back to the scan.
//!
//! Reads cleaned pages (`data/clean/<doc>`) when present, falling back to
//! raw OCR per page (see `read_pages`). Layout (lines, paragraph breaks) is
//! always computed from the raw word geometry; hyphenated line breaks are
//! fused only when the text is emitted, so the geometry never shifts under
//! the layout analysis.

use std::path::{Path, PathBuf};

use anyhow::Result;
use library_core::{FxHashSet, Word, tokenize};

use crate::{PageOcr, read_pages};

/// A word joins the current line while its vertical center stays within
/// this fraction of the line's height from the line's center.
const LINE_BAND: f32 = 0.6;
/// Paragraph break when the top-to-top gap between lines exceeds this
/// multiple of the page's median gap.
const PARA_GAP: f32 = 1.7;
/// ...or when a line's left edge sits this far (in page widths) right of
/// the page's median left edge (a first-line indent).
const PARA_INDENT: f32 = 0.015;

/// Write `data/text/<doc>.md` and return its path.
pub fn write_doc(data: &Path, doc: &str) -> Result<PathBuf> {
    write_doc_pages(data, doc, &read_pages(data, doc)?)
}

/// [`write_doc`] from already-loaded pages (e.g. the set `prepare_text`
/// returns), skipping the re-read of the whole doc.
pub fn write_doc_pages(data: &Path, doc: &str, pages: &[PageOcr]) -> Result<PathBuf> {
    let md = markdown(doc, pages);
    let dir = data.join("text");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{doc}.md"));
    let tmp = dir.join(format!("{doc}.md.tmp"));
    std::fs::write(&tmp, md)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn markdown(doc: &str, pages: &[PageOcr]) -> String {
    // document vocabulary, for deciding hyphenated joins
    let vocab: FxHashSet<String> = pages
        .iter()
        .flat_map(|p| p.words.iter())
        .flat_map(|w| tokenize(&w.t))
        .collect();

    let mut out = format!("# {doc}\n");
    for page in pages {
        out.push_str(&format!("\n<!-- page {} -->\n", page.page));
        for para in paragraphs(&lines(&page.words)) {
            out.push('\n');
            out.push_str(&join_words(&para, &vocab));
            out.push('\n');
        }
    }
    out
}

/// Decide whether `prev` (ending in a lone `-`) and `next` are a word split
/// by a line break, and if so return the fused text. The hyphen is dropped
/// when the fused token occurs elsewhere in the document ("develop-" +
/// "ment" -> "development") and kept otherwise ("Apple-" + "compatible" ->
/// "Apple-compatible") — split prose words recur unhyphenated somewhere,
/// compounds don't.
fn fuse(prev: &str, next: &str, vocab: &FxHashSet<String>) -> Option<String> {
    if prev.len() < 2
        || !prev.ends_with('-')
        || prev.ends_with("--")
        || !next.chars().next().is_some_and(|c| c.is_lowercase())
    {
        return None;
    }
    let fused = format!("{}{}", &prev[..prev.len() - 1], next);
    let known = tokenize(&fused).first().is_some_and(|t| vocab.contains(t));
    Some(if known { fused } else { format!("{prev}{next}") })
}

/// Join a paragraph's words into prose, fusing hyphenated line breaks.
fn join_words(words: &[Word], vocab: &FxHashSet<String>) -> String {
    let mut out = String::new();
    for w in words {
        if let Some(at) = out.rfind(' ').map(|i| i + 1).or(Some(0)).filter(|_| !out.is_empty())
            && let Some(fused) = fuse(&out[at..], &w.t, vocab)
        {
            out.truncate(at);
            out.push_str(&fused);
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w.t);
    }
    out
}

struct Line {
    words: Vec<Word>,
    /// running mean of word vertical centers
    cy: f32,
    /// running mean of word heights
    h: f32,
}

impl Line {
    fn top(&self) -> f32 {
        self.cy - self.h / 2.0
    }

    fn left(&self) -> f32 {
        self.words.first().map_or(0.0, |w| w.x)
    }
}

/// Group words into lines. Vision emits words in reading order, so a
/// sequential scan suffices: a word starts a new line when its vertical
/// center leaves the current line's band.
fn lines(words: &[Word]) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for w in words {
        let cy = w.y + w.h / 2.0;
        match out.last_mut() {
            Some(line) if (cy - line.cy).abs() <= LINE_BAND * line.h.max(w.h) => {
                let n = line.words.len() as f32;
                line.cy = (line.cy * n + cy) / (n + 1.0);
                line.h = (line.h * n + w.h) / (n + 1.0);
                line.words.push(w.clone());
            }
            _ => out.push(Line { words: vec![w.clone()], cy, h: w.h }),
        }
    }
    out
}

/// Fold lines into paragraphs (each a flat word sequence): break on an
/// outsized vertical gap or a first-line indent, both measured against page
/// medians so the thresholds track the scan's type size.
fn paragraphs(lines: &[Line]) -> Vec<Vec<Word>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut gaps: Vec<f32> = lines.windows(2).map(|w| w[1].top() - w[0].top()).collect();
    gaps.sort_by(f32::total_cmp);
    let median_gap = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0);
    let mut lefts: Vec<f32> = lines.iter().map(Line::left).collect();
    lefts.sort_by(f32::total_cmp);
    let median_left = lefts[lefts.len() / 2];

    let mut paras: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let brk = i == 0
            || (median_gap > 0.0 && line.top() - lines[i - 1].top() > PARA_GAP * median_gap)
            || line.left() > median_left + PARA_INDENT;
        if brk && !cur.is_empty() {
            paras.push(std::mem::take(&mut cur));
        }
        cur.extend(line.words.iter().cloned());
    }
    if !cur.is_empty() {
        paras.push(cur);
    }
    paras
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str, x: f32, y: f32) -> Word {
        Word { t: t.into(), x, y, w: 0.04, h: 0.02 }
    }

    /// A line of words at `y`, starting at `x0`.
    fn line(ts: &[&str], x0: f32, y: f32) -> Vec<Word> {
        ts.iter().enumerate().map(|(i, t)| w(t, x0 + i as f32 * 0.05, y)).collect()
    }

    fn vocab_of(words: &[Word]) -> FxHashSet<String> {
        words.iter().flat_map(|w| tokenize(&w.t)).collect()
    }

    #[test]
    fn hyphen_join_prefers_known_words() {
        let words = [
            line(&["the", "develop-"], 0.1, 0.10),
            line(&["ment", "of", "Apple-"], 0.1, 0.13),
            line(&["compatible", "development"], 0.1, 0.16),
        ]
        .concat();
        let joined = join_words(&words, &vocab_of(&words));
        // "development" recurs in the doc -> fused; "applecompatible"
        // doesn't -> hyphen kept
        assert_eq!(joined, "the development of Apple-compatible development");
    }

    #[test]
    fn double_dash_and_capitals_do_not_merge() {
        let words = [
            line(&["either--"], 0.1, 0.10),
            line(&["it", "Visi-"], 0.1, 0.13),
            line(&["Calc"], 0.1, 0.16),
        ]
        .concat();
        assert_eq!(join_words(&words, &FxHashSet::default()), "either-- it Visi- Calc");
    }

    #[test]
    fn paragraphs_split_on_gap_and_indent() {
        let mut words = line(&["First", "paragraph", "line", "one"], 0.10, 0.10);
        words.extend(line(&["and", "line", "two."], 0.10, 0.13));
        // big vertical gap
        words.extend(line(&["Second", "paragraph."], 0.10, 0.25));
        // first-line indent
        words.extend(line(&["Third", "paragraph", "opens", "indented"], 0.13, 0.28));
        words.extend(line(&["and", "continues", "flush."], 0.10, 0.31));
        let paras: Vec<String> = paragraphs(&lines(&words))
            .iter()
            .map(|p| join_words(p, &FxHashSet::default()))
            .collect();
        assert_eq!(
            paras,
            vec![
                "First paragraph line one and line two.",
                "Second paragraph.",
                "Third paragraph opens indented and continues flush.",
            ]
        );
    }

    #[test]
    fn hyphen_continuation_does_not_fake_an_indent() {
        // "develop- / ment of better software" — the continuation fragment
        // is part of the line's geometry, so merging must not make the
        // second line look indented and split the paragraph.
        let mut words = line(&["thanks", "mainly", "to", "the", "develop-"], 0.10, 0.10);
        words.extend(line(&["ment", "of", "better", "software", "development"], 0.10, 0.13));
        let paras: Vec<String> = paragraphs(&lines(&words))
            .iter()
            .map(|p| join_words(p, &vocab_of(&words)))
            .collect();
        assert_eq!(
            paras,
            vec!["thanks mainly to the development of better software development"]
        );
    }

    #[test]
    fn markdown_carries_page_markers() {
        let pages = vec![
            PageOcr { page: 1, words: line(&["Hello", "world."], 0.1, 0.1) },
            PageOcr { page: 2, words: vec![] },
        ];
        let md = markdown("some-doc", &pages);
        assert!(md.starts_with("# some-doc\n"));
        assert!(md.contains("<!-- page 1 -->\n\nHello world.\n"));
        assert!(md.contains("<!-- page 2 -->\n"));
    }
}
