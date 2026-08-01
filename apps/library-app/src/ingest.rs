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

use crate::engine::{AppState, EngineCommitter};
use crate::settings::ingest_ctx;

/// The launchd label releases up to 0.1.1 installed for background ingest.
const LEGACY_AGENT_LABEL: &str = "computer.flower.library.ingest";

/// Where that release wrote its plist. Split out so the label and path stay
/// pinned by a test: get either wrong and the orphaned agent survives the
/// cleanup silently, which is the whole failure this code exists to prevent.
fn legacy_plist(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LEGACY_AGENT_LABEL}.plist"))
}

/// Boot out and delete the pre-0.2 background ingest agent.
///
/// Indexing now happens only while the app runs, so that agent has no
/// replacement — but launchd remembers it. Left alone it wakes every fifteen
/// minutes forever, pointed at a binary path that the rename to
/// `dev.thelibrary` made meaningless. Every launch checks for the plist
/// (one `stat` once it's gone) so an install that predates this release is
/// cleaned up whenever it next opens the app.
pub(crate) fn uninstall_legacy_agent() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let plist = legacy_plist(Path::new(&home));
    if !plist.exists() {
        return; // the common case, and the whole cost of this call
    }
    let uid = unsafe { libc::getuid() };
    // bootout of an agent launchd never loaded fails; that's fine
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LEGACY_AGENT_LABEL}")])
        .output();
    match std::fs::remove_file(&plist) {
        Ok(()) => println!("removed the legacy background ingest agent ({LEGACY_AGENT_LABEL})"),
        Err(e) => eprintln!("could not remove {}: {e}", plist.display()),
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
        // bytes, not pages: the card renders this stage as MB (format.ts)
        Progress::Download { done, total } => {
            ("download", done as usize, total as usize, String::new())
        }
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
    let ctx = ingest_ctx(&state.settings, &state.ctx);
    let data = ctx.data.clone();
    let meta = ctx.meta.clone();

    // engine must be up before we can commit (stores are shared)
    let eng = loop {
        if let Some(e) = state
            .engine
            .read()
            .expect("engine slot lock poisoned")
            .clone()
        {
            break e;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    // startup recovery doubles as backfill: pre-status-era docs that are
    // already in the manifest get `ready` so the sweep never re-ingests them
    {
        let pend = worker::pending(&data, &meta);
        let lib = eng.lib.read().expect("library lock poisoned");
        if let Err(e) = worker::backfill_ready_with(&meta, &pend, |d| worker::manifest_has(&lib, d))
        {
            eprintln!("status backfill failed: {e:#}");
        }
    }

    let mut committer = EngineCommitter { eng };
    loop {
        let mut committed = false;
        for doc in worker::pending(&data, &meta) {
            let outcome = worker::process_doc(&ctx, &doc, &mut committer, &mut |p| {
                emit_progress(&app, &doc, p)
            });
            match outcome {
                Outcome::Ready => {
                    committed = true;
                    emit_stage(&app, &doc, "done");
                }
                Outcome::Failed => {
                    let msg = status::read(&meta, &doc)
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
        // atlas warm-up: at most one build per sweep that actually
        // committed. The sweep drains the whole queue first and wake-ups
        // are coalesced below, so a multi-doc batch triggers one build; if
        // more docs land mid-build, the finished sidecar's fingerprint is
        // already stale and the next sweep (or view open) rebuilds.
        if committed {
            let eng = committer.eng.clone();
            let fp = {
                let lib = eng.lib.read().expect("library lock poisoned");
                library_core::atlas::fingerprint(&lib, &data)
            };
            if library_core::atlas::load_fresh(&data, &fp).is_none()
                && let Some(claim) = library_core::atlas::try_claim()
            {
                let build_ctx = library_core::meta::Ctx::new(data.clone(), meta.clone());
                let librarian = crate::chat::librarian_bin(&app);
                std::thread::spawn(move || {
                    let lib = eng.lib.read().expect("library lock poisoned");
                    if let Err(e) =
                        library_core::atlas::build(claim, &lib, &build_ctx, Some(&librarian))
                    {
                        eprintln!("atlas build failed: {e:#}");
                    }
                });
            }
        }
        // drain buffered wake-ups so a burst of drops is one sweep
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => while rx.try_recv().is_ok() {},
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// What an add did, per file. One bad file no longer aborts the batch: a
/// drop of twenty scans where the third is a `.docx` should add nineteen
/// and say so, not fail with nothing visibly added and three files already
/// copied.
#[derive(Serialize, Default)]
pub(crate) struct AddResult {
    /// Documents newly queued for ingest.
    pub queued: Vec<String>,
    /// Files we already had — same bytes, already in a watched folder.
    pub duplicates: usize,
    /// Files whose type we can't read yet.
    pub skipped: Vec<String>,
    /// `(filename, why)` for files that could not be copied at all.
    pub failed: Vec<(String, String)>,
}

/// Every ingestible file under `dir`, recursively.
///
/// Dropping a folder is the obvious first thing anyone tries, and it used
/// to do nothing at all — silently, because a directory has no extension
/// and the filter dropped it without a word.
fn expand(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 16 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => expand(&p, out, depth + 1),
            Ok(_) if library_ingest::SourceKind::of(&p).is_some() => out.push(p),
            _ => {}
        }
    }
}

/// Accept dropped or picked files and folders: copy them into the default
/// watched folder, then let the scanner mint the documents.
///
/// `collection` names a subfolder to drop into — the shelf *is* the folder,
/// so filing something is putting it somewhere.
#[tauri::command]
pub(crate) fn ingest_paths(
    state: State<'_, AppState>,
    paths: Vec<String>,
    collection: Option<String>,
) -> Result<AddResult, String> {
    let meta = &state.ctx;
    let root = meta
        .default_root()
        .ok_or("no library folder is set up yet — choose one in Settings")?;
    let dest_dir = match collection.as_deref().filter(|c| !c.is_empty()) {
        Some(col) => root.path.join(col),
        None => root.path.clone(),
    };

    // folders expand to their contents; a folder of scans is one drop
    let mut files = Vec::new();
    for p in &paths {
        let path = PathBuf::from(p);
        if path.is_dir() {
            expand(&path, &mut files, 0);
        } else {
            files.push(path);
        }
    }

    let mut out = AddResult::default();
    for path in files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if library_ingest::SourceKind::of(&path).is_none() {
            out.skipped.push(name);
            continue;
        }
        // each file stands alone: a failure here costs one file, not the drop
        if let Err(e) = library_ingest::copy_into(&path, &dest_dir) {
            out.failed.push((name, e.to_string()));
        }
    }

    // one scan for the whole batch, which is also what mints the documents
    let applied = library_core::roots::sync_root(&state.ctx.meta, &root, now());
    out.duplicates = applied.duplicates;
    for doc in &applied.queued {
        status::write(meta, doc, &DocStatus::new(DocState::Queued)).map_err(|e| e.to_string())?;
    }
    out.queued = applied.queued;

    if !out.queued.is_empty() {
        let _ = state.wake.send(());
    }
    Ok(out)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plist_is_the_path_0_1_1_actually_wrote() {
        // verbatim from the deleted library_ingest::agent — a drift here
        // leaves the orphaned agent loaded and this cleanup a no-op
        assert_eq!(
            legacy_plist(Path::new("/Users/someone")),
            Path::new("/Users/someone/Library/LaunchAgents/computer.flower.library.ingest.plist")
        );
    }
}
