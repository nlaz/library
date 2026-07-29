//! Note-box commands: thin wrappers over the core logic
//! (`library_core::notes`), which is where the behavior and its tests
//! live. Reads take the fs or a read lock; writes take the engine's
//! write locks exactly like the ingest committer, so searches keep
//! running between saves. Handlers return errors — a poisoned write
//! lock would outlive any panic here.

use library_core::notes::{self, CardRec, NewCard};
use tauri::State;

use crate::engine::{AppState, engine};

fn embed(s: &str) -> library_core::Emb {
    ese::encode_single(s)
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
