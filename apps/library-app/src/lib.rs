//! The Library as a Tauri desktop app.
//!
//! Everything runs in this one process: the fold stores, the embedding
//! models, and the ingest pipeline. Searches take a read lock; the ingest
//! worker does OCR/embedding lock-free and takes the write lock only for the
//! brief atomic swap — so the app can search while it ingests, and the fjall
//! single-writer lock is never contended.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, mpsc};
use std::time::Instant;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::wire::{self, Response, WireHit};
use library_core::{ClipEmb, Emb, FxHashSet, Images, Library, tokenize};
use library_ingest::{IngestCtx, Progress};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

const K: usize = 20;
/// Image hits appended to an "all" response (kind=images gets a full K).
const K_IMG_BLEND: usize = 6;

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

struct Job {
    pdf: PathBuf,
    doc: String,
    collection: Option<String>,
}

pub struct AppState {
    settings: Settings,
    engine: RwLock<Option<std::sync::Arc<Engine>>>,
    jobs: mpsc::Sender<Job>,
    /// Doc ids queued or mid-ingest; read by `docs`, guards double drops.
    pending: Mutex<HashSet<String>>,
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
    IngestCtx { data: s.data.clone(), width: s.width, clean: false }
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
    // fjall panics with `Locked` if another process (e.g. a dev
    // library-server) holds the store — turn that into a message, not a crash
    let opened = catch_unwind(AssertUnwindSafe(|| {
        let lib = library_core::open(settings.data.join("library.db"));
        let images = library_core::open_images(settings.data.join("images.db"));
        (lib, images)
    }));
    let (lib, images) = match opened {
        Ok(x) => x,
        Err(_) => {
            return fail(format!(
                "could not open the library stores in {} — is another instance \
                 or library-server running against the same data directory?",
                settings.data.display()
            ));
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

#[derive(Deserialize)]
pub struct Query {
    pub seq: u64,
    pub q: String,
    /// "instant" = lexical only (every keystroke), "full" = hybrid (debounced)
    #[serde(default)]
    pub mode: String,
    /// restrict to a collection from data/collections.json; empty = everything
    #[serde(default)]
    pub col: String,
    /// "all" | "text" | "images" (empty = "all")
    #[serde(default)]
    pub kind: String,
}

fn answer(eng: &Engine, data: &Path, q: &Query) -> Response {
    let start = Instant::now();
    let want_text = q.kind != "images";
    let want_imgs = q.kind == "images" || (q.kind != "text" && q.mode == "full");

    // collection filter, pushed down into every ranker
    let member: Option<FxHashSet<String>> = (!q.col.is_empty())
        .then(|| wire::read_collections(data).remove(&q.col))
        .flatten()
        .map(|docs| docs.into_iter().collect());

    let mut phase = "lex";
    let mut hits: Vec<WireHit> = Vec::new();

    if want_text {
        let qemb: Option<Emb> = (q.mode == "full").then(|| ese::encode_single(&q.q));
        if qemb.is_some() {
            phase = "hybrid";
        }
        let qtoks = tokenize(&q.q);
        let found = eng.lib.read().unwrap().rtx(|r| {
            library_core::search(&r, &q.q, qemb.as_ref(), K, member.as_ref())
        });
        hits.extend(found.iter().map(|h| wire::wire_hit(h, &qtoks)));
    }

    if want_imgs {
        let k = if q.kind == "images" { K } else { K_IMG_BLEND };
        let qemb: Option<ClipEmb> = eng
            .clip_text
            .embed(vec![q.q.clone()], None)
            .ok()
            .and_then(|mut v| v.pop())
            .and_then(|v| v.try_into().ok());
        if let Some(e) = qemb {
            phase = if want_text { "hybrid+img" } else { "img" };
            let found = eng
                .images
                .read()
                .unwrap()
                .rtx(|r| library_core::image_search(&r, &e, 40, member.as_ref()));
            hits.extend(wire::group_image_hits(&found, k));
        }
    }

    Response { seq: q.seq, phase, us: start.elapsed().as_micros(), hits }
}

#[tauri::command]
async fn search(state: State<'_, AppState>, query: Query) -> Result<Response, String> {
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
    tauri::async_runtime::spawn_blocking(move || answer(&eng, &data, &query))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn ready(state: State<'_, AppState>) -> bool {
    state.engine.read().unwrap().is_some()
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
    pub processing: bool,
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
    let tmp = data.join("titles.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(titles).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, data.join("titles.json")).map_err(|e| e.to_string())
}

#[tauri::command]
fn collections(state: State<'_, AppState>) -> wire::Collections {
    wire::read_collections(&state.settings.data)
}

#[tauri::command]
fn docs(state: State<'_, AppState>) -> Vec<DocInfo> {
    let data = &state.settings.data;
    let cols = wire::read_collections(data);
    let titles = read_titles(data);
    let pending = state.pending.lock().unwrap();

    let mut out: Vec<DocInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(data.join("pages")) {
        for e in entries.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = e.file_name().to_string_lossy().into_owned();
            let pages = std::fs::read_dir(e.path())
                .map(|it| {
                    it.flatten()
                        .filter(|f| {
                            let n = f.file_name();
                            let n = n.to_string_lossy();
                            n.starts_with("page-") && n.ends_with(".jpg")
                        })
                        .count() as u32
                })
                .unwrap_or(0);
            seen.insert(id.clone());
            out.push(DocInfo {
                pages,
                title: titles.get(&id).cloned(),
                collections: cols
                    .iter()
                    .filter(|(_, docs)| docs.iter().any(|d| *d == id))
                    .map(|(c, _)| c.clone())
                    .collect(),
                processing: pending.contains(&id),
                id,
            });
        }
    }
    // queued docs whose pages dir doesn't exist yet still get a card
    for id in pending.iter() {
        if !seen.contains(id) {
            out.push(DocInfo {
                id: id.clone(),
                title: titles.get(id).cloned(),
                pages: 0,
                collections: Vec::new(),
                processing: true,
            });
        }
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
/// data/pdfs is kept so the doc can be re-ingested later.
#[tauri::command]
async fn delete_doc(state: State<'_, AppState>, doc: String) -> Result<(), String> {
    if state.pending.lock().unwrap().contains(&doc) {
        return Err("still processing — try again when ingest finishes".into());
    }
    let eng = engine(&state)?;
    let data = state.settings.data.clone();
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
    };
    let _ = app.emit(
        "ingest:progress",
        &IngestEvent { doc: doc.to_string(), stage, done, total, message },
    );
}

/// One PDF, start to finish. The write locks are held only around the two
/// atomic swaps; everything slow happens against no store at all.
fn run_job(app: &AppHandle, job: &Job) -> anyhow::Result<()> {
    let state = app.state::<AppState>();
    let ctx = ingest_ctx(&state.settings);
    let doc = &job.doc;

    // engine must be up before we can embed (models are shared)
    let eng = loop {
        if let Some(e) = state.engine.read().unwrap().clone() {
            break e;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };

    let recs = library_ingest::prepare_text(&ctx, &job.pdf, doc, None, &mut |p| {
        emit_progress(app, doc, p)
    })?;

    emit_stage(app, doc, "indexing");
    {
        let mut lib = eng.lib.write().unwrap();
        library_ingest::commit_text(&mut lib, doc, &recs);
    }

    let figs = library_ingest::prepare_figures(&ctx, doc, &mut |p| emit_progress(app, doc, p))?;

    emit_stage(app, doc, "indexing");
    {
        let mut images = eng.images.write().unwrap();
        library_ingest::commit_figures(&mut images, doc, &figs);
    }

    if let Some(col) = &job.collection {
        library_ingest::collect(&ctx.data, col, doc)?;
    }
    Ok(())
}

fn emit_stage(app: &AppHandle, doc: &str, stage: &'static str) {
    let _ = app.emit(
        "ingest:progress",
        &IngestEvent { doc: doc.to_string(), stage, done: 0, total: 0, message: String::new() },
    );
}

fn ingest_worker(app: AppHandle, rx: mpsc::Receiver<Job>) {
    // utility QoS for this thread only (Vision OCR and ort inherit it);
    // the GUI stays at full priority
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    while let Ok(job) = rx.recv() {
        let result = run_job(&app, &job);
        let state = app.state::<AppState>();
        state.pending.lock().unwrap().remove(&job.doc);
        match result {
            Ok(()) => emit_stage(&app, &job.doc, "done"),
            Err(e) => {
                eprintln!("ingest '{}' failed: {e:#}", job.doc);
                let _ = app.emit(
                    "ingest:progress",
                    &IngestEvent {
                        doc: job.doc.clone(),
                        stage: "error",
                        done: 0,
                        total: 0,
                        message: format!("{e:#}"),
                    },
                );
            }
        }
    }
}

/// Accept dropped/picked PDFs: copy each into the library and queue it.
/// Returns the doc ids actually queued (dedup'd against in-flight jobs).
#[tauri::command]
fn ingest_paths(
    state: State<'_, AppState>,
    paths: Vec<String>,
    collection: Option<String>,
) -> Result<Vec<String>, String> {
    let ctx = ingest_ctx(&state.settings);
    let mut queued = Vec::new();
    for p in paths {
        let path = PathBuf::from(&p);
        if path.extension().map(|e| !e.eq_ignore_ascii_case("pdf")).unwrap_or(true) {
            continue;
        }
        let (doc, copy) = library_ingest::add_pdf(&ctx, &path, None).map_err(|e| e.to_string())?;
        {
            let mut pending = state.pending.lock().unwrap();
            if !pending.insert(doc.clone()) {
                continue; // already queued or mid-ingest
            }
        }
        state
            .jobs
            .send(Job { pdf: copy, doc: doc.clone(), collection: collection.clone() })
            .map_err(|e| e.to_string())?;
        queued.push(doc);
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
            std::thread::spawn(move || {
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
            std::thread::spawn(move || {
                responder.respond(serve_static(data.join("ocr"), "application/json", request))
            });
        })
        .setup(|app| {
            let settings = load_settings(app.handle());
            let (tx, rx) = mpsc::channel::<Job>();
            app.manage(AppState {
                settings,
                engine: RwLock::new(None),
                jobs: tx,
                pending: Mutex::new(HashSet::new()),
            });

            let h = app.handle().clone();
            std::thread::spawn(move || init_engine(h));
            let h = app.handle().clone();
            std::thread::spawn(move || ingest_worker(h, rx));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search,
            ready,
            collections,
            docs,
            set_title,
            set_collections,
            delete_doc,
            ingest_paths,
            get_settings,
            set_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
