//! User preferences: the handful of choices that decide what the app
//! offers, kept in meta.db's `settings` table.
//!
//! Not to be confused with `settings.rs`, which is bootstrap configuration
//! — where the data directory is, how wide a page renders. That file is
//! read before anything else can open, so it cannot hold anything a
//! running app might want to change under itself. This can. meta.db is
//! also the one store the app, the server and the ingest CLI may all write
//! to at once, which is where every piece of library state that isn't
//! derived already lives.
//!
//! Deliberately untyped: a key and a string, with the meaning of the
//! string owned by whoever reads it. Every preference so far is set and
//! read in one place in the frontend (see `tauri.ts`), and a second copy
//! of "1 means yes" over here would be a rule with nobody to enforce it.

use tauri::State;

use crate::engine::AppState;

#[tauri::command]
pub(crate) fn get_pref(state: State<'_, AppState>, key: String) -> Option<String> {
    state.ctx.setting(&key)
}

#[tauri::command]
pub(crate) fn set_pref(
    state: State<'_, AppState>,
    key: String,
    value: String,
) -> Result<(), String> {
    state
        .ctx
        .set_setting(&key, &value)
        .map_err(|e| format!("could not save that setting: {e}"))
}
