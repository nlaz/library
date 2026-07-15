//! The Library server: WebTransport (QUIC) search + HTTP static/assets.
//!
//! Protocol:
//!   client -> server  datagrams   {"seq": u64, "q": "...", "mode": "instant"|"full"}
//!   server -> client  uni streams one JSON message per answered query:
//!                                 {"seq", "phase": "lex"|"hybrid", "hits": [...]}
//!
//! Datagrams are the right fit for keystrokes: every query supersedes the
//! previous one, so losing a stale one costs nothing. Each response rides its
//! own uni stream, so a slow/large result can never head-of-line-block a
//! newer one. The client drops any message whose seq is older than the last
//! one it rendered.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{Json, Router, extract::Path as UrlPath, routing::get, routing::post};
use clap::Parser;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::wire::{count_pages, read_collections};
use library_core::{ClipEmb, Images, Library, Query};
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use wtransport::{Endpoint, Identity, ServerConfig};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "data")]
    data: PathBuf,
    /// Directory of built web assets to serve at `/`.
    #[arg(long, default_value = "apps/web/dist")]
    web: PathBuf,
    #[arg(long, default_value_t = 4433)]
    wt_port: u16,
    #[arg(long, default_value_t = 8080)]
    http_port: u16,
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    /// "all" | "text" | "images" (empty = "all")
    #[serde(default)]
    kind: String,
    #[serde(default)]
    col: String,
    k: Option<usize>,
}

#[derive(Deserialize)]
struct TextParams {
    from: Option<u32>,
    to: Option<u32>,
}

#[derive(Deserialize)]
struct SampleParams {
    #[serde(default)]
    col: String,
    seed: Option<u64>,
    /// CSV of "doc:page" recently served to this session (sidecar-injected,
    /// never model-visible) so repeat sampling walks new shelves.
    #[serde(default)]
    avoid: String,
}

struct App {
    lib: Library,
    images: Images,
    /// CLIP text encoder: embeds queries into the shared text/image space
    /// for figure search. Text-chunk queries use ese (no model object).
    clip: TextEmbedding,
    data: PathBuf,
}

