//! Keeping `data/pages` inside a budget.
//!
//! Page renders are derived: a 1600px JPEG of a page that already exists,
//! in a file the user keeps. On a real library they run about five times
//! the size of the sources they came from — 6.6 GiB against 1.2 GiB — which
//! is a lot of disk to spend on something re-creatable in ~160ms.
//!
//! So they are a cache, and this is what bounds it. The rule that matters
//! is not "evict the least recently read" but the one above it:
//!
//!   **A render is only cache if it can be made again.**
//!
//! Two documents in the author's own library are `ready`, readable, and
//! have no row in `files` at all — the scanner has never seen a file for
//! them. Their page images are not a cache of anything; they are the only
//! surviving rendering of those books, 282 MiB of it. An LRU that assumes
//! every `page-*.jpg` is re-derivable would delete them, and nothing could
//! bring them back. So the sweep asks [`source_state`] first and pins
//! anything it cannot re-render, budget or no budget.

use std::path::Path;

use library_core::meta::Ctx;
use library_ingest::status::{self, DocState};
use library_ingest::{Source, source_state};

/// Pref key holding the budget, in bytes, as a decimal string. `"0"` means
/// no limit.
pub(crate) const BUDGET_KEY: &str = "cache.pages.budget_bytes";
/// Pref key recording that the user has been told the cache exists.
pub(crate) const ANNOUNCED_KEY: &str = "cache.pages.announced";

/// Default budget. Comfortably more than a working set of a few open books,
/// and small enough to be worth having on a laptop.
pub(crate) const DEFAULT_BUDGET: u64 = 4 * 1024 * 1024 * 1024;

/// Below this a working set thrashes: every search re-renders what the last
/// one evicted, and the app feels broken while doing exactly what it was
/// told. Refuse rather than obey.
pub(crate) const MIN_BUDGET: u64 = 1024 * 1024 * 1024;

/// Evict down to this fraction of the budget, not to the budget itself.
/// Stopping exactly at the line means the next render crosses it again and
/// the sweep runs forever, trading disk for a permanent stream of renders.
const LOW_WATER: f64 = 0.9;

/// What a sweep may do with one document's renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Mid-ingest. Deleting a JPEG the OCR pass just wrote would make its
    /// own skip predicate lie about what still needs doing.
    Busy,
    /// The document the reader has open. Evicting underneath someone's eyes
    /// is the one moment the re-render latency is guaranteed to be seen.
    Open,
    /// Cannot be re-rendered: no file row, the file is missing, or it is an
    /// iCloud stub. **Never evicted.** Not a cache — the last copy.
    Pinned,
    /// Retracted, and its file is still there. Unreachable from the UI and
    /// fully re-derivable, so it goes regardless of the budget.
    Dead,
    /// Ordinary cache. Evicted least-recently-read first, under pressure.
    Live,
}

/// One document's renders, as the sweep sees them.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub doc: String,
    pub bytes: u64,
    pub last_read: u64,
    pub class: Class,
}

/// What a sweep did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    pub freed: u64,
    pub docs: Vec<String>,
    /// Total render bytes before the sweep.
    pub before: u64,
    /// Bytes held by documents that cannot be re-rendered.
    pub pinned: u64,
}

/// Read the budget, clamped. An unparseable value reads as "no limit":
/// a setting we cannot understand must never be taken as permission to
/// delete everything.
pub(crate) fn budget(ctx: &Ctx) -> u64 {
    match ctx.setting(BUDGET_KEY) {
        None => DEFAULT_BUDGET,
        Some(s) => match s.trim().parse::<u64>() {
            Ok(0) => 0,
            Ok(n) => n.max(MIN_BUDGET),
            Err(_) => 0,
        },
    }
}

