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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, mpsc};
use std::time::{Duration, Instant};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::wire::{self, Response};
use library_core::{ClipEmb, Images, Library, Query};
use library_ingest::status::{self, DocState, DocStatus};
use library_ingest::worker::{self, CommitErr, Committer, Outcome};
use library_ingest::{IngestCtx, Progress};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

pub struct Engine {
    lib: RwLock<Library>,
    images: RwLock<Images>,
    /// CLIP text encoder for figure search; text queries embed with ese.
    clip_text: TextEmbedding,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub data: PathBuf,
    /// Rendered page-image width in pixels.
    #[serde(default = "default_width")]
    pub width: u32,
}

fn default_width() -> u32 {
    1600
}

pub struct AppState {
    settings: Settings,
    engine: RwLock<Option<std::sync::Arc<Engine>>>,
    /// Wakes the worker thread for an immediate sweep; what to ingest
    /// comes from the status files, not the channel.
    wake: mpsc::Sender<()>,
    /// The librarian chat sidecar (AFM agent loop). The outer Mutex
    /// serializes turns; `chat_stdin` is shared separately so `chat_cancel`
    /// can write while a turn holds the bridge.
    chat: std::sync::Mutex<Option<ChatBridge>>,
    chat_stdin: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>>>,
}

// ---------------------------------------------------------------------------
// settings / paths
// ---------------------------------------------------------------------------

/// Repo root at dev time; the bundle has no repo, so release builds rely on
/// settings.json / LIBRARY_DATA / resources instead.
fn dev_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load_settings(app: &AppHandle) -> Settings {
    let file = app
        .path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("settings.json"));
    let saved: Option<Settings> = file
        .as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());

    let mut s = saved.unwrap_or_else(|| Settings {
        data: dev_root().join("data"),
        width: default_width(),
    });
    if let Ok(data) = std::env::var("LIBRARY_DATA") {
        s.data = PathBuf::from(data);
    }
    s
}

