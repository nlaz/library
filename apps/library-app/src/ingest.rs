//! Ingest: the in-app worker thread sweeping the filesystem queue, progress
//! events to the webview, and the drop/pick entry point.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use library_ingest::Progress;
use library_ingest::status::{self, DocState, DocStatus};
use library_ingest::worker::{self, Outcome};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::engine::{AppState, EngineCommitter, dev_root};
use crate::settings::ingest_ctx;

/// Install/repair the launchd agent so ingestion continues while the app
/// is closed. Best-effort: a missing worker binary (e.g. a bare release
/// bundle without the sidecar) just logs and skips.
pub(crate) fn install_agent(data: &Path) {
    let candidates = [
        // bundled sidecar next to the app binary
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("library-ingest"))),
        // dev builds share the workspace target dir
        Some(dev_root().join("target/release/library-ingest")),
        Some(dev_root().join("target/debug/library-ingest")),
    ];
    let Some(bin) = candidates.into_iter().flatten().find(|p| p.is_file()) else {
        eprintln!("library-ingest binary not found — background ingest agent not installed");
        return;
    };
    match library_ingest::agent::install(&bin, data) {
        Ok(path) => println!("ingest agent: {}", path.display()),
        Err(e) => eprintln!("ingest agent install failed: {e:#}"),
    }
}

#[derive(Serialize, Clone)]
struct IngestEvent {
    doc: String,
    stage: &'static str,
    done: usize,
    total: usize,
    message: String,
}

fn emit_progress(app: &AppHandle, doc: &str, p: Progress) {
    let (stage, done, total, message) = match p {
        Progress::Log(line) => ("log", 0, 0, line),
        Progress::Ocr { done, total } => ("ocr", done as usize, total as usize, String::new()),
        // metrics-only event; the UI progress bar has nothing to show for it
        Progress::OcrSummary { .. } => return,
        Progress::Clean { done, total } => ("clean", done, total, String::new()),
        Progress::Embed { done, total } => ("embed", done, total, String::new()),
        Progress::Figures { done, total } => ("figures", done, total, String::new()),
        Progress::Clip { done, total } => ("clip", done, total, String::new()),
        Progress::Indexing => ("indexing", 0, 0, String::new()),
    };
    let _ = app.emit(
        "ingest:progress",
        &IngestEvent {
            doc: doc.to_string(),
            stage,
            done,
            total,
            message,
        },
    );
}

fn emit_stage(app: &AppHandle, doc: &str, stage: &'static str) {
    let _ = app.emit(
        "ingest:progress",
        &IngestEvent {
            doc: doc.to_string(),
            stage,
            done: 0,
            total: 0,
            message: String::new(),
        },
    );
}

/// Sweep the filesystem queue until it's dry, then wait for a wake-up (a
/// new drop, a retry) or the periodic timeout. The periodic sweep is what
/// picks up work the CLI worker staged after this app instance launched
/// (see `library_ingest::worker` for the handoff race).
pub(crate) fn ingest_worker(app: AppHandle, rx: mpsc::Receiver<()>) {
    // utility QoS for this thread only (Vision OCR and ort inherit it);
    // the GUI stays at full priority
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    let state = app.state::<AppState>();
    let ctx = ingest_ctx(&state.settings);
    let data = ctx.data.clone();

    // engine must be up before we can commit (stores are shared)
    let eng = loop {
        if let Some(e) = state.engine.read().unwrap().clone() {
            break e;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    // startup recovery doubles as backfill: pre-status-era docs that are
    // already in the manifest get `ready` so the sweep never re-ingests them
    {
        let pend = worker::pending(&data);
        let lib = eng.lib.read().unwrap();
        if let Err(e) = worker::backfill_ready_with(&data, &pend, |d| worker::manifest_has(&lib, d))
        {
            eprintln!("status backfill failed: {e:#}");
        }
    }

    let mut committer = EngineCommitter { eng };
    loop {
        for doc in worker::pending(&data) {
            let outcome = worker::process_doc(&ctx, &doc, &mut committer, &mut |p| {
                emit_progress(&app, &doc, p)
            });
            match outcome {
                Outcome::Ready => emit_stage(&app, &doc, "done"),
                Outcome::Failed => {
                    let msg = status::read(&data, &doc)
                        .and_then(|s| s.error)
                        .unwrap_or_else(|| "ingest failed".into());
                    eprintln!("ingest '{doc}' failed: {msg}");
                    let _ = app.emit(
                        "ingest:progress",
                        &IngestEvent {
                            doc: doc.clone(),
                            stage: "error",
                            done: 0,
                            total: 0,
                            message: msg,
                        },
                    );
                }
                // Staged can't happen here (EngineCommitter never returns
                // Locked); Skipped means someone else has the claim
                Outcome::Staged | Outcome::Skipped => {}
            }
        }
        // drain buffered wake-ups so a burst of drops is one sweep
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => while rx.try_recv().is_ok() {},
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Accept dropped/picked PDFs: bring each into the library (`mode:
/// "move"` relocates the file; anything else copies), mark it queued, and
/// wake the worker. Returns the doc ids actually queued (dedup'd against
/// docs already in flight).
#[tauri::command]
pub(crate) fn ingest_paths(
    state: State<'_, AppState>,
    paths: Vec<String>,
    collection: Option<String>,
    mode: Option<String>,
) -> Result<Vec<String>, String> {
    let ctx = ingest_ctx(&state.settings);
    let data = &state.settings.data;
    let mover = if mode.as_deref() == Some("move") {
        library_ingest::move_pdf
    } else {
        library_ingest::add_pdf
    };
    let mut queued = Vec::new();
    for p in paths {
        let path = PathBuf::from(&p);
        if path
            .extension()
            .map(|e| !e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(true)
        {
            continue;
        }
        let (doc, _pdf) = mover(&ctx, &path, None).map_err(|e| e.to_string())?;
        // in-flight docs keep their state; terminal states re-queue
        // (deleted tombstones revive — re-adding is an explicit user act)
        match status::read(data, &doc).map(|s| s.state) {
            Some(DocState::Queued | DocState::Preparing | DocState::Staged) => continue,
            Some(DocState::TextReady) => continue, // finishing figures already
            _ => {}
        }
        status::write(data, &doc, &DocStatus::new(DocState::Queued)).map_err(|e| e.to_string())?;
        // collections apply at enqueue time: the card lands on its shelf
        // immediately, and the shared worker loop stays collection-free
        if let Some(col) = &collection {
            library_ingest::collect(data, col, &doc).map_err(|e| e.to_string())?;
        }
        queued.push(doc);
    }
    if !queued.is_empty() {
        let _ = state.wake.send(());
    }
    Ok(queued)
}
