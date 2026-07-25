//! Note-box and annotation commands: thin wrappers over the core logic
//! (`library_core::notes` / `annots`), which is where the behavior and
//! its tests live. Reads take the fs or a read lock; writes take the
//! engine's write locks exactly like the ingest committer, so searches
//! keep running between saves. Handlers return errors — a poisoned write
//! lock would outlive any panic here.

use library_core::annots::{self, AnnotRec};
use library_core::notes::{self, CardRec, NeighborCard, NewCard, ThreadProposal};
use tauri::State;

use crate::engine::{AppState, engine};

fn embed(s: &str) -> library_core::Emb {
    ese::encode_single(s)
}

#[tauri::command]
pub(crate) async fn list_annotations(
    state: State<'_, AppState>,
    doc: String,
) -> Result<Vec<AnnotRec>, String> {
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || annots::load_annots(&data, &doc))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) async fn save_annotation(
    state: State<'_, AppState>,
    annot: AnnotRec,
) -> Result<AnnotRec, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut lib = eng.lib.write().expect("library lock poisoned");
        annots::save_annot(&mut lib, &data, annot, &embed).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn delete_annotation(
    state: State<'_, AppState>,
    doc: String,
    id: String,
) -> Result<(), String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut lib = eng.lib.write().expect("library lock poisoned");
        annots::delete_annot(&mut lib, &data, &doc, &id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn list_cards(state: State<'_, AppState>) -> Result<Vec<CardRec>, String> {
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || notes::load_cards(&data))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) async fn create_card(
    state: State<'_, AppState>,
    input: NewCard,
) -> Result<CardRec, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut lib = eng.lib.write().expect("library lock poisoned");
        notes::create_card(&mut lib, &data, input, &embed).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn update_card(
    state: State<'_, AppState>,
    card: CardRec,
) -> Result<CardRec, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut lib = eng.lib.write().expect("library lock poisoned");
        notes::update_card(&mut lib, &data, card, &embed).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn propose_thread(
    state: State<'_, AppState>,
    text: String,
) -> Result<Option<ThreadProposal>, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let lib = eng.lib.read().expect("library lock poisoned");
        notes::propose_thread(&lib, &data, &embed(&text))
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) async fn card_neighbors(
    state: State<'_, AppState>,
    id: String,
    k: Option<usize>,
) -> Result<Vec<NeighborCard>, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let lib = eng.lib.read().expect("library lock poisoned");
        notes::card_neighbors(&lib, &data, &id, k.unwrap_or(8))
    })
    .await
    .map_err(|e| e.to_string())
}