impl App {
    fn answer(&self, q: &Query) -> library_core::wire::Response {
        library_core::answer(&self.lib, &self.images, &self.data, q, |s| {
            self.clip
                .embed(vec![s.to_string()], None)
                .ok()
                .and_then(|mut v| v.pop())
                .and_then(|v| v.try_into().ok())
        })
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let t = Instant::now();
    let lib = library_core::open(args.data.join("library.db"));
    let n = lib.rtx(|((_, vec), _)| vec.len());
    println!("store open: {n} chunks, {:?} (incl. hnsw rebuild)", t.elapsed());

    let t = Instant::now();
    let images = library_core::open_images(args.data.join("images.db"));
    let n = images.rtx(|(vec, _)| vec.len());
    println!("image store open: {n} figures, {:?}", t.elapsed());

    let t = Instant::now();
    let clip = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::ClipVitB32).with_cache_dir(args.data.join("models")),
    )?;
    println!("embedding model ready (clip-text; ese needs no load): {:?}", t.elapsed());

    let app = Arc::new(App { lib, images, clip, data: args.data.clone() });

    // real collection names ride into the sidecar's tool schema +
    // instructions (Sidecar::spawn has no data-dir access of its own)
    let _ = SIDECAR_COLLECTIONS
        .set(read_collections(&args.data).into_keys().collect::<Vec<_>>().join(","));

    // --- WebTransport endpoint ---------------------------------------------
    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_hash: Vec<u8> = identity.certificate_chain().as_slice()[0]
        .hash()
        .as_ref()
        .to_vec();

    let wt_config = ServerConfig::builder()
        .with_bind_default(args.wt_port)
        .with_identity(identity)
        .build();
    let endpoint = Endpoint::server(wt_config)?;
    println!("webtransport: https://127.0.0.1:{}", args.wt_port);

    // --- HTTP: web app, page images, cert hash ------------------------------
    let http = Router::new()
        .route("/api/cert_hash", get({
            let h = cert_hash.clone();
            move || async move { Json(h) }
        }))
        .route("/api/collections", get({
            let data = args.data.clone();
            move || async move { Json(read_collections(&data)) }
        }))
        // plain-JSON search for programmatic callers (the chat sidecar's
        // search_library tool, the eval harness). Delegates to the shared
        // agent tools in library_core::tools: complete=false, absolute
        // confidence, top-hit page text — this feeds a 4k-context model.
        .route("/api/search", get({
            let app = app.clone();
            move |axum::extract::Query(p): axum::extract::Query<SearchParams>| {
                let app = app.clone();
                async move {
                    let out = tokio::task::spawn_blocking(move || {
                        let k = p.k.unwrap_or(library_core::tools::TOOL_K);
                        if p.kind == "images" {
                            let member = match library_core::tools::resolve_collection(
                                &app.data, &p.col,
                            ) {
                                Ok(m) => m,
                                Err(e) => return e,
                            };
                            let qemb: Option<ClipEmb> = app
                                .clip
                                .embed(vec![p.q.clone()], None)
                                .ok()
                                .and_then(|mut v| v.pop())
                                .and_then(|v| v.try_into().ok());
                            let found = qemb
                                .map(|e| {
                                    app.images.rtx(|r| {
                                        library_core::image_search(
                                            &r,
                                            &e,
                                            library_core::IMG_FETCH,
                                            member.as_ref(),
                                        )
                                    })
                                })
                                .unwrap_or_default();
                            library_core::tools::image_hits_for_tool(&found, &app.data, k)
                        } else {
                            app.lib.rtx(|r| {
                                library_core::tools::search_tool(
                                    &r, &app.lib, &app.data, &p.q, &p.col, k,
                                )
                            })
                        }
                    })
                    .await
                    .expect("search task panicked");
                    Json(out)
                }
            }
        }))
        // chat agent: relay the librarian sidecar's NDJSON as SSE. The
        // sidecar (apps/librarian) runs the Apple Foundation Models agent
        // loop; its tools call back into /api/search and /api/text here.
        .route("/api/chat", post(chat))
        // reading-order text, sliced by page — what an agent reads after
        // search points it at a page. Capped small: the reader is a model
        // with a 4k-token context, and errors go back as content it can act
        // on, never a bare status code.
        .route("/api/text/{doc}", get({
            let data = args.data.clone();
            move |UrlPath(doc): UrlPath<String>,
                  axum::extract::Query(p): axum::extract::Query<TextParams>| {
                let data = data.clone();
                async move {
                    Json(library_core::tools::read_pages_tool(&data, &doc, p.from, p.to))
                }
            }
        }))
        // a random readable page — the browse affordance behind the sidecar's
        // sample_page tool ("tell me something interesting"). `seed` is a
        // test hook for the eval harness.
        .route("/api/sample", get({
            let data = args.data.clone();
            move |axum::extract::Query(p): axum::extract::Query<SampleParams>| {
                let data = data.clone();
                async move {
                    let avoid: Vec<String> = p
                        .avoid
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    Json(library_core::tools::sample_page_tool(&data, &p.col, p.seed, &avoid))
                }
            }
        }))
        // metadata for the reader drawer in the plain-web build (read-only;
        // the desktop build gets the same facts from the `docs` command)
        .route("/api/doc/{doc}", get({
            let data = args.data.clone();
            move |UrlPath(doc): UrlPath<String>| {
                let data = data.clone();
                async move {
                    let titles: std::collections::BTreeMap<String, String> =
                        std::fs::read(data.join("titles.json"))
                            .ok()
                            .and_then(|b| serde_json::from_slice(&b).ok())
                            .unwrap_or_default();
                    let collections: Vec<String> = read_collections(&data)
                        .into_iter()
                        .filter(|(_, docs)| docs.iter().any(|d| d == &doc))
                        .map(|(name, _)| name)
                        .collect();
                    let status: serde_json::Value =
                        std::fs::read(data.join("status").join(format!("{doc}.json")))
                            .ok()
                            .and_then(|b| serde_json::from_slice(&b).ok())
                            .unwrap_or(serde_json::Value::Null);
                    Json(serde_json::json!({
                        "id": doc,
                        "title": titles.get(&doc),
                        "pages": count_pages(&data.join("pages").join(&doc)),
                        "collections": collections,
                        "status": status,
                    }))
                }
            }
        }))
        // the reader has no other way to learn a doc's page count in the
        // plain-web build (the desktop build gets it for free from `docs`)
        .route("/api/pages/{doc}", get({
            let data = args.data.clone();
            move |UrlPath(doc): UrlPath<String>| {
                let data = data.clone();
                async move {
                    Json(serde_json::json!({ "pages": count_pages(&data.join("pages").join(doc)) }))
                }
            }
        }))
        .nest_service("/pages", ServeDir::new(args.data.join("pages")))
        .nest_service("/ocr", ServeDir::new(args.data.join("ocr")))
        .fallback_service(ServeDir::new(&args.web))
        .layer(CorsLayer::permissive());
    let addr = SocketAddr::from(([127, 0, 0, 1], args.http_port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("http: http://{addr}");
    tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });

    // --- accept loop ---------------------------------------------------------
    loop {
        let incoming = endpoint.accept().await;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_session(incoming, app).await {
                eprintln!("session ended: {e:#}");
            }
        });
    }
}

