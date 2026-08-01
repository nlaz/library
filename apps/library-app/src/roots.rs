//! Watched folders: the first-run default, and the commands behind the
//! Settings page.
//!
//! Linking a folder is the one privileged act in the app — it is how the
//! user hands us access to part of their disk — so it happens through the
//! native open panel and nowhere else. macOS grants access to what the user
//! picks in that panel; a path typed into a text field would be refused for
//! any protected location and would look like a bug rather than a
//! permission.

use std::path::{Path, PathBuf};

use library_core::meta::RootRec;
use library_core::roots;
use serde::Serialize;
use tauri::State;

use crate::engine::AppState;

/// The folder a new library gets by default.
///
/// `~/The Library`, not `~/Library` — that one is macOS's own, has been
/// hidden in Finder since 10.7, and is where this app's own private data
/// lives. Two different things named the same would be a cruel joke to play
/// on anyone trying to find their books in a terminal.
pub(crate) fn default_library_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join("The Library"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Make sure the library has somewhere to put things.
///
/// Only ever creates a folder when there are no roots at all — a user who
/// unlinked everything on purpose should not find the default folder back
/// the next morning. In dev builds the default lives under the repo's data
/// dir so a debug run never touches the real one.
pub(crate) fn ensure_default_root(ctx: &library_core::meta::Ctx) {
    if !ctx.roots().is_empty() {
        return;
    }
    let dir = if cfg!(debug_assertions) {
        ctx.data.join("Library")
    } else {
        match default_library_dir() {
            Some(d) => d,
            None => return,
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create the library folder {}: {e}", dir.display());
        return;
    }
    match ctx.add_root(&dir, now()) {
        Ok(r) => println!("library folder: {}", r.path.display()),
        Err(e) => eprintln!("could not link the library folder: {e}"),
    }
}

/// A root as the Settings page shows it: what it is, and what we can say
/// about it without making the user open Finder to find out.
#[derive(Serialize)]
pub(crate) struct RootInfo {
    #[serde(flatten)]
    pub rec: RootRec,
    /// Documents currently indexed under it.
    pub docs: usize,
    /// Whether the folder is readable right now. A root can be linked and
    /// unreadable at the same time — an ejected drive — and the difference
    /// is the whole reason the list shows state at all.
    pub available: bool,
}

#[tauri::command]
pub(crate) fn list_roots(state: State<'_, AppState>) -> Vec<RootInfo> {
    state
        .ctx
        .roots()
        .into_iter()
        .map(|rec| RootInfo {
            docs: state.ctx.files_in_root(&rec.id).len(),
            available: rec.path.is_dir(),
            rec,
        })
        .collect()
}

/// Link a folder and scan it immediately, so the books appear now rather
/// than at whatever the next sweep happens to be.
#[tauri::command]
pub(crate) async fn link_root(
    state: State<'_, AppState>,
    path: String,
) -> Result<RootInfo, String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("{path} is not a folder"));
    }
    let ctx = state.ctx.clone();
    let wake = state.wake.clone();
    let rec = tauri::async_runtime::spawn_blocking(move || -> Result<RootRec, String> {
        let rec = ctx.add_root(&dir, now()).map_err(|e| e.to_string())?;
        let applied = roots::sync_root(&ctx.meta, &rec, now());
        for doc in &applied.queued {
            let st =
                library_ingest::status::DocStatus::new(library_ingest::status::DocState::Queued);
            let _ = library_ingest::status::write(&ctx.meta, doc, &st);
        }
        if !applied.queued.is_empty() {
            let _ = wake.send(());
        }
        Ok(rec)
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(RootInfo {
        docs: state.ctx.files_in_root(&rec.id).len(),
        available: rec.path.is_dir(),
        rec,
    })
}

/// Unlink a folder. The files stay exactly where they are; their documents
/// go missing, which is the same path a deletion takes — so the notes and
/// the page renders survive, and re-linking the folder brings everything
/// back without re-reading a page.
#[tauri::command]
pub(crate) fn unlink_root(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let ctx = &state.ctx;
    if ctx.roots().len() <= 1 {
        return Err("this is your only library folder — link another one first".into());
    }
    for f in ctx.files_in_root(&id) {
        let _ = library_ingest::status::write(
            &ctx.meta,
            &f.doc,
            &library_ingest::status::DocStatus::new(library_ingest::status::DocState::Deleted),
        );
    }
    ctx.remove_root(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn set_default_root(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.ctx.set_default_root(&id).map_err(|e| e.to_string())
}

/// Bytes under `dir`, for the storage readout. Best-effort: an unreadable
/// subdirectory contributes zero rather than failing the whole figure.
pub(crate) fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

/// What the caches cost, so the Settings page can say it without the user
/// going hunting through Application Support.
#[tauri::command]
pub(crate) async fn storage_use(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let derived: u64 = ["pages", "ocr", "text", "clean", "edits"]
            .iter()
            .map(|d| dir_bytes(&data.join(d)))
            .sum();
        serde_json::json!({
            "path": data.to_string_lossy(),
            "derived_bytes": derived,
            "index_bytes": dir_bytes(&data.join("library.db")) + dir_bytes(&data.join("images.db")),
            "model_bytes": dir_bytes(&data.join("models")),
        })
    })
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_folder_is_not_the_one_macos_owns() {
        let dir = default_library_dir().expect("a home dir");
        assert!(dir.ends_with("The Library"));
        assert_ne!(
            dir.file_name().unwrap_or_default(),
            "Library",
            "~/Library is macOS's own and is hidden in Finder"
        );
    }

    #[test]
    fn ensure_default_root_is_a_one_time_act() {
        let dir = std::env::temp_dir().join(format!("app-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("data dir");
        let ctx = library_core::meta::Ctx::in_memory(&dir).expect("meta");

        ensure_default_root(&ctx);
        assert_eq!(ctx.roots().len(), 1);
        let first = ctx.roots()[0].clone();
        assert!(first.is_default);
        assert!(
            first.path.is_dir(),
            "the folder is created, not just recorded"
        );

        // a second launch adds nothing
        ensure_default_root(&ctx);
        assert_eq!(ctx.roots().len(), 1);

        // and a user who unlinked everything does not get it back
        ctx.remove_root(&first.id).expect("unlink");
        ctx.add_root(&dir.join("elsewhere"), 1).expect("their own");
        ensure_default_root(&ctx);
        assert_eq!(
            ctx.roots().len(),
            1,
            "we only step in when there is nothing"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn dir_bytes_sums_nested_files_and_tolerates_absence() {
        let dir = std::env::temp_dir().join(format!("app-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).expect("dirs");
        std::fs::write(dir.join("a/one"), b"12345").expect("write");
        std::fs::write(dir.join("a/b/two"), b"123").expect("write");

        assert_eq!(dir_bytes(&dir), 8);
        assert_eq!(dir_bytes(&dir.join("nope")), 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
