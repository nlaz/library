//! OCR cleanup: run the optional `tools/clean-pages` helper (on-device
//! Apple Foundation Model) and apply its verified edits to the OCR words,
//! writing `data/clean/<doc>/page-NNNN.json` in the `PageOcr` schema.
//! Downstream (`read_pages`) prefers these over the raw OCR page by page.
//!
//! The helper proposes; this module disposes. Every edit is re-gated here —
//! exact anchor in the fused text, edit distance, boundary duplication —
//! so a divergent or misbehaving helper can void edits but never corrupt
//! text. Independently of the model, hyphenated line breaks are fused
//! deterministically (same rule as `textout`), which is what makes
//! "development" findable when the scan printed "develop- ment".

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use library_core::{FxHashSet, Word, tokenize};
use serde::Deserialize;

use crate::{PageOcr, Progress, ProgressFn, read_ocr, read_pages};

#[derive(Deserialize)]
struct Edit {
    original: String,
    corrected: String,
    verified: bool,
}

#[derive(Deserialize)]
struct PageEdits {
    #[allow(dead_code)]
    page: u32,
    edits: Vec<Edit>,
}

/// The helper binary: `$LIBRARY_CLEAN_TOOL`, or `tools/clean-pages/clean-pages`
/// relative to the current directory. `None` means "cleanup not installed".
pub fn clean_tool() -> Option<PathBuf> {
    let path = std::env::var_os("LIBRARY_CLEAN_TOOL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tools/clean-pages/clean-pages"));
    path.is_file().then_some(path)
}

/// Run the helper (if installed) and apply its edits. Returns the number of
/// pages that changed plus the doc's final pages (so callers don't re-read
/// the whole doc). Progress: `Progress::Clean` while the model runs.
pub fn clean_doc(data: &Path, doc: &str, progress: ProgressFn) -> Result<(usize, Vec<PageOcr>)> {
    let Some(tool) = clean_tool() else {
        progress(Progress::Log(
            "cleanup skipped: tools/clean-pages not built (see tools/build.sh)".into(),
        ));
        return Ok((0, read_pages(data, doc)?));
    };
    let edits_dir = data.join("edits").join(doc);

    let mut child = Command::new(&tool)
        .arg("--ocr-dir")
        .arg(data.join("ocr").join(doc))
        .arg("--out-dir")
        .arg(&edits_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning {}", tool.display()))?;
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line?;
        // "clean <done>/<total>"
        if let Some((done, total)) = line
            .strip_prefix("clean ")
            .and_then(|s| s.split_once('/'))
            .and_then(|(d, t)| Some((d.parse().ok()?, t.parse().ok()?)))
        {
            progress(Progress::Clean { done, total });
        }
    }
    let status = child.wait()?;
    if !status.success() {
        // model unavailable (Apple Intelligence off) or helper failure:
        // report and carry on with raw OCR — cleanup is best-effort
        progress(Progress::Log(format!(
            "cleanup skipped: {} exited {status}",
            tool.display()
        )));
        return Ok((0, read_pages(data, doc)?));
    }

    apply_edits(data, doc, progress)
}

/// Apply cached edits (`data/edits/<doc>`) to the raw OCR, writing changed
/// pages to `data/clean/<doc>`. Separated from the model run so edits can be
/// re-applied (e.g. after a gate change) without touching the model.
/// Returns the changed-page count plus every final page, fused and edited —
/// what `read_pages` would hand back, without re-reading the doc.
pub fn apply_edits(data: &Path, doc: &str, progress: ProgressFn) -> Result<(usize, Vec<PageOcr>)> {
    let pages = read_ocr(&data.join("ocr").join(doc))?;
    let edits_dir = data.join("edits").join(doc);
    let clean_dir = data.join("clean").join(doc);
    std::fs::create_dir_all(&clean_dir)?;

    let vocab: FxHashSet<String> = pages
        .iter()
        .flat_map(|p| p.words.iter())
        .flat_map(|w| tokenize(&w.t))
        .collect();

    let total = pages.len();
    let mut out_pages: Vec<PageOcr> = Vec::with_capacity(total);
    let mut changed = 0usize;
    let mut rejected: Vec<String> = Vec::new();
    let mut applied = 0usize;
    for page in pages {
        let mut words = fuse_hyphens(&page.words, &vocab);
        let fused = words.len() != page.words.len();

        let ef = edits_dir.join(format!("page-{:04}.json", page.page));
        let mut edited = false;
        if let Ok(bytes) = std::fs::read(&ef) {
            let pe: PageEdits = serde_json::from_slice(&bytes)
                .context(format!("bad edits json {}", ef.display()))?;
            for e in pe.edits {
                match apply(&mut words, &e) {
                    Ok(()) => {
                        edited = true;
                        applied += 1;
                    }
                    Err(why) => rejected.push(format!(
                        "p.{} '{}' -> '{}': {why}",
                        page.page, e.original, e.corrected
                    )),
                }
            }
        }

        let rec = PageOcr {
            page: page.page,
            words,
        };
        if fused || edited {
            let out = clean_dir.join(format!("page-{:04}.json", page.page));
            let tmp = out.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&rec)?)?;
            std::fs::rename(&tmp, &out)?;
            changed += 1;
        }
        out_pages.push(rec);
    }
    if !rejected.is_empty() {
        std::fs::write(edits_dir.join("rejected.log"), rejected.join("\n") + "\n")?;
    }
    progress(Progress::Log(format!(
        "cleanup: {applied} edits applied, {} rejected, {changed}/{total} pages changed",
        rejected.len(),
    )));
    Ok((changed, out_pages))
}

