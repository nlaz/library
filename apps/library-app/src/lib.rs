//! The Library as a Tauri desktop app.
//!
//! Everything runs in this one process: the fold stores, the embedding
//! models, and the ingest pipeline. Searches take a read lock; the ingest
//! worker does OCR/embedding lock-free and takes the write lock only for the
//! brief atomic swap — so the app can search while it ingests, and the fjall
//! single-writer lock is never contended.
//!
//! Ingest state is NOT held here: the queue is `data/pdfs/` filtered by the
//! `docs` table (see `library_ingest::worker`). The app's worker thread
//! sweeps that queue — holding the stores makes it the owner
//! — and picks up anything the `library-ingest worker` CLI staged while the
//! app was closed. Indexing only happens while the app runs; there is no
//! background agent.

mod cache;
mod chat;
mod commands;
mod docs;
mod engine;
mod ingest;
mod marginalia;
mod mem;
mod prefs;
mod render;
mod roots;
mod serve;
mod settings;
mod update;
mod watch;

use std::sync::{RwLock, mpsc};

use tauri::Manager;

use crate::engine::{AppState, init_engine};
use crate::ingest::{ingest_worker, uninstall_legacy_agent};
use crate::serve::{serve_pages, serve_static};
use crate::settings::load_settings;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        // registered for its Rust API only — update.rs drives the check and
        // the install, and the webview is deliberately not given the
        // plugin's commands (see capabilities/default.json)
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol("pages", |ctx, request, responder| {
            let state = ctx.app_handle().state::<AppState>();
            let server = state.pages.clone();
            // a search response can carry ~20-26 hits, each firing a page-image
            // request — spawn_blocking uses tokio's bounded, reused blocking
            // pool instead of a raw OS thread per request, which used to mean
            // a burst of thread creations on every keystroke's re-render
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_pages(&server, request))
            });
        })
        .register_asynchronous_uri_scheme_protocol("ocr", |ctx, request, responder| {
            let data = ctx.app_handle().state::<AppState>().settings.data.clone();
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_static(
                    data.join("ocr"),
                    "application/json",
                    "public, max-age=31536000, immutable",
                    request,
                ))
            });
        })
        .setup(|app| {
            let settings = load_settings(app.handle());
            // the metadata db must open before anything reads a title or a
            // shelf; failing here is fatal in the way a missing data dir is
            let ctx = library_core::meta::Ctx::open(&settings.data)
                .expect("opening meta.db in the data dir");
            // a library with no folder has nowhere to put a dropped file
            roots::ensure_default_root(&ctx);
            let (tx, rx) = mpsc::channel::<()>();
            let pages = crate::serve::PageServer::new(&settings.data, ctx.clone(), settings.width);
            app.manage(AppState {
                settings,
                ctx,
                pages,
                engine: RwLock::new(None),
                status: std::sync::Mutex::new(crate::engine::Status::default()),
                wake: tx,
                chat: std::sync::Mutex::new(None),
                chat_stdin: std::sync::Mutex::new(None),
            });

            let h = app.handle().clone();
            std::thread::spawn(move || init_engine(h));
            let h = app.handle().clone();
            std::thread::spawn(move || ingest_worker(h, rx));
            std::thread::spawn(uninstall_legacy_agent);
            // FSEvents: a latency improvement over the periodic sweep, not
            // a second source of truth — it only wakes the same reconcile
            let wctx = app.state::<AppState>().ctx.clone();
            let wake = app.state::<AppState>().wake.clone();
            std::thread::spawn(move || watch::watch_roots(wctx, wake));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::complete,
            commands::ready,
            commands::startup_status,
            commands::perf_searches,
            commands::perf_memory,
            commands::perf_ingest,
            commands::atlas,
            docs::collections,
            docs::docs,
            docs::set_title,
            docs::delete_doc,
            docs::cancel_doc,
            docs::retry_doc,
            docs::reveal_doc,
            ingest::ingest_paths,
            roots::list_roots,
            roots::link_root,
            roots::unlink_root,
            roots::set_default_root,
            roots::move_to_shelf,
            roots::shelves,
            roots::storage_use,
            roots::sweep_cache,
            settings::get_settings,
            settings::set_settings,
            prefs::get_pref,
            prefs::set_pref,
            update::updates_supported,
            update::check_update,
            update::install_update,
            update::restart_app,
            chat::chat_turn,
            chat::chat_cancel,
            chat::chat_status,
            marginalia::list_cards,
            marginalia::create_card,
            marginalia::update_card,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