/// Total bytes of `page-*.jpg` under a document's directory. Covers are
/// deliberately not counted: they are on the floor and never evicted, so
/// including them would promise space the sweep cannot free.
fn page_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("page-") && n.ends_with(".jpg")
        })
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Classify every document with renders on disk.
pub(crate) fn survey(ctx: &Ctx, open_doc: Option<&str>) -> Vec<Entry> {
    let statuses = status::scan(ctx);
    let recency: std::collections::HashMap<String, u64> = ctx.read_order().into_iter().collect();
    let pages = ctx.data.join("pages");

    let Ok(rd) = std::fs::read_dir(&pages) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let doc = e.file_name().to_string_lossy().into_owned();
        let bytes = page_bytes(&e.path());
        let state = statuses.get(&doc).map(|s| s.state);

        // Order matters, and this is the order: never touch work in
        // progress, never touch what is being read, never touch what cannot
        // be remade — and only then ask whether it is wanted.
        let busy = matches!(
            state,
            Some(DocState::Queued | DocState::Preparing | DocState::Staged)
        ) || library_ingest::worker::claimed(&ctx.data, &doc);
        let class = if busy {
            Class::Busy
        } else if open_doc == Some(doc.as_str()) {
            Class::Open
        } else if !matches!(source_state(ctx, &doc), Source::Ready(_)) {
            Class::Pinned
        } else if state == Some(DocState::Deleted) {
            Class::Dead
        } else {
            Class::Live
        };

        out.push(Entry {
            doc: doc.clone(),
            bytes,
            last_read: recency.get(&doc).copied().unwrap_or(0),
            class,
        });
    }
    out
}

/// Decide what to evict. Pure, so the policy can be tested without a
/// filesystem — and it is the part worth being sure about.
pub(crate) fn plan(entries: &[Entry], budget: u64) -> Sweep {
    let before: u64 = entries.iter().map(|e| e.bytes).sum();
    let pinned: u64 = entries
        .iter()
        .filter(|e| e.class == Class::Pinned)
        .map(|e| e.bytes)
        .sum();

    let mut sweep = Sweep {
        freed: 0,
        docs: Vec::new(),
        before,
        pinned,
    };

    // Retracted documents go whatever the budget says: nothing can reach
    // them, and every byte is re-derivable.
    for e in entries.iter().filter(|e| e.class == Class::Dead) {
        if e.bytes > 0 {
            sweep.freed += e.bytes;
            sweep.docs.push(e.doc.clone());
        }
    }
    if budget == 0 {
        return sweep; // no limit: dead documents only
    }

    // Everything still on disk after the dead are gone.
    let mut held: u64 = before - sweep.freed;
    let target = (budget as f64 * LOW_WATER) as u64;
    if held <= target {
        return sweep;
    }

    let mut live: Vec<&Entry> = entries.iter().filter(|e| e.class == Class::Live).collect();
    // least recently read first; ties broken by size so a sweep that must
    // choose frees more with fewer victims
    live.sort_by(|a, b| {
        a.last_read
            .cmp(&b.last_read)
            .then(b.bytes.cmp(&a.bytes))
            .then(a.doc.cmp(&b.doc))
    });
    for e in live {
        if held <= target {
            break;
        }
        if e.bytes == 0 {
            continue;
        }
        held -= e.bytes;
        sweep.freed += e.bytes;
        sweep.docs.push(e.doc.clone());
    }
    sweep
}

