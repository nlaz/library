//! The engine: fold stores + embedding model behind read/write locks, and
//! the app state that owns it.

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock, mpsc};
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
    /// Loads in the background *after* `app:ready` — empty means still
    /// warming, and hybrid answers degrade to text-only until it lands.
    pub(crate) clip_text: OnceLock<TextEmbedding>,
}

pub struct AppState {
    pub(crate) settings: Settings,
    /// Cache dir + metadata db, opened once at startup and shared by every
    /// command, the ingest worker and the chat tools.
    pub(crate) ctx: library_core::meta::Ctx,
    /// Serves `pages://`, and repairs a cache miss by re-rendering. Built
    /// once so its render gate and read-recency throttle are shared across
    /// every request rather than per-request state.
    pub(crate) pages: crate::serve::PageServer,
    pub(crate) engine: RwLock<Option<std::sync::Arc<Engine>>>,
    /// Last launch-screen status, latched so a late webview can catch up.
    pub(crate) status: std::sync::Mutex<Status>,
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

/// What the launch screen is showing. One event type for the whole startup
/// sequence; the frontend (launch-model.ts) owns how the steps are drawn,
/// including the labels for steps not yet reached.
#[derive(Clone, serde::Serialize)]
pub(crate) struct Status {
    /// Machine-readable step, in order: `stores` | `layout` | `clip`, then
    /// `ready` when the sequence is done.
    pub(crate) step: &'static str,
    /// One clause, already user-facing. Only rendered for a step the
    /// frontend doesn't recognise; otherwise it is a log aid.
    pub(crate) detail: String,
    /// Bytes, while a model is downloading; zero on an indeterminate step.
    pub(crate) done: u64,
    pub(crate) total: u64,
}

impl Default for Status {
    /// The state before `init_engine` has run a line: the window is up and
    /// the stores are about to open.
    fn default() -> Status {
        Status::step("stores", "Opening the library")
    }
}

impl Status {
    fn step(step: &'static str, detail: &str) -> Status {
        Status {
            step,
            detail: detail.to_string(),
            done: 0,
            total: 0,
        }
    }
}

/// Emit a launch-screen status **and** latch it on the state.
///
/// The latch is what makes the frontend's subscription raceable: with a warm
/// cache the whole sequence can finish before the webview has run a line of
/// JavaScript, and a launch screen that only listens would then wait forever
/// for an event already sent. `startup_status` hands over the last one.
fn status(app: &AppHandle, s: Status) {
    if let Ok(mut slot) = app.state::<AppState>().status.lock() {
        *slot = s.clone();
    }
    let _ = app.emit("app:status", &s);
}

pub(crate) fn init_engine(app: AppHandle) {
    let settings = app.state::<AppState>().settings.clone();
    let fail = |msg: String| {
        eprintln!("engine init failed: {msg}");
        let _ = app.emit("app:error", &msg);
    };

    status(&app, Status::step("stores", "Opening the library"));
    let t = Instant::now();
    // `Locked` means another process holds the stores — the `library-ingest`
    // CLI inside one of its brief commit windows (which include an HNSW
    // checkpoint, tens of seconds on a big library), so retry before
    // declaring failure.
    let deadline = Instant::now() + Duration::from_secs(90);
    let (mut lib, images) = loop {
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

    // one-time: noted annotations become notebox cards; log-and-continue —
    // a failed migration must not brick startup (no marker means the next
    // launch retries)
    match library_core::annots::migrate_annots_to_cards(
        &mut lib,
        &app.state::<AppState>().ctx,
        &|s| ese::encode_single(s),
    ) {
        Ok(0) => {}
        Ok(n) => println!("migrated {n} margin notes into cards"),
        Err(e) => eprintln!("annotation migration skipped: {e}"),
    }

    let engine = std::sync::Arc::new(Engine {
        lib: RwLock::new(lib),
        images: RwLock::new(images),
        clip_text: OnceLock::new(),
    });
    *app.state::<AppState>()
        .engine
        .write()
        .expect("engine slot lock poisoned") = Some(engine.clone());
    let _ = app.emit("app:ready", ());

    // Everything past this point is models, and readiness does not wait for
    // any of it: text queries embed with ese (no model object), so search
    // works the moment the stores open. That is why `app:ready` fires above —
    // the launch screen stays up for the downloads, but it can offer a way in.
    //
    // On a machine that has never run this app, both steps below *fetch*
    // rather than load: together they are the one part of startup measured in
    // minutes. The layout detector goes first because it is the smaller of
    // the two and the one nothing else would ever fetch — doing it here
    // rather than at first ingest means the launch screen owns every model
    // wait the app has, and the first document added is indexed at full
    // quality instead of falling back to word-gap figure detection.
    let t = Instant::now();
    let models = settings.data.join("models");
    status(
        &app,
        Status::step("layout", "Fetching the page-layout model"),
    );
    if let Err(e) = library_ingest::models::ensure_layout(&settings.data, |done, total| {
        status(
            &app,
            Status {
                step: "layout",
                detail: "Fetching the page-layout model".into(),
                done,
                total,
            },
        );
    }) {
        // not fatal: ingestion falls back to word-gap figure detection
        eprintln!("page-layout model unavailable: {e:#}");
    } else {
        println!("layout model ready in {:?}", t.elapsed());
    }

    let t = Instant::now();
    status(&app, Status::step("clip", "Preparing figure search"));
    let loaded = library_ingest::models::watch_download(
        &models,
        library_ingest::models::CLIP_TEXT_BYTES,
        || {
            TextEmbedding::try_new(
                InitOptions::new(EmbeddingModel::ClipVitB32).with_cache_dir(models.clone()),
            )
        },
        |done, total| {
            status(
                &app,
                Status {
                    step: "clip",
                    detail: "Fetching the figure-search model".into(),
                    done,
                    total,
                },
            );
        },
    );
    match loaded {
        Ok(c) => {
            let _ = engine.clip_text.set(c);
            println!("embedding model ready in {:?}", t.elapsed());
        }
        Err(e) => fail(format!(
            "figure search unavailable — embedding model failed to load: {e}"
        )),
    }

    // Last, and biggest: the image encoder. Nothing on this screen needs it —
    // it is what *indexes* pictures, not what searches them — but fetching it
    // here is what keeps it off the critical path of the first document,
    // where it would otherwise appear as a stalled progress bar partway
    // through an ingest. Cached and dropped, not held: see `ensure_clip_vision`.
    let t = Instant::now();
    status(
        &app,
        Status::step("vision", "Fetching the figure-indexing model"),
    );
    if let Err(e) = library_ingest::models::ensure_clip_vision(&settings.data, |done, total| {
        status(
            &app,
            Status {
                step: "vision",
                detail: "Fetching the figure-indexing model".into(),
                done,
                total,
            },
        );
    }) {
        // not fatal: ingest fetches it itself, it just costs the wait later
        eprintln!("figure-indexing model not pre-fetched: {e:#}");
    } else {
        println!("image encoder cached in {:?}", t.elapsed());
    }

    status(&app, Status::step("ready", ""));
}

pub(crate) fn engine(state: &AppState) -> Result<std::sync::Arc<Engine>, String> {
    state
        .engine
        .read()
        .expect("engine slot lock poisoned")
        .clone()
        .ok_or_else(|| "warming up".to_string())
}

pub(crate) fn answer(eng: &Engine, ctx: &library_core::meta::Ctx, q: &Query) -> Response {
    let lib = eng.lib.read().expect("library lock poisoned");
    let images = eng.images.read().expect("images lock poisoned");
    library_core::answer(&lib, &images, ctx, q, |s| {
        eng.clip_text
            .get()?
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
        let mut lib = self.eng.lib.write().expect("library lock poisoned");
        Ok(library_ingest::commit_text(&mut lib, doc, recs))
    }

    fn figures(
        &mut self,
        doc: &str,
        recs: &[library_core::ImageRec],
    ) -> Result<(usize, usize), CommitErr> {
        let mut images = self.eng.images.write().expect("images lock poisoned");
        Ok(library_ingest::commit_figures(&mut images, doc, recs))
    }
}
