//! Browse + library management: the doc cards, titles, collections, and
//! delete/retry.

use std::collections::HashSet;
use std::path::PathBuf;

use library_core::wire;
use library_ingest::status::{self, DocState, DocStatus};
use library_ingest::worker;
use serde::Serialize;
use tauri::State;

use crate::engine::{AppState, engine};

#[derive(Serialize)]
pub struct DocInfo {
    pub id: String,
    /// User-set display title, and nothing else — an untitled book must
    /// stay untitled so "Rename" knows it is naming, not re-naming.
    pub title: Option<String>,
    /// The name of the file this document came from, without its
    /// extension. What the UI shows when there is no title: the id is
    /// minted and unreadable, and this is what the user recognises.
    pub name: Option<String>,
    pub pages: u32,
    pub collections: Vec<String>,
    /// Not yet searchable: queued, preparing, or staged.
    pub processing: bool,
    /// Durable ingest status (the `docs` table); `None` for docs
    /// that predate status tracking.
    pub status: Option<DocStatus>,
}

#[tauri::command]
pub(crate) fn collections(state: State<'_, AppState>) -> wire::Collections {
    state.ctx.shelves()
}

fn is_processing(st: Option<&DocStatus>) -> bool {
    matches!(
        st.map(|s| s.state),
        Some(DocState::Queued | DocState::Preparing | DocState::Staged)
    )
}

