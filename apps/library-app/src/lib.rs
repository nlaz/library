//! The Library as a Tauri desktop app.
//!
//! Everything runs in this one process: the fold stores, the embedding
//! models, and the ingest pipeline. Searches take a read lock; the ingest
//! worker does OCR/embedding lock-free and takes the write lock only for the
//! brief atomic swap — so the app can search while it ingests, and the fjall
//! single-writer lock is never contended.
//!
//! Ingest state is NOT held here: the queue is the filesystem
//! (`data/pdfs/` + `data/status/`, see `library_ingest::worker`), shared
//! with the `library-ingest worker` CLI that launchd runs while the app is
//! closed. The app's worker thread sweeps that same queue — holding the
//! stores makes it the owner — and picks up anything the CLI staged.

mod chat;
mod commands;
mod docs;
mod engine;
mod ingest;
mod serve;
mod settings;

use std::sync::{RwLock, mpsc};

use tauri::Manager;

use crate::engine::{AppState, init_engine};
use crate::ingest::{ingest_worker, install_agent};
use crate::serve::serve_static;
use crate::settings::load_settings;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .register_asynchronous_uri_scheme_protocol("pages", |ctx, request, responder| {
            let data = ctx.app_handle().state::<AppState>().settings.data.clone();
            // a search response can carry ~20-26 hits, each firing a page-image
            // request — spawn_blocking uses tokio's bounded, reused blocking
            // pool instead of a raw OS thread per request, which used to mean
            // a burst of thread creations on every keystroke's re-render
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_static(data.join("pages"), "image/jpeg", request))
            });
        })
        .register_asynchronous_uri_scheme_protocol("ocr", |ctx, request, responder| {
            let data = ctx.app_handle().state::<AppState>().settings.data.clone();
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_static(data.join("ocr"), "application/json", request))
            });
        })
        .setup(|app| {
            let settings = load_settings(app.handle());
            let (tx, rx) = mpsc::channel::<()>();
            app.manage(AppState {
                settings,
                engine: RwLock::new(None),
                wake: tx,
                chat: std::sync::Mutex::new(None),
                chat_stdin: std::sync::Mutex::new(None),
            });

            let h = app.handle().clone();
            std::thread::spawn(move || init_engine(h));
            let h = app.handle().clone();
            std::thread::spawn(move || ingest_worker(h, rx));
            let data = app.state::<AppState>().settings.data.clone();
            std::thread::spawn(move || {
                // data must be absolute in the plist; dev settings may be
                // repo-relative
                if let Ok(abs) = std::path::absolute(&data) {
                    install_agent(&abs);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::complete,
            commands::ready,
            commands::perf_searches,
            commands::perf_ingest,
            docs::collections,
            docs::docs,
            docs::set_title,
            docs::set_collections,
            docs::delete_doc,
            docs::retry_doc,
            ingest::ingest_paths,
            settings::get_settings,
            settings::set_settings,
            chat::chat_turn,
            chat::chat_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
