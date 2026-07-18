//! Browse + library management: the doc cards, titles, collections, and
//! delete/retry.

use std::collections::HashSet;
use std::path::Path;

use library_core::wire;
use library_ingest::status::{self, DocState, DocStatus};
use library_ingest::worker;
use serde::Serialize;
use tauri::State;

use crate::engine::{AppState, engine};

#[derive(Serialize)]
pub struct DocInfo {
    pub id: String,
    /// User-set display title; the UI falls back to prettifying the id.
    pub title: Option<String>,
    pub pages: u32,
    pub collections: Vec<String>,
    /// Not yet searchable: queued, preparing, or staged.
    pub processing: bool,
    /// Durable ingest status (`data/status/<doc>.json`); `None` for docs
    /// that predate status tracking.
    pub status: Option<DocStatus>,
}

/// data/titles.json: {"doc-id": "Display Title", ...}. The doc id is the
/// primary key across the index and filesystem, so renames live here.
type Titles = std::collections::BTreeMap<String, String>;

fn read_titles(data: &Path) -> Titles {
    std::fs::read(data.join("titles.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_titles(data: &Path, titles: &Titles) -> Result<(), String> {
    status::write_json_atomic(&data.join("titles.json"), titles).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn collections(state: State<'_, AppState>) -> wire::Collections {
    wire::read_collections(&state.settings.data)
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
    let cols = wire::read_collections(data);
    let titles = read_titles(data);
    let statuses = status::scan(data);

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
                continue; // tombstone: only the PDF remains
            }
            let pages = wire::count_pages(&e.path());
            seen.insert(id.clone());
            out.push(DocInfo {
                pages,
                title: titles.get(&id).cloned(),
                collections: cols
                    .iter()
                    .filter(|(_, docs)| docs.iter().any(|d| *d == id))
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
    let data = &state.settings.data;
    let mut titles = read_titles(data);
    let title = title.trim();
    let changed = if title.is_empty() {
        titles.remove(&doc).is_some()
    } else {
        titles.insert(doc, title.to_string()) != Some(title.to_string())
    };
    if changed {
        write_titles(data, &titles)?;
    }
    Ok(())
}

/// Replace a doc's collection membership (empty list = in no collection).
#[tauri::command]
pub(crate) fn set_collections(
    state: State<'_, AppState>,
    doc: String,
    collections: Vec<String>,
) -> Result<(), String> {
    library_ingest::set_collections(&state.settings.data, &doc, &collections)
        .map_err(|e| e.to_string())
}

/// Remove a doc: retract it from both indexes, delete its page renders and
/// OCR cache, and prune it from collections/titles. The copied PDF in
/// data/pdfs is kept; a `deleted` tombstone status stops the background
/// worker from re-ingesting it (re-adding the same PDF revives it).
#[tauri::command]
pub(crate) async fn delete_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    let data = state.settings.data.clone();
    if worker::claimed(&data, &doc)
        || status::read(&data, &doc).map(|s| s.state) == Some(DocState::Preparing)
    {
        return Err("still processing — try again when ingest finishes".into());
    }
    let eng = engine(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        // retract from the stores first so search can't hand out hits whose
        // page images are already gone
        {
            let mut lib = eng.lib.write().unwrap();
            library_ingest::commit_text(&mut lib, &doc, &[]);
        }
        {
            let mut images = eng.images.write().unwrap();
            library_ingest::commit_figures(&mut images, &doc, &[]);
        }
        for dir in ["pages", "ocr"] {
            if let Err(e) = std::fs::remove_dir_all(data.join(dir).join(&doc)) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("removing {dir}/{doc}: {e}"));
                }
            }
        }
        worker::clear_staged(&data, &doc);
        status::write(&data, &doc, &DocStatus::new(DocState::Deleted))
            .map_err(|e| e.to_string())?;
        library_ingest::set_collections(&data, &doc, &[]).map_err(|e| e.to_string())?;
        let mut titles = read_titles(&data);
        if titles.remove(&doc).is_some() {
            write_titles(&data, &titles)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Re-queue a doc whose ingest failed.
#[tauri::command]
pub(crate) fn retry_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    let data = &state.settings.data;
    if status::read(data, &doc).map(|s| s.state) != Some(DocState::Failed) {
        return Err("not in a failed state".into());
    }
    status::write(data, &doc, &DocStatus::new(DocState::Queued)).map_err(|e| e.to_string())?;
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
}