/// Fuse hyphenated line breaks in the word list itself (the helper saw the
/// same fusion, so edit anchors line up). The merged word keeps the first
/// fragment's box: fragments sit on different lines, and a union box would
/// derail the line grouping in `textout`.
fn fuse_hyphens(words: &[Word], vocab: &FxHashSet<String>) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::with_capacity(words.len());
    for w in words {
        if let Some(prev) = out.last_mut()
            && prev.t.len() > 1
            && prev.t.ends_with('-')
            && !prev.t.ends_with("--")
            && w.t.chars().next().is_some_and(|c| c.is_lowercase())
        {
            let fused = format!("{}{}", &prev.t[..prev.t.len() - 1], w.t);
            let known = tokenize(&fused).first().is_some_and(|t| vocab.contains(t));
            prev.t = if known {
                fused
            } else {
                format!("{}{}", prev.t, w.t)
            };
            continue;
        }
        out.push(w.clone());
    }
    out
}

/// Levenshtein over bytes — the inputs are short ASCII-ish tokens.
fn distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (row[j] + 1).min(cur + 1).min(prev + usize::from(ca != cb));
            prev = cur;
        }
    }
    row[b.len()]
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| *c != ' ' && *c != '-').collect()
}

/// Apply one edit to the fused words, or say why not. The original must
/// match a whole-word span exactly; the span's words collapse into one
/// carrying the corrected text and the first word's box.
fn apply(words: &mut Vec<Word>, e: &Edit) -> Result<(), &'static str> {
    if !e.verified {
        return Err("unverified");
    }
    if e.original == e.corrected || e.original.is_empty() || e.corrected.is_empty() {
        return Err("noop");
    }
    if e.original.len() > 40 || e.corrected.len() > 48 {
        return Err("too long");
    }
    // anchor: the original as a whole-word span. The model quotes bare
    // words but OCR tokens keep their punctuation ("preadsheets,"), so the
    // span's edge words may carry extra leading/trailing punctuation, which
    // the replacement preserves. Middle words must match exactly.
    let toks: Vec<&str> = e.original.split_whitespace().collect();
    let n = toks.len();
    let edge_match = |w: &str, t: &str, first: bool, last: bool| -> Option<(String, String)> {
        if w == t {
            return Some((String::new(), String::new()));
        }
        let core = w.trim_start_matches(|c: char| !c.is_alphanumeric());
        let prefix = &w[..w.len() - core.len()];
        let core = core.trim_end_matches(|c: char| !c.is_alphanumeric());
        let suffix = &w[prefix.len() + core.len()..];
        ((first || prefix.is_empty()) && (last || suffix.is_empty()) && core == t)
            .then(|| (prefix.to_string(), suffix.to_string()))
    };
    let (start, prefix, suffix) = (0..words.len().saturating_sub(n - 1))
        .find_map(|i| {
            let (prefix, _) = edge_match(&words[i].t, toks[0], true, n == 1)?;
            let (_, suffix) = edge_match(&words[i + n - 1].t, toks[n - 1], n == 1, true)?;
            (1..n.saturating_sub(1))
                .all(|j| words[i + j].t == toks[j])
                .then_some((i, prefix, suffix))
        })
        .ok_or("no anchor")?;

    let (o, c) = (squash(&e.original), squash(&e.corrected));
    if o != c && distance(&o, &c) > 2.max(o.len() / 4) {
        return Err("edit distance");
    }

    // boundary duplication: "a victim" -> "a victim of" where the next word
    // already is "of" would yield "of of"
    let extra: Vec<&str> = e.corrected.split_whitespace().collect();
    if extra.len() > n
        && extra[..n] == toks[..]
        && words.get(start + n).is_some_and(|w| w.t == extra[n])
    {
        return Err("duplicates next word");
    }
    if extra.len() > n
        && extra[extra.len() - n..] == toks[..]
        && start > 0
        && words[start - 1].t == extra[0]
    {
        return Err("duplicates previous word");
    }

    words[start].t = format!("{prefix}{}{suffix}", e.corrected);
    words.drain(start + 1..start + n);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str) -> Word {
        Word {
            t: t.into(),
            x: 0.1,
            y: 0.1,
            w: 0.04,
            h: 0.02,
        }
    }

    fn words(ts: &[&str]) -> Vec<Word> {
        ts.iter().map(|t| w(t)).collect()
    }

    fn edit(o: &str, c: &str) -> Edit {
        Edit {
            original: o.into(),
            corrected: c.into(),
            verified: true,
        }
    }

    #[test]
    fn applies_single_and_multi_word_edits() {
        let mut ws = words(&[
            "the",
            "creation",
            "of",
            "preadsheets",
            "and",
            "impor",
            "tant",
            "things",
        ]);
        apply(&mut ws, &edit("preadsheets", "spreadsheets")).unwrap();
        apply(&mut ws, &edit("impor tant", "important")).unwrap();
        let ts: Vec<&str> = ws.iter().map(|w| w.t.as_str()).collect();
        assert_eq!(
            ts,
            vec![
                "the",
                "creation",
                "of",
                "spreadsheets",
                "and",
                "important",
                "things"
            ]
        );
    }

    #[test]
    fn rejects_unverified_unanchored_and_rewrites() {
        let mut ws = words(&["plain", "text"]);
        let mut e = edit("plain", "plane");
        e.verified = false;
        assert_eq!(apply(&mut ws, &e), Err("unverified"));
        assert_eq!(
            apply(&mut ws, &edit("missing", "present")),
            Err("no anchor")
        );
        // "text" -> something entirely different: distance gate
        assert_eq!(
            apply(&mut ws, &edit("text", "manuscript")),
            Err("edit distance")
        );
        // partial-word matches must not anchor ("plain" != "plai")
        assert_eq!(apply(&mut ws, &edit("plai", "play")), Err("no anchor"));
    }

    #[test]
    fn anchors_through_edge_punctuation() {
        // OCR tokens keep punctuation the model's quote drops
        let mut ws = words(&["creation", "of", "preadsheets,", "word", "processors"]);
        apply(&mut ws, &edit("preadsheets", "spreadsheets")).unwrap();
        assert_eq!(ws[2].t, "spreadsheets,");

        let mut ws = words(&["said", "\"impor", "tant\"", "here"]);
        apply(&mut ws, &edit("impor tant", "important")).unwrap();
        let ts: Vec<&str> = ws.iter().map(|w| w.t.as_str()).collect();
        assert_eq!(ts, vec!["said", "\"important\"", "here"]);

        // punctuation strictly inside the span still blocks the anchor
        let mut ws = words(&["impor,", "tant"]);
        assert_eq!(
            apply(&mut ws, &edit("impor tant", "important")),
            Err("no anchor")
        );
    }

    #[test]
    fn rejects_boundary_duplication() {
        let mut ws = words(&[
            "folded", "in", "1979,", "a", "victim", "of", "its", "strategy",
        ]);
        assert_eq!(
            apply(&mut ws, &edit("a victim", "a victim of")),
            Err("duplicates next word")
        );
        assert_eq!(
            apply(&mut ws, &edit("victim of", "a victim of")),
            Err("duplicates previous word")
        );
    }

    #[test]
    fn fuses_hyphens_with_vocab_rule() {
        let ws = words(&["develop-", "ment", "Apple-", "compatible", "development"]);
        let vocab: FxHashSet<String> = ws.iter().flat_map(|w| tokenize(&w.t)).collect();
        let fused = fuse_hyphens(&ws, &vocab);
        let ts: Vec<&str> = fused.iter().map(|w| w.t.as_str()).collect();
        assert_eq!(ts, vec!["development", "Apple-compatible", "development"]);
    }

    #[test]
    fn fuse_hyphens_vocab_hit_fuses() {
        // "automatic" is in the vocab, so the hyphen is dropped and the two
        // OCR tokens fuse into one clean word.
        let ws = words(&["auto-", "matic"]);
        let vocab: FxHashSet<String> = ["automatic".to_string()].into_iter().collect();
        let fused = fuse_hyphens(&ws, &vocab);
        let ts: Vec<&str> = fused.iter().map(|w| w.t.as_str()).collect();
        assert_eq!(ts, vec!["automatic"]);
    }

    #[test]
    fn fuse_hyphens_vocab_miss_merges_but_keeps_hyphen() {
        // Surprising current behavior, pinned here: when the fused spelling
        // isn't in the vocab, `fuse_hyphens` does NOT leave the two OCR
        // tokens as separate words. It still merges them into a single
        // `Word`, just keeping the hyphen literally ("cats-dogs") instead of
        // producing the clean fusion ("catsdogs"). So an unknown fusion
        // collapses the word count exactly like a known one does — it just
        // spells the merged word differently.
        let ws = words(&["cats-", "dogs"]);
        let vocab: FxHashSet<String> = ["cats".to_string(), "dogs".to_string()]
            .into_iter()
            .collect();
        let fused = fuse_hyphens(&ws, &vocab);
        let ts: Vec<&str> = fused.iter().map(|w| w.t.as_str()).collect();
        assert_eq!(ts, vec!["cats-dogs"]);
    }

    #[test]
    fn fuse_hyphens_trailing_hyphen_at_end_of_input() {
        // A hyphenated word with nothing after it has no candidate to fuse
        // with, so it passes through unchanged.
        let ws = words(&["hello", "world-"]);
        let vocab: FxHashSet<String> = FxHashSet::default();
        let fused = fuse_hyphens(&ws, &vocab);
        let ts: Vec<&str> = fused.iter().map(|w| w.t.as_str()).collect();
        assert_eq!(ts, vec!["hello", "world-"]);
    }

    #[test]
    fn distance_exact_on_small_pairs() {
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("abc", "abc"), 0);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("abc", ""), 3);
        assert_eq!(distance("flaw", "lawn"), 2);
    }

    #[test]
    fn distance_is_symmetric() {
        let pairs = [
            ("kitten", "sitting"),
            ("preadsheets", "spreadsheets"),
            ("", "x"),
            ("same", "same"),
            ("abcdef", "fedcba"),
        ];
        for (a, b) in pairs {
            assert_eq!(distance(a, b), distance(b, a), "distance({a:?}, {b:?})");
        }
    }

    #[test]
    fn distance_has_no_internal_bound_or_early_exit() {
        // Despite the "bounded ... with early exit" framing one might expect
        // from a fast approximate-match helper, `distance` as written takes
        // no bound parameter and computes the full Levenshtein matrix every
        // time — there's no sentinel/cap value it can return. Any bounding
        // happens at the call site in `apply`, which thresholds the result
        // against `2.max(o.len() / 4)`. This pins the current (unbounded)
        // behavior: two completely dissimilar equal-length strings cost one
        // substitution per character, exactly `len`, not a capped value.
        assert_eq!(distance("abcdefghij", "zyxwvutsrq"), 10);
    }

    #[test]
    fn squash_removes_all_spaces_and_hyphens() {
        // `squash` isn't a whitespace-run collapse — it deletes every literal
        // ' ' and '-' character outright (runs, single occurrences, leading,
        // and trailing all disappear entirely rather than collapsing to one
        // space).
        assert_eq!(squash("a  b"), "ab");
        assert_eq!(squash("a-b"), "ab");
        assert_eq!(squash("multi-word-hyphen"), "multiwordhyphen");
        assert_eq!(squash("  leading and trailing  "), "leadingandtrailing");
    }

    #[test]
    fn squash_leaves_other_whitespace_untouched() {
        // Only ' ' and '-' are filtered; tabs/newlines are not whitespace-
        // normalized and pass straight through unchanged.
        assert_eq!(squash("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn squash_empty_string_is_empty() {
        assert_eq!(squash(""), "");
    }
}
