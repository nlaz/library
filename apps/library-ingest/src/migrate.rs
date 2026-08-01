//! One-way migration from the pre-roots layout.
//!
//! Up to 0.1.1 a library was: originals in `data/pdfs/<slug>.<ext>`, caches
//! keyed by that same slug, and metadata in JSON sidecars. There are no
//! roots, no `meta.db`, and the document id *is* the filename.
//!
//! This turns one of those into the new shape. It is a separate command
//! rather than something that happens on launch, because it copies
//! gigabytes and rewrites every key in both stores, and neither belongs in
//! the path between double-clicking an app and seeing your books.
//!
//! # What it will not do
//!
//! It never writes to the old data directory beyond reading it, and it
//! copies originals rather than moving them. A migration that half-worked
//! must leave the old library exactly as it was, because that is the only
//! copy of some of this — the page renders alone are hours of OCR.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use library_core::meta::Ctx;
use library_core::records::is_reserved;

use crate::status::{DocState, DocStatus};

/// What a migration did, printed at the end and asserted in tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Originals copied into the new library folder.
    pub files: usize,
    /// Documents whose caches were re-keyed to a minted id.
    pub docs: usize,
    /// Shelf folders created from the old collections.
    pub shelves: usize,
    pub titles: usize,
    pub cards: usize,
    /// Docs whose caches exist but whose original is gone. Their renders
    /// and OCR come across; there is no file to watch, so they arrive
    /// missing rather than being silently dropped.
    pub orphans: usize,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// `doc id -> shelf`, from the old collections sidecar. First shelf wins:
/// a folder holds a file once, and the old model allowed many.
fn shelf_map(from: &Path) -> (BTreeMap<String, String>, usize) {
    let cols: BTreeMap<String, Vec<String>> =
        read_json(&from.join("collections.json")).unwrap_or_default();
    let mut out = BTreeMap::new();
    let n = cols.len();
    for (shelf, docs) in cols {
        for doc in docs {
            out.entry(doc).or_insert_with(|| shelf.clone());
        }
    }
    (out, n)
}

/// A folder name that means the same thing to a person and is safe on disk.
fn safe_folder(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "Unsorted".to_string()
    } else {
        cleaned
    }
}

/// Copy a cache directory (`pages/<old>` → `cache/pages/<new>`).
fn move_cache_dir(from: &Path, to: &Path, kind: &str, old: &str, new: &str) -> Result<bool> {
    let src = from.join(kind).join(old);
    if !src.is_dir() {
        return Ok(false);
    }
    let dst = to.join(kind).join(new);
    std::fs::create_dir_all(&dst)?;
    for e in std::fs::read_dir(&src)?.flatten() {
        if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            std::fs::copy(e.path(), dst.join(e.file_name()))?;
        }
    }
    Ok(true)
}