fn save_settings(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn ingest_ctx(s: &Settings) -> IngestCtx {
    // clean: false — the in-app ingest must never park the ~2GB on-device
    // model in memory implicitly; cached edits still get applied
    IngestCtx { data: s.data.clone(), width: s.width, clean: false, text_layer: true }
}

// ---------------------------------------------------------------------------
// engine init (background: store open + model load take seconds)
// ---------------------------------------------------------------------------

fn init_engine(app: AppHandle) {
    let settings = app.state::<AppState>().settings.clone();
    let fail = |msg: String| {
        eprintln!("engine init failed: {msg}");
        let _ = app.emit("app:error", &msg);
    };

    let t = Instant::now();
    // `Locked` usually means the launchd ingest worker is inside one of its
    // brief commit windows (which include an HNSW checkpoint — tens of
    // seconds on a big library), so retry before declaring failure.
    let deadline = Instant::now() + Duration::from_secs(90);
    let (lib, images) = loop {
        let opened = library_core::try_open(settings.data.join("library.db")).and_then(|lib| {
            library_core::try_open_images(settings.data.join("images.db"))
                .map(|images| (lib, images))
        });
        match opened {
            Ok(x) => break x,
            Err(fjall::Error::Locked) if Instant::now() < deadline => {
                let _ = app.emit(
                    "app:waiting",
                    "waiting for the background indexer to finish its commit…",
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(fjall::Error::Locked) => {
                return fail(format!(
                    "could not open the library stores in {} — is another instance \
                     or library-server running against the same data directory?",
                    settings.data.display()
                ));
            }
            Err(e) => {
                return fail(format!(
                    "could not open the library stores in {}: {e}",
                    settings.data.display()
                ));
            }
        }
    };
    println!("stores open in {:?}", t.elapsed());

    let t = Instant::now();
    let models = settings.data.join("models");
    // text queries embed with ese (no model object); only the CLIP text
    // encoder (shared space with figure crops) still loads
    let clip_text = match TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::ClipVitB32).with_cache_dir(models),
    ) {
        Ok(c) => c,
        Err(e) => return fail(format!("embedding model failed to load: {e}")),
    };
    println!("embedding model ready in {:?}", t.elapsed());

    let engine = std::sync::Arc::new(Engine {
        lib: RwLock::new(lib),
        images: RwLock::new(images),
        clip_text,
    });
    *app.state::<AppState>().engine.write().unwrap() = Some(engine);
    let _ = app.emit("app:ready", ());
}

/// Install/repair the launchd agent so ingestion continues while the app
/// is closed. Best-effort: a missing worker binary (e.g. a bare release
/// bundle without the sidecar) just logs and skips.
fn install_agent(data: &Path) {
    let candidates = [
        // bundled sidecar next to the app binary
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("library-ingest"))),
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

fn engine(state: &AppState) -> Result<std::sync::Arc<Engine>, String> {
    state
        .engine
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| "warming up".to_string())
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

fn answer(eng: &Engine, data: &Path, q: &Query) -> Response {
    let lib = eng.lib.read().unwrap();
    let images = eng.images.read().unwrap();
    library_core::answer(&lib, &images, data, q, |s| {
        eng.clip_text
            .embed(vec![s.to_string()], None)
            .ok()
            .and_then(|mut v| v.pop())
            .and_then(|v| v.try_into().ok())
    })
}

#[tauri::command]
async fn search(state: State<'_, AppState>, query: Query) -> Result<Response, String> {
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
async fn complete(
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
        let lib = eng.lib.read().unwrap();
        lib.rtx(|(_, (_, terms))| terms.complete_ranked(q, k.unwrap_or(8)))
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn ready(state: State<'_, AppState>) -> bool {
    state.engine.read().unwrap().is_some()
}

// ---------------------------------------------------------------------------
// chat: the librarian sidecar (apps/librarian) over stdio. The sidecar runs
// the Apple Foundation Models agent loop; its tool calls come back as
// `tool_request` lines and are executed in-process against the engine via
// the shared library_core::tools — the same implementations the server's
// HTTP routes use. Model sessions live in the sidecar, keyed by `conv`.
// ---------------------------------------------------------------------------

struct ChatBridge {
    child: std::process::Child,
    stdin: std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}

impl Drop for ChatBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn librarian_bin(app: &AppHandle) -> PathBuf {
    if let Ok(p) = std::env::var("LIBRARIAN_BIN") {
        return PathBuf::from(p);
    }
    // bundled resource in release, repo build at dev time
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join("librarian");
        if p.exists() {
            return p;
        }
    }
    dev_root().join("apps/librarian/.build/release/librarian")
}

fn spawn_chat(app: &AppHandle) -> Result<ChatBridge, String> {
    use std::io::BufRead;
    let bin = librarian_bin(app);
    // real collection names ride into the tool schema + instructions so the
    // model can scope searches without guessing
    let cols: Vec<String> = library_core::wire::read_collections(
        &app.state::<AppState>().settings.data,
    )
    .into_keys()
    .collect();
    let mut child = std::process::Command::new(&bin)
        .args(["serve", "--tools-stdin", "--collections", &cols.join(",")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("librarian sidecar failed to start ({}): {e}", bin.display()))?;
    let stdin = std::sync::Arc::new(std::sync::Mutex::new(child.stdin.take().expect("piped")));
    let mut lines = std::io::BufReader::new(child.stdout.take().expect("piped")).lines();
    match lines.next() {
        Some(Ok(l)) if l.contains("\"ready\"") => {}
        _ => return Err("librarian sidecar did not become ready".into()),
    }
    Ok(ChatBridge { child, stdin, lines })
}

fn execute_tool(eng: &Engine, data: &Path, name: &str, args: &serde_json::Value) -> String {
    use library_core::tools;
    let figure_search = |q: &str, col: &str| -> String {
        let member = match tools::resolve_collection(data, col) {
            Ok(m) => m,
            Err(e) => return e.to_string(),
        };
        let qemb: Option<ClipEmb> = eng
            .clip_text
            .embed(vec![q.to_string()], None)
            .ok()
            .and_then(|mut v| v.pop())
            .and_then(|v| v.try_into().ok());
        let found = qemb
            .map(|e| {
                eng.images.read().unwrap().rtx(|r| {
                    library_core::image_search(&r, &e, library_core::IMG_FETCH, member.as_ref())
                })
            })
            .unwrap_or_default();
        tools::image_hits_for_tool(&found, data, tools::TOOL_K).to_string()
    };
    let q = args["query"].as_str().unwrap_or("");
    let col = args["collection"].as_str().unwrap_or("");
    match name {
        "search_library" => {
            let lib = eng.lib.read().unwrap();
            lib.rtx(|r| tools::search_tool(&r, &lib, data, q, col, tools::TOOL_K)).to_string()
        }
        "search_figures" => figure_search(q, col),
        "sample_page" => {
            // sidecar-injected session state, not a model-visible param
            let avoid: Vec<String> = args["avoid"]
                .as_str()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            tools::sample_page_tool(data, col, None, &avoid).to_string()
        }
        "read_pages" => {
            let doc = args["doc"].as_str().unwrap_or("");
            let from = args["from"].as_u64().map(|n| n as u32);
            let to = args["to"].as_u64().map(|n| n as u32);
            tools::read_pages_tool(data, doc, from, to).to_string()
        }
        "library_overview" => tools::overview_tool(data).to_string(),
        "list_collections" => tools::collections_tool(data).to_string(),
        _ => serde_json::json!({ "error": format!("unknown tool {name:?}") }).to_string(),
    }
}

/// One chat turn: forwards sidecar events to the webview as `chat:event`,
/// executes tool requests in-process, returns at `turn_end`. Runs on the
/// blocking pool; a wedged model is recovered by `chat_cancel` (the stop
/// button), which the sidecar honors between stream snapshots.
fn chat_turn_blocking(app: AppHandle, conv: String, messages: serde_json::Value) -> Result<(), String> {
    use std::io::Write;

    let state = app.state::<AppState>();
    let eng = engine(&state)?;
    let data = state.settings.data.clone();

    let mut guard = state.chat.lock().unwrap();
    if guard.is_none() || guard.as_mut().is_some_and(|b| b.child.try_wait().is_ok_and(|s| s.is_some())) {
        let bridge = spawn_chat(&app)?;
        *state.chat_stdin.lock().unwrap() = Some(bridge.stdin.clone());
        *guard = Some(bridge);
    }
    // take the bridge out for the turn: any error path drops (and kills) the
    // child, a clean turn_end puts it back for the next turn
    let mut bridge = guard.take().expect("just spawned");

    let line = serde_json::json!({ "e": "turn", "conv": conv, "messages": messages });
    {
        let mut stdin = bridge.stdin.lock().unwrap();
        if writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_err() {
            return Err("could not reach the librarian sidecar".into());
        }
    }

    loop {
        match bridge.lines.next() {
            Some(Ok(line)) => {
                let ev: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                match ev["e"].as_str() {
                    Some("turn_end") => {
                        *guard = Some(bridge);
                        return Ok(());
                    }
                    Some("tool_request") => {
                        let result = execute_tool(&eng, &data, ev["name"].as_str().unwrap_or(""), &ev["args"]);
                        let resp = serde_json::json!({
                            "e": "tool_response", "id": ev["id"], "result": result,
                        });
                        let mut stdin = bridge.stdin.lock().unwrap();
                        if writeln!(stdin, "{resp}").and_then(|_| stdin.flush()).is_err() {
                            return Err("could not reach the librarian sidecar".into());
                        }
                    }
                    _ => {
                        let _ = app.emit("chat:event", line);
                    }
                }
            }
            _ => return Err("librarian sidecar exited early".into()), // EOF mid-turn
        }
    }
}

#[tauri::command]
async fn chat_turn(app: AppHandle, conv: String, messages: serde_json::Value) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || chat_turn_blocking(app, conv, messages))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn chat_cancel(state: State<'_, AppState>) {
    use std::io::Write;
    if let Some(stdin) = state.chat_stdin.lock().unwrap().as_ref() {
        let mut stdin = stdin.lock().unwrap();
        let _ = writeln!(stdin, "{}", serde_json::json!({ "e": "cancel" }));
        let _ = stdin.flush();
    }
}

// ---------------------------------------------------------------------------
// browse
// ---------------------------------------------------------------------------

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
fn collections(state: State<'_, AppState>) -> wire::Collections {
    wire::read_collections(&state.settings.data)
}

fn is_processing(st: Option<&DocStatus>) -> bool {
    matches!(
        st.map(|s| s.state),
        Some(DocState::Queued | DocState::Preparing | DocState::Staged)
    )
}

#[tauri::command]
fn docs(state: State<'_, AppState>) -> Vec<DocInfo> {
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
        if seen.contains(id)
            || matches!(st.state, DocState::Ready | DocState::Deleted)
        {
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

// ---------------------------------------------------------------------------
// library management
// ---------------------------------------------------------------------------

/// Set (or clear, with an empty/whitespace title) a doc's display title.
#[tauri::command]
fn set_title(state: State<'_, AppState>, doc: String, title: String) -> Result<(), String> {
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
fn set_collections(
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
async fn delete_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
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
fn retry_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    let data = &state.settings.data;
    if status::read(data, &doc).map(|s| s.state) != Some(DocState::Failed) {
        return Err("not in a failed state".into());
    }
    status::write(data, &doc, &DocStatus::new(DocState::Queued)).map_err(|e| e.to_string())?;
    let _ = state.wake.send(());
    Ok(())
}

// ---------------------------------------------------------------------------
// ingest
// ---------------------------------------------------------------------------

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
        Progress::Clean { done, total } => ("clean", done, total, String::new()),
        Progress::Embed { done, total } => ("embed", done, total, String::new()),
        Progress::Figures { done, total } => ("figures", done, total, String::new()),
        Progress::Clip { done, total } => ("clip", done, total, String::new()),
        Progress::Indexing => ("indexing", 0, 0, String::new()),
    };
    let _ = app.emit(
        "ingest:progress",
        &IngestEvent { doc: doc.to_string(), stage, done, total, message },
    );
}

fn emit_stage(app: &AppHandle, doc: &str, stage: &'static str) {
    let _ = app.emit(
        "ingest:progress",
        &IngestEvent { doc: doc.to_string(), stage, done: 0, total: 0, message: String::new() },
    );
}

/// Commits through the live engine's write locks — never `Locked`; searches
/// keep running between swaps.
struct EngineCommitter {
    eng: std::sync::Arc<Engine>,
}

impl Committer for EngineCommitter {
    fn text(
        &mut self,
        doc: &str,
        recs: &[library_core::ChunkRec],
    ) -> Result<(usize, usize), CommitErr> {
        let mut lib = self.eng.lib.write().unwrap();
        Ok(library_ingest::commit_text(&mut lib, doc, recs))
    }

    fn figures(
        &mut self,
        doc: &str,
        recs: &[library_core::ImageRec],
    ) -> Result<(usize, usize), CommitErr> {
        let mut images = self.eng.images.write().unwrap();
        Ok(library_ingest::commit_figures(&mut images, doc, recs))
    }
}

/// Sweep the filesystem queue until it's dry, then wait for a wake-up (a
/// new drop, a retry) or the periodic timeout. The periodic sweep is what
/// picks up work the CLI worker staged after this app instance launched
/// (see `library_ingest::worker` for the handoff race).
fn ingest_worker(app: AppHandle, rx: mpsc::Receiver<()>) {
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
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {
                while rx.try_recv().is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Accept dropped/picked PDFs: bring each into the library (`mode:
/// "move"` relocates the file; anything else copies), mark it queued, and
/// wake the worker. Returns the doc ids actually queued (dedup'd against
/// docs already in flight).
#[tauri::command]
fn ingest_paths(
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
        if path.extension().map(|e| !e.eq_ignore_ascii_case("pdf")).unwrap_or(true) {
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

// ---------------------------------------------------------------------------
// settings commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.clone()
}

#[tauri::command]
fn set_settings(app: AppHandle, s: Settings) -> Result<(), String> {
    // takes effect on next launch; live swap of the data dir isn't worth it
    save_settings(&app, &s)
}

// ---------------------------------------------------------------------------
// pages:// and ocr:// protocols
// ---------------------------------------------------------------------------

/// Serve files under `root` for a custom URI scheme. The CORS header matters:
/// the webview page's origin (dev server or tauri://) is cross-origin to
/// these schemes, so fetch() needs it even though everything is local.
fn serve_static(
    root: PathBuf,
    content_type: &'static str,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let not_found = || {
        tauri::http::Response::builder()
            .status(404)
            .header("access-control-allow-origin", "*")
            .body(Vec::new())
            .unwrap()
    };
    let raw = request.uri().path();
    let Ok(path) = percent_decode_str(raw).decode_utf8() else {
        return not_found();
    };
    let rel = Path::new(path.trim_start_matches('/'));
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return not_found();
    }
    match std::fs::read(root.join(rel)) {
        Ok(bytes) => tauri::http::Response::builder()
            .status(200)
            .header("content-type", content_type)
            .header("cache-control", "public, max-age=31536000, immutable")
            .header("access-control-allow-origin", "*")
            .body(bytes)
            .unwrap(),
        Err(_) => not_found(),
    }
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .register_asynchronous_uri_scheme_protocol("pages", |ctx, request, responder| {
            let data = ctx
                .app_handle()
                .state::<AppState>()
                .settings
                .data
                .clone();
            // a search response can carry ~20-26 hits, each firing a page-image
            // request — spawn_blocking uses tokio's bounded, reused blocking
            // pool instead of a raw OS thread per request, which used to mean
            // a burst of thread creations on every keystroke's re-render
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_static(data.join("pages"), "image/jpeg", request))
            });
        })
        .register_asynchronous_uri_scheme_protocol("ocr", |ctx, request, responder| {
            let data = ctx
                .app_handle()
                .state::<AppState>()
                .settings
                .data
                .clone();
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
            search,
            complete,
            ready,
            collections,
            docs,
            set_title,
            set_collections,
            delete_doc,
            retry_doc,
            ingest_paths,
            get_settings,
            set_settings,
            chat_turn,
            chat_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