/// Chat rides a persistent `librarian serve` sidecar: sessions live in the
/// sidecar (warm model, native AFM transcripts per conversation), the server
/// serializes turns through a Mutex and relays NDJSON events as SSE. Client
/// disconnect mid-turn sends a `cancel` line and drains to `turn_end` so the
/// next turn never reads stale events. A wedged or dead child is dropped and
/// respawned on the next turn.
const CHAT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Collection names for the sidecar's --collections flag, set once in main.
static SIDECAR_COLLECTIONS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

struct Sidecar {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl Sidecar {
    async fn spawn() -> std::io::Result<Sidecar> {
        use tokio::io::AsyncBufReadExt;
        let bin = std::env::var("LIBRARIAN_BIN")
            .unwrap_or_else(|_| "apps/librarian/.build/release/librarian".into());
        let mut child = tokio::process::Command::new(&bin)
            .arg("serve")
            .args(
                SIDECAR_COLLECTIONS
                    .get()
                    .filter(|c| !c.is_empty())
                    .map(|c| vec!["--collections".to_string(), c.clone()])
                    .unwrap_or_default(),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let mut lines =
            tokio::io::BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        // wait for the ready line (model prewarm happens behind it)
        match tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line()).await {
            Ok(Ok(Some(l))) if l.contains("\"ready\"") => {}
            _ => {
                return Err(std::io::Error::other("librarian sidecar did not become ready"));
            }
        }
        Ok(Sidecar { child, stdin, lines })
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

type SharedSidecar = Arc<tokio::sync::Mutex<Option<Sidecar>>>;

fn sidecar_slot() -> SharedSidecar {
    static SLOT: std::sync::OnceLock<SharedSidecar> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(tokio::sync::Mutex::new(None))).clone()
}

async fn chat(body: String) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use tokio::io::AsyncWriteExt;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let err_event = |msg: String| {
        Ok(Event::default()
            .event("error")
            .data(serde_json::json!({ "e": "error", "message": msg }).to_string()))
    };

    tokio::spawn(async move {
        let slot = sidecar_slot();
        let mut guard = slot.lock().await; // serializes turns
        if guard.as_mut().is_none_or(|s| !s.alive()) {
            *guard = match Sidecar::spawn().await {
                Ok(s) => Some(s),
                Err(e) => {
                    let _ = tx.send(err_event(format!("librarian sidecar failed to start: {e}"))).await;
                    return;
                }
            };
        }
        let sidecar = guard.as_mut().expect("just spawned");

        // request line: the turn envelope wrapping the client body
        let req: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let line = serde_json::json!({
            "e": "turn",
            "conv": req.get("conv").and_then(|c| c.as_str()).unwrap_or("default"),
            "messages": req.get("messages").cloned().unwrap_or(serde_json::json!([])),
        });
        if sidecar.stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
            *guard = None; // dead child; next turn respawns
            let _ = tx.send(err_event("could not reach the librarian sidecar".into())).await;
            return;
        }

        let deadline = tokio::time::Instant::now() + CHAT_DEADLINE;
        let mut client_gone = false;
        loop {
            match tokio::time::timeout_at(deadline, sidecar.lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let name = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|v| v.get("e").and_then(|e| e.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "message".into());
                    if name == "turn_end" {
                        return; // turn complete; keep the child for the next one
                    }
                    if !client_gone
                        && tx.send(Ok(Event::default().event(name).data(line))).await.is_err()
                    {
                        // client disconnected: cancel, then drain to turn_end
                        client_gone = true;
                        let _ = sidecar.stdin.write_all(b"{\"e\":\"cancel\"}\n").await;
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) => {
                    *guard = None; // EOF/read error: child is gone
                    let _ = tx.send(err_event("librarian sidecar exited early".into())).await;
                    return;
                }
                Err(_) => {
                    *guard = None; // wedged: drop it, kill_on_drop reaps
                    let _ = tx.send(err_event("chat turn timed out".into())).await;
                    return;
                }
            }
        }
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

async fn serve_session(
    incoming: wtransport::endpoint::IncomingSession,
    app: Arc<App>,
) -> Result<()> {
    let request = incoming.await?;
    let conn = request.accept().await?;
    println!("session from {}", conn.remote_address());

    loop {
        let dgram = conn.receive_datagram().await?;
        let q: Query = match serde_json::from_slice(&dgram) {
            Ok(q) => q,
            Err(_) => continue,
        };

        let resp = {
            let app = app.clone();
            // embedding + search are sync; keep the event loop clean
            tokio::task::spawn_blocking(move || app.answer(&q)).await?
        };

        let mut stream = conn.open_uni().await?.await?;
        stream.write_all(&serde_json::to_vec(&resp)?).await?;
        stream.finish().await?;
    }
}