/// Delete a document's page renders. Never the directory, and never the
/// cover: the cover is on the floor, and keeping the directory avoids
/// racing `create_dir_all` in the render path.
fn evict(pages: &Path, doc: &str) {
    let dir = pages.join(doc);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("page-") && name.ends_with(".jpg") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Survey, plan, and — unless `dry_run` — carry it out.
pub(crate) fn sweep(ctx: &Ctx, open_doc: Option<&str>, dry_run: bool) -> Sweep {
    let entries = survey(ctx, open_doc);
    let plan = plan(&entries, budget(ctx));
    if !dry_run {
        let pages = ctx.data.join("pages");
        for doc in &plan.docs {
            evict(&pages, doc);
        }
    }
    plan
}

/// Path a document's renders live under — for tests and callers that want
/// to check the sweep's work.
#[cfg(test)]
fn pages_of(ctx: &Ctx, doc: &str) -> std::path::PathBuf {
    ctx.data.join("pages").join(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(doc: &str, bytes: u64, last_read: u64, class: Class) -> Entry {
        Entry {
            doc: doc.into(),
            bytes,
            last_read,
            class,
        }
    }

    const GB: u64 = 1024 * 1024 * 1024;

    // THE test. Two documents in the author's library are ready, readable,
    // and have no source file at all — their renders are the only surviving
    // copy of those books. A budget of zero must not touch them, and
    // neither must a budget of anything else.
    #[test]
    fn a_doc_whose_source_is_gone_keeps_every_page() {
        let entries = vec![
            entry("lost", 200, 0, Class::Pinned),
            entry("fine", 200, 0, Class::Live),
        ];
        for budget in [1, MIN_BUDGET, DEFAULT_BUDGET] {
            let s = plan(&entries, budget);
            assert!(
                !s.docs.contains(&"lost".to_string()),
                "pinned doc evicted at budget {budget}"
            );
        }
        assert_eq!(plan(&entries, 1).pinned, 200);
    }

    #[test]
    fn retracted_docs_go_regardless_of_the_budget() {
        let entries = vec![
            entry("gone", 500, 0, Class::Dead),
            entry("kept", 500, 0, Class::Live),
        ];
        // budget 0 = no limit, and still the tombstone's renders go
        let s = plan(&entries, 0);
        assert_eq!(s.docs, vec!["gone"]);
        assert_eq!(s.freed, 500);
    }

    #[test]
    fn nothing_is_evicted_under_budget() {
        let entries = vec![entry("a", 100, 0, Class::Live)];
        assert_eq!(plan(&entries, DEFAULT_BUDGET).docs, Vec::<String>::new());
    }

    // Stopping exactly at the budget means the next render crosses it and
    // the sweep runs again, forever. The watermark is what makes eviction
    // converge instead of oscillate.
    #[test]
    fn eviction_stops_at_the_low_watermark_not_the_budget() {
        let budget = 10_000u64;
        let entries: Vec<Entry> = (0..10)
            .map(|i| entry(&format!("d{i}"), 1_000, i as u64, Class::Live))
            .collect();
        let s = plan(&entries, budget);
        let held = 10_000 - s.freed;
        assert!(held <= 9_000, "held {held} is above the watermark");
        assert!(held > 8_000, "over-evicted down to {held}");
    }

    #[test]
    fn least_recently_read_goes_first() {
        let entries = vec![
            entry("fresh", 6_000, 900, Class::Live),
            entry("stale", 6_000, 100, Class::Live),
        ];
        let s = plan(&entries, 10_000);
        assert_eq!(s.docs, vec!["stale"]);
    }

    // An unread document sorts ahead of everything read: its renders have
    // never been looked at, so they are the cheapest thing to lose.
    #[test]
    fn never_opened_is_the_first_to_go() {
        let entries = vec![
            entry("read-once", 6_000, 1, Class::Live),
            entry("never-read", 6_000, 0, Class::Live),
        ];
        assert_eq!(plan(&entries, 10_000).docs, vec!["never-read"]);
    }

    #[test]
    fn busy_and_open_docs_are_never_swept() {
        let entries = vec![
            entry("ingesting", 9_000, 0, Class::Busy),
            entry("reading", 9_000, 0, Class::Open),
        ];
        let s = plan(&entries, MIN_BUDGET.min(1_000));
        assert!(s.docs.is_empty(), "swept {:?}", s.docs);
    }

    #[test]
    fn budget_clamps_and_distrusts_nonsense() {
        let dir = std::env::temp_dir().join(format!("cache-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ctx = Ctx::in_memory(&dir).unwrap();

        assert_eq!(budget(&ctx), DEFAULT_BUDGET, "unset means the default");

        ctx.set_setting(BUDGET_KEY, "0").unwrap();
        assert_eq!(budget(&ctx), 0, "0 means no limit");

        ctx.set_setting(BUDGET_KEY, "5").unwrap();
        assert_eq!(budget(&ctx), MIN_BUDGET, "below the floor is clamped up");

        ctx.set_setting(BUDGET_KEY, &(8 * GB).to_string()).unwrap();
        assert_eq!(budget(&ctx), 8 * GB);

        // a value we cannot read must not be taken as licence to delete
        ctx.set_setting(BUDGET_KEY, "banana").unwrap();
        assert_eq!(budget(&ctx), 0);
    }

    /// A data dir with renders on disk for each named doc.
    fn tree(name: &str, docs: &[(&str, Option<DocState>, u32)]) -> Ctx {
        let dir = std::env::temp_dir().join(format!("cache-survey-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ctx = Ctx::in_memory(&dir).unwrap();
        for (doc, state, pages) in docs {
            let p = dir.join("pages").join(doc);
            std::fs::create_dir_all(&p).unwrap();
            let o = dir.join("ocr").join(doc);
            std::fs::create_dir_all(&o).unwrap();
            for i in 1..=*pages {
                std::fs::write(p.join(format!("page-{i:04}.jpg")), vec![0u8; 512]).unwrap();
                std::fs::write(o.join(format!("page-{i:04}.json")), b"{}").unwrap();
            }
            if let Some(st) = state {
                status::write(&ctx.meta, doc, &status::DocStatus::new(*st)).unwrap();
            }
        }
        ctx
    }

    // The guard the whole design rests on, exercised through the real
    // classifier rather than a hand-made entry: a ready, readable document
    // whose file the scanner has never seen has no source to re-render
    // from, so its renders are not a cache and must never be swept. This is
    // the 282 MiB in the author's own library.
    #[test]
    fn survey_pins_a_ready_doc_with_no_source_file() {
        let ctx = tree("pinned", &[("orphan", Some(DocState::Ready), 3)]);
        let entries = survey(&ctx, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].class,
            Class::Pinned,
            "a doc with no file row cannot be re-rendered and is not cache"
        );
        assert!(entries[0].bytes > 0);

        // and the policy honours it at any budget, including a punitive one
        let s = plan(&entries, MIN_BUDGET);
        assert!(s.docs.is_empty(), "swept the last copy: {:?}", s.docs);
        assert_eq!(s.pinned, entries[0].bytes);
        let _ = std::fs::remove_dir_all(&ctx.data);
    }

    // A tombstone whose file is still on disk is the reclaimable case — the
    // 217 MiB of orphaned renders the survey found — and it must be told
    // apart from a tombstone whose file is gone, which is the "disappearance
    // is not a deletion" case and keeps its renders.
    #[test]
    fn survey_separates_a_retracted_doc_from_one_whose_file_vanished() {
        let ctx = tree("dead", &[("tombstone", Some(DocState::Deleted), 2)]);
        // no files row, so the source is unresolvable: pinned, not dead
        let entries = survey(&ctx, None);
        assert_eq!(entries[0].class, Class::Pinned);
        assert!(plan(&entries, MIN_BUDGET).docs.is_empty());
        let _ = std::fs::remove_dir_all(&ctx.data);
    }

    #[test]
    fn survey_marks_the_open_doc_and_docs_being_ingested() {
        let ctx = tree(
            "busy",
            &[
                ("reading", Some(DocState::Ready), 2),
                ("ingesting", Some(DocState::Preparing), 2),
            ],
        );
        let entries = survey(&ctx, Some("reading"));
        let class = |doc: &str| entries.iter().find(|e| e.doc == doc).unwrap().class;
        assert_eq!(class("reading"), Class::Open);
        assert_eq!(class("ingesting"), Class::Busy);
        let _ = std::fs::remove_dir_all(&ctx.data);
    }

    // End to end over a real tree: the sweep must take page renders and
    // leave everything else — the cover especially, which is on the floor,
    // and the directory itself, which the render path assumes exists.
    #[test]
    fn the_sweep_takes_pages_and_spares_the_floor() {
        let dir = std::env::temp_dir().join(format!("cache-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ctx = Ctx::in_memory(&dir).unwrap();
        let pages = pages_of(&ctx, "doc");
        std::fs::create_dir_all(&pages).unwrap();
        for i in 1..=4 {
            std::fs::write(pages.join(format!("page-{i:04}.jpg")), vec![0u8; 1024]).unwrap();
        }
        std::fs::write(pages.join("cover.jpg"), b"cover").unwrap();
        std::fs::create_dir_all(dir.join("ocr").join("doc")).unwrap();
        std::fs::write(dir.join("ocr/doc/page-0001.json"), b"{}").unwrap();

        evict(&dir.join("pages"), "doc");

        assert!(pages.exists(), "the directory must survive the sweep");
        assert!(
            pages.join("cover.jpg").exists(),
            "the cover is on the floor"
        );
        assert_eq!(
            std::fs::read_dir(&pages).unwrap().count(),
            1,
            "only the cover should remain"
        );
        assert!(
            dir.join("ocr/doc/page-0001.json").exists(),
            "OCR is never evictable — it is what makes a re-render cheap"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