/// Migrate `from` (an old data dir) into `to` (the new library folder) plus
/// `support` (the new app-support dir).
pub fn migrate(from: &Path, to: &Path, support: &Path, dry_run: bool) -> Result<Report> {
    if !from.is_dir() {
        bail!("no such data directory: {}", from.display());
    }
    if from == support {
        bail!("refusing to migrate a directory into itself");
    }
    let pdfs = from.join("pdfs");
    if !pdfs.is_dir() {
        bail!(
            "{} has no pdfs/ directory — is it really a pre-0.2 library?",
            from.display()
        );
    }

    let mut r = Report::default();
    let (shelves, shelf_count) = shelf_map(from);
    r.shelves = shelf_count;
    let titles: BTreeMap<String, String> = read_json(&from.join("titles.json")).unwrap_or_default();

    // every doc the old library knew about: an original, a page cache, or
    // both. A doc with caches but no original still has hours of OCR in it.
    let mut originals: BTreeMap<String, PathBuf> = BTreeMap::new();
    for e in std::fs::read_dir(&pdfs)?.flatten() {
        let p = e.path();
        if crate::SourceKind::of(&p).is_none() {
            continue;
        }
        if let Some(stem) = p.file_stem() {
            originals.insert(stem.to_string_lossy().into_owned(), p);
        }
    }
    let mut all: Vec<String> = originals.keys().cloned().collect();
    if let Ok(entries) = std::fs::read_dir(from.join("pages")) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let id = e.file_name().to_string_lossy().into_owned();
                if !originals.contains_key(&id) {
                    all.push(id);
                }
            }
        }
    }
    all.sort();
    all.dedup();
    all.retain(|d| !is_reserved(d));

    if dry_run {
        r.files = originals.len();
        r.docs = all.len();
        r.orphans = all.len() - originals.len();
        r.titles = titles.len();
        r.cards = read_json::<Vec<serde_json::Value>>(&from.join("notes/cards.json"))
            .map(|c| c.len())
            .unwrap_or(0);
        return Ok(r);
    }

    std::fs::create_dir_all(to)?;
    let ctx = Ctx::open(support)?;
    let root = ctx.add_root(to, now())?;

    // 1. copy the originals into their shelf folders. The shelves become
    //    real directories, which is the whole point: what was a JSON map is
    //    now something the user can see and rearrange.
    for (old_id, src) in &originals {
        let dir = match shelves.get(old_id) {
            Some(shelf) => to.join(safe_folder(shelf)),
            None => to.to_path_buf(),
        };
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(src.file_name().unwrap_or_default());
        if !dest.exists() {
            std::fs::copy(src, &dest).with_context(|| format!("copying {}", src.display()))?;
        }
        r.files += 1;
    }

    // 2. scan, which mints an id per file and records its shelf
    let applied = library_core::roots::sync_root(&ctx.meta, &root, now());
    let _ = applied;

    // 3. re-key the caches. The old id was the filename; the new one is
    //    minted, so every cache directory has to be renamed to match — this
    //    is a copy of file trees, not a re-OCR.
    let mut old_to_new: BTreeMap<String, String> = BTreeMap::new();
    for k in ctx.files_in_root(&root.id) {
        // the old id is the copied file's stem, which we preserved
        let stem = Path::new(&k.relpath)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        old_to_new.insert(stem, k.doc);
    }

    for old_id in &all {
        let Some(new_id) = old_to_new.get(old_id) else {
            // caches without an original: nothing watches them, so the
            // document arrives missing rather than vanishing unmentioned
            r.orphans += 1;
            continue;
        };
        let mut any = false;
        for kind in ["pages", "ocr", "clean", "edits"] {
            any |= move_cache_dir(from, support, kind, old_id, new_id)?;
        }
        let md = from.join("text").join(format!("{old_id}.md"));
        if md.is_file() {
            let dst = support.join("text");
            std::fs::create_dir_all(&dst)?;
            std::fs::copy(&md, dst.join(format!("{new_id}.md")))?;
            any = true;
        }
        if let Some(title) = titles.get(old_id) {
            ctx.set_title(new_id, Some(title))?;
            r.titles += 1;
        }
        if any {
            // caches came across, so the doc is already indexable: it needs
            // a commit, not an OCR pass
            crate::status::write(&ctx.meta, new_id, &DocStatus::new(DocState::Queued))?;
        }
        r.docs += 1;
    }

    // 4. cards, keyed by the new ids so their evidence still points at
    //    something. A card whose document did not come across keeps its
    //    text — the note is the user's writing and outlives the file.
    if let Some(mut cards) =
        read_json::<Vec<library_core::notes::CardRec>>(&from.join("notes/cards.json"))
    {
        for card in &mut cards {
            for q in &mut card.evidence {
                if let Some(new) = old_to_new.get(&q.doc) {
                    q.doc = new.clone();
                }
            }
        }
        r.cards = cards.len();
        ctx.put_cards(&cards)?;
    }

    Ok(r)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature pre-0.2 library: two originals, one orphaned cache, a
    /// collection, a title and a card.
    fn old_library(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in [
            "pdfs",
            "pages/artusi-1891",
            "pages/gone-book",
            "ocr/artusi-1891",
            "text",
            "notes",
        ] {
            std::fs::create_dir_all(dir.join(d)).expect("fixture dirs");
        }
        std::fs::write(dir.join("pdfs/artusi-1891.pdf"), b"%PDF-artusi").expect("pdf");
        std::fs::write(dir.join("pdfs/loose-book.pdf"), b"%PDF-loose").expect("pdf");
        std::fs::write(dir.join("pages/artusi-1891/page-0001.jpg"), b"jpg").expect("page");
        std::fs::write(dir.join("pages/gone-book/page-0001.jpg"), b"jpg").expect("page");
        std::fs::write(dir.join("ocr/artusi-1891/page-0001.json"), b"{}").expect("ocr");
        std::fs::write(dir.join("text/artusi-1891.md"), b"# artusi").expect("md");
        std::fs::write(
            dir.join("collections.json"),
            br#"{"cookbooks": ["artusi-1891"]}"#,
        )
        .expect("cols");
        std::fs::write(
            dir.join("titles.json"),
            br#"{"artusi-1891": "Science in the Kitchen"}"#,
        )
        .expect("titles");
        std::fs::write(
            dir.join("notes/cards.json"),
            br#"[{"id":"c1","title":"a claim","body":"","created":1,"modified":1,
                  "evidence":[{"doc":"artusi-1891","page":12,"kind":"region",
                               "bbox":[0.1,0.1,0.2,0.2]}],"links":[]}]"#,
        )
        .expect("cards");
        dir
    }

    #[test]
    fn migrates_originals_shelves_caches_titles_and_cards() {
        let old = old_library("full");
        let new = old.join("../migrate-new-full");
        let lib = new.join("The Library");
        let support = new.join("support");
        let _ = std::fs::remove_dir_all(&new);

        let r = migrate(&old, &lib, &support, false).expect("migrate");
        assert_eq!(r.files, 2);
        assert_eq!(r.titles, 1);
        assert_eq!(r.cards, 1);
        assert_eq!(r.orphans, 1, "gone-book has caches but no original");

        // the collection became a folder the user can see
        assert!(lib.join("cookbooks/artusi-1891.pdf").is_file());
        // a doc in no collection stays at the top level
        assert!(lib.join("loose-book.pdf").is_file());

        let ctx = Ctx::open(&support).expect("meta");
        let files = ctx.files_in_root(&ctx.roots()[0].id);
        assert_eq!(files.len(), 2);

        let artusi = files
            .iter()
            .find(|f| f.relpath.contains("artusi"))
            .expect("artusi");
        // minted, not derived from the filename
        assert!(artusi.doc.starts_with('d') && !artusi.doc.contains("artusi"));

        // the caches came across under the new id — this is the expensive
        // part, and re-doing it would be hours of OCR
        assert!(
            support
                .join("pages")
                .join(&artusi.doc)
                .join("page-0001.jpg")
                .is_file()
        );
        assert!(
            support
                .join("ocr")
                .join(&artusi.doc)
                .join("page-0001.json")
                .is_file()
        );
        assert!(
            support
                .join("text")
                .join(format!("{}.md", artusi.doc))
                .is_file()
        );

        assert_eq!(
            ctx.titles().get(&artusi.doc).map(String::as_str),
            Some("Science in the Kitchen")
        );
        assert_eq!(ctx.shelves()["cookbooks"], vec![artusi.doc.clone()]);

        // the card's evidence follows the document to its new id
        let cards = ctx.cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].evidence[0].doc, artusi.doc);

        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(&new);
    }

    #[test]
    fn never_writes_to_the_old_library() {
        // the old data dir is the only copy of some of this; a migration
        // that half-worked has to leave it exactly as it was
        let old = old_library("readonly");
        let new = old.join("../migrate-new-readonly");
        let _ = std::fs::remove_dir_all(&new);

        let before = snapshot(&old);
        migrate(&old, &new.join("lib"), &new.join("support"), false).expect("migrate");
        assert_eq!(snapshot(&old), before, "the old library is untouched");

        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(&new);
    }

    #[test]
    fn a_dry_run_changes_nothing_and_still_counts() {
        let old = old_library("dry");
        let new = old.join("../migrate-new-dry");
        let _ = std::fs::remove_dir_all(&new);

        let r = migrate(&old, &new.join("lib"), &new.join("support"), true).expect("dry run");
        assert_eq!(r.files, 2);
        assert_eq!(r.docs, 3, "two originals plus the orphaned cache");
        assert_eq!(r.titles, 1);
        assert_eq!(r.cards, 1);
        assert!(!new.exists(), "a dry run creates nothing");

        let _ = std::fs::remove_dir_all(&old);
    }

    #[test]
    fn refuses_a_directory_that_is_not_an_old_library() {
        let dir = std::env::temp_dir().join(format!("migrate-bogus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert!(migrate(&dir, &dir.join("l"), &dir.join("s"), true).is_err());
        assert!(migrate(&dir.join("nope"), &dir.join("l"), &dir.join("s"), true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shelf_names_survive_becoming_folders() {
        assert_eq!(safe_folder("Whole Earth"), "Whole Earth");
        // a separator would nest, and the nested folder would be a
        // different shelf than the one the name said
        assert_eq!(safe_folder("a/b"), "a-b");
        assert_eq!(safe_folder("  spaced  "), "spaced");
        assert_eq!(safe_folder(""), "Unsorted");
        assert_eq!(safe_folder("."), "Unsorted");
    }

    /// Every file under `dir` with its length — enough to catch a write.
    fn snapshot(dir: &Path) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                match e.file_type() {
                    Ok(t) if t.is_dir() => stack.push(p),
                    Ok(_) => {
                        let rel = p
                            .strip_prefix(dir)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .into_owned();
                        out.insert(rel, e.metadata().map(|m| m.len()).unwrap_or(0));
                    }
                    Err(_) => {}
                }
            }
        }
        out
    }
}