#[tauri::command]
pub(crate) fn docs(state: State<'_, AppState>) -> Vec<DocInfo> {
    let data = &state.settings.data;
    let ctx = &state.ctx;
    let cols = ctx.shelves();
    let titles = ctx.titles();
    let names = ctx.file_names();
    let statuses = status::scan(ctx);

    let mut out: Vec<DocInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(data.join("pages")) {
        for e in entries.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = e.file_name().to_string_lossy().into_owned();
            let st = statuses.get(&id);
            if st.map(|s| s.state) == Some(DocState::Deleted) {
                continue; // tombstone: only the source file remains
            }
            let pages = wire::count_pages(&e.path());
            seen.insert(id.clone());
            out.push(DocInfo {
                pages,
                title: titles.get(&id).cloned(),
                name: names.get(&id).cloned(),
                collections: cols
                    .iter()
                    .filter(|(_, docs)| docs.contains(&id))
                    .map(|(c, _)| c.clone())
                    .collect(),
                processing: is_processing(st),
                status: st.cloned(),
                id,
            });
        }
    }
    // docs with a live status but no pages dir yet (just queued, or failed
    // before rendering) still get a card
    for (id, st) in &statuses {
        if seen.contains(id) || matches!(st.state, DocState::Ready | DocState::Deleted) {
            continue;
        }
        out.push(DocInfo {
            id: id.clone(),
            title: titles.get(id).cloned(),
            name: names.get(id).cloned(),
            pages: 0,
            collections: cols
                .iter()
                .filter(|(_, docs)| docs.iter().any(|d| d == id))
                .map(|(c, _)| c.clone())
                .collect(),
            processing: is_processing(Some(st)),
            status: Some(st.clone()),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Set (or clear, with an empty/whitespace title) a doc's display title.
#[tauri::command]
pub(crate) fn set_title(
    state: State<'_, AppState>,
    doc: String,
    title: String,
) -> Result<(), String> {
    let title = title.trim();
    state
        .ctx
        .set_title(&doc, (!title.is_empty()).then_some(title))
        .map_err(|e| e.to_string())
}

/// Throw away everything derived from a document's file: page renders, OCR,
/// the cleaned text and the markdown edition.
///
/// `edits/` is deliberately not in the list. Everything else here is a
/// machine's output and comes back for the price of an ingest; edits are a
/// person's hand corrections to bad OCR, and there is no recomputing those.
///
/// Best-effort by design — this runs from two places that have already
/// decided the document is going, and a cache file that resists deletion is
/// not a reason to leave the library half-changed.
pub(crate) fn forget_caches(data: &std::path::Path, doc: &str) {
    for dir in ["pages", "ocr", "clean"] {
        let _ = std::fs::remove_dir_all(data.join(dir).join(doc));
    }
    let _ = std::fs::remove_file(data.join("text").join(format!("{doc}.md")));
}

/// Remove a doc: retract it from both indexes, delete everything derived
/// from it, and prune its title. The file itself is left wherever the user
/// keeps it; a `deleted` tombstone stops the worker from re-ingesting it
/// (putting the same file back revives it).
#[tauri::command]
pub(crate) async fn delete_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    if library_core::records::is_reserved(&doc) {
        // reserved ids contain `/` — never let one near remove_dir_all
        return Err("not a document".into());
    }
    let data = state.settings.data.clone();
    let ctx = state.ctx.clone();
    if worker::claimed(&data, &doc)
        || status::read(&ctx, &doc).map(|s| s.state) == Some(DocState::Preparing)
    {
        return Err("still processing — try again when ingest finishes".into());
    }
    let eng = engine(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        // retract from the stores first so search can't hand out hits whose
        // page images are already gone
        {
            let mut lib = eng.lib.write().expect("library lock poisoned");
            library_ingest::commit_text(&mut lib, &doc, &[]);
        }
        {
            let mut images = eng.images.write().expect("images lock poisoned");
            library_ingest::commit_figures(&mut images, &doc, &[]);
        }
        forget_caches(&data, &doc);
        worker::clear_staged(&data, &doc);
        status::write(&ctx, &doc, &DocStatus::new(DocState::Deleted)).map_err(|e| e.to_string())?;
        ctx.set_title(&doc, None).map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The file a "Show in Finder" points at, wherever the user keeps it.
/// Reserved ids are synthetic docs with no file at all, and a document
/// whose file has gone missing has nothing to show.
fn reveal_target(meta: &library_core::meta::Meta, doc: &str) -> Result<PathBuf, String> {
    if library_core::records::is_reserved(doc) {
        return Err("not a document".into());
    }
    library_ingest::source_path(meta, doc).ok_or_else(|| {
        "this document's file is no longer where the library last saw it".to_string()
    })
}

/// Select a document's file in Finder. Files stay where the user put them,
/// so this is a shortcut rather than the only way back — but it is the one
/// that works without knowing which watched folder a book came from.
#[tauri::command]
pub(crate) async fn reveal_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    let path = reveal_target(&state.ctx, &doc)?;
    // blocking: `open` waits on Finder, which can be slow to come forward
    tauri::async_runtime::spawn_blocking(move || {
        // -R reveals the file in its folder instead of opening it in Preview
        let st = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(&path)
            .status()
            .map_err(|e| format!("showing {} in Finder: {e}", path.display()))?;
        if !st.success() {
            return Err(format!("Finder could not show {}", path.display()));
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Re-queue a doc whose ingest failed.
#[tauri::command]
pub(crate) fn retry_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    if status::read(&state.ctx, &doc).map(|s| s.state) != Some(DocState::Failed) {
        return Err("not in a failed state".into());
    }
    status::write(&state.ctx, &doc, &DocStatus::new(DocState::Queued))
        .map_err(|e| e.to_string())?;
    let _ = state.wake.send(());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_processing_none_is_not_processing() {
        assert!(!is_processing(None));
    }

    #[test]
    fn is_processing_covers_every_state() {
        // (state, expected) for every DocState variant: only the
        // not-yet-searchable states — queued, preparing, staged — count.
        let cases = [
            (DocState::Queued, true),
            (DocState::Preparing, true),
            (DocState::Staged, true),
            (DocState::TextReady, false),
            (DocState::Ready, false),
            (DocState::Failed, false),
            (DocState::Deleted, false),
        ];
        for (state, expected) in cases {
            let st = DocStatus::new(state);
            assert_eq!(
                is_processing(Some(&st)),
                expected,
                "is_processing for {state:?}"
            );
        }
    }

    #[test]
    fn forget_caches_clears_the_derived_work_and_spares_the_hand_edits() {
        let data = std::env::temp_dir().join(format!("library-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        for dir in ["pages", "ocr", "clean", "edits"] {
            std::fs::create_dir_all(data.join(dir).join("dabc")).expect("cache dir");
            std::fs::write(data.join(dir).join("dabc").join("1"), b"x").expect("write");
        }
        std::fs::create_dir_all(data.join("text")).expect("text dir");
        std::fs::write(data.join("text").join("dabc.md"), b"# a").expect("write");
        // a second doc's caches, to catch a path join that drops the id
        std::fs::create_dir_all(data.join("pages").join("dxyz")).expect("other doc");

        forget_caches(&data, "dabc");

        for dir in ["pages", "ocr", "clean"] {
            assert!(
                !data.join(dir).join("dabc").exists(),
                "{dir} is machine output and must go"
            );
        }
        assert!(!data.join("text").join("dabc.md").exists());
        assert!(
            data.join("edits").join("dabc").exists(),
            "hand corrections to bad OCR cannot be recomputed"
        );
        assert!(data.join("pages").join("dxyz").exists(), "only this doc");

        // a doc with nothing cached is not an error
        forget_caches(&data, "dnothing");

        std::fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn reveal_target_finds_the_file_and_refuses_what_has_none() {
        let dir = std::env::temp_dir().join(format!("library-reveal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = dir.join("Library");
        std::fs::create_dir_all(&lib).expect("root dir");
        std::fs::write(lib.join("kant.pdf"), b"%PDF-").expect("write");

        let ctx = library_core::meta::Ctx::in_memory(&dir).expect("meta");
        let root = ctx.add_root(&lib, 1).expect("link");
        library_core::roots::sync_root(&ctx.meta, &root, 2);
        let doc = ctx
            .files_in_root(&root.id)
            .first()
            .expect("a file")
            .doc
            .clone();

        assert_eq!(reveal_target(&ctx, &doc), Ok(root.path.join("kant.pdf")));
        // a document whose file has gone
        assert!(reveal_target(&ctx, "dGONE").is_err());
        // reserved ids contain `/` — they must never reach a path join
        assert_eq!(
            reveal_target(&ctx, "~cards/abc"),
            Err("not a document".into())
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
