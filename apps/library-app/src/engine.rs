//! The engine: fold stores + embedding model behind read/write locks, and
//! the app state that owns it.

use std::path::{Path, PathBuf};
use std::sync::{RwLock, mpsc};
use std::time::{Duration, Instant};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::wire::Response;
use library_core::{Images, Library, Query};
use library_ingest::worker::{CommitErr, Committer};
use tauri::{AppHandle, Emitter, Manager};

use crate::chat::ChatBridge;
use crate::settings::Settings;

pub struct Engine {
    pub(crate) lib: RwLock<Library>,
    pub(crate) images: RwLock<Images>,
    /// CLIP text encoder for figure search; text queries embed with ese.
    pub(crate) clip_text: TextEmbedding,
}

pub struct AppState {
    pub(crate) settings: Settings,
    pub(crate) engine: RwLock<Option<std::sync::Arc<Engine>>>,
    /// Wakes the worker thread for an immediate sweep; what to ingest
    /// comes from the status files, not the channel.
    pub(crate) wake: mpsc::Sender<()>,
    /// The librarian chat sidecar (AFM agent loop). The outer Mutex
    /// serializes turns; `chat_stdin` is shared separately so `chat_cancel`
    /// can write while a turn holds the bridge.
    pub(crate) chat: std::sync::Mutex<Option<ChatBridge>>,
    pub(crate) chat_stdin:
        std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>>>,
}

/// Repo root at dev time; the bundle has no repo, so release builds rely on
/// settings.json / LIBRARY_DATA / resources instead.
pub(crate) fn dev_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------------------
// engine init (background: store open + model load take seconds)
// ---------------------------------------------------------------------------

pub(crate) fn init_engine(app: AppHandle) {
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

pub(crate) fn engine(state: &AppState) -> Result<std::sync::Arc<Engine>, String> {
    state
        .engine
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| "warming up".to_string())
}

pub(crate) fn answer(eng: &Engine, data: &Path, q: &Query) -> Response {
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

/// Commits through the live engine's write locks — never `Locked`; searches
/// keep running between swaps.
pub(crate) struct EngineCommitter {
    pub(crate) eng: std::sync::Arc<Engine>,
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
