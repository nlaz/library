//! The search-surface commands: query answering, type-ahead completion,
//! readiness, and the hidden perf views.

use library_core::Query;
use library_core::wire::Response;
use tauri::State;

use crate::engine::{AppState, answer, engine};

#[tauri::command]
pub(crate) async fn search(state: State<'_, AppState>, query: Query) -> Result<Response, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || answer(&eng, &data, &query))
        .await
        .map_err(|e| e.to_string())
}

/// Frequency-ranked word completions for the search box's type-ahead
/// dropdown — the desktop analogue of the server's `/api/complete` route.
/// One prefix scan over the term dictionary; no embedding.
#[tauri::command]
pub(crate) async fn complete(
    state: State<'_, AppState>,
    prefix: String,
    k: Option<usize>,
) -> Result<Vec<String>, String> {
    let eng = engine(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let q = prefix.trim();
        if q.is_empty() {
            return Vec::<String>::new();
        }
        let lib = eng.lib.read().expect("library lock poisoned");
        lib.rtx(|(_, (_, terms))| terms.complete_ranked(q, k.unwrap_or(8)))
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn ready(state: State<'_, AppState>) -> bool {
    state
        .engine
        .read()
        .expect("engine slot lock poisoned")
        .is_some()
}

/// Hidden perf view (Cmd+.): the search ring (per-stage timings + per-hit
/// ranker provenance) plus the constants/corpus-counts header — the desktop
/// analogue of the server's `/api/perf/searches` route.
#[tauri::command]
pub(crate) async fn perf_searches(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let chunks = eng
            .lib
            .read()
            .expect("library lock poisoned")
            .rtx(|((_, vec), _)| vec.len());
        let figures = eng
            .images
            .read()
            .expect("images lock poisoned")
            .rtx(|(vec, _)| vec.len());
        let docs = std::fs::read_dir(data.join("pages"))
            .map(|d| d.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        serde_json::json!({
            "meta": library_core::perf::meta(chunks, figures, docs),
            "searches": library_core::perf::search_log(),
        })
    })
    .await
    .map_err(|e| e.to_string())
}

/// Memory provenance for the perf view: where RAM goes (indexes, caches,
/// models, stores) against process RSS — the desktop analogue of the
/// server's `/api/perf/memory` route.
#[tauri::command]
pub(crate) async fn perf_memory(
    state: State<'_, AppState>,
) -> Result<library_core::perf::MemoryBreakdown, String> {
    let eng = engine(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let rss = memory_stats::memory_stats().map(|m| m.physical_mem as u64);
        let lib = eng.lib.read().expect("library lock poisoned");
        let images = eng.images.read().expect("images lock poisoned");
        library_core::perf::memory(&lib, &images, rss)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Per-doc ingest metrics; lazily backfills legibility for docs from before
/// metrics existed (first call on a big library takes seconds).
#[tauri::command]
pub(crate) async fn perf_ingest(
    state: State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || library_core::perf::ingest_rows(&data))
        .await
        .map_err(|e| e.to_string())
}

/// Hidden corpus-atlas view (Cmd+/): the cached themes/throughlines
/// sidecar, built lazily in the background on first open and warmed after
/// ingest — the desktop analogue of the server's `/api/atlas` route.
#[tauri::command]
pub(crate) async fn atlas(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    let librarian = crate::chat::librarian_bin(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let fp = {
            let lib = eng.lib.read().expect("library lock poisoned");
            library_core::atlas::fingerprint(&lib, &data)
        };
        if !refresh.unwrap_or(false)
            && let Some(a) = library_core::atlas::load_fresh(&data, &fp)
        {
            // a manual rebuild may be running behind a still-fresh sidecar;
            // the flag keeps the client polling
            return serde_json::json!({
                "status": "ready",
                "rebuilding": library_core::atlas::building().is_some(),
                "atlas": a,
            });
        }
        if let Some(claim) = library_core::atlas::try_claim() {
            std::thread::spawn(move || {
                let lib = eng.lib.read().expect("library lock poisoned");
                if let Err(e) = library_core::atlas::build(claim, &lib, &data, Some(&librarian)) {
                    eprintln!("atlas build failed: {e:#}");
                }
            });
        }
        let (since, stage) = library_core::atlas::building().unwrap_or((0, "starting"));
        serde_json::json!({"status": "building", "since": since, "stage": stage})
    })
    .await
    .map_err(|e| e.to_string())
}
