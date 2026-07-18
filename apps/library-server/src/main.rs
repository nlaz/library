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
use axum::{Json, Router, extract::Path as UrlPath, routing::get, routing::post};
use clap::Parser;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::wire::{count_pages, read_collections};
use library_core::{ClipEmb, Images, Library, Query};
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

mod chat;
mod wt;

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
struct CompleteParams {
    q: String,
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
    println!(
        "store open: {n} chunks, {:?} (incl. hnsw rebuild)",
        t.elapsed()
    );

    let t = Instant::now();
    let images = library_core::open_images(args.data.join("images.db"));
    let n = images.rtx(|(vec, _)| vec.len());
    println!("image store open: {n} figures, {:?}", t.elapsed());

    let t = Instant::now();
    let clip = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::ClipVitB32).with_cache_dir(args.data.join("models")),
    )?;
    println!(
        "embedding model ready (clip-text; ese needs no load): {:?}",
        t.elapsed()
    );

    let app = Arc::new(App {
        lib,
        images,
        clip,
        data: args.data.clone(),
    });

    // real collection names ride into the sidecar's tool schema +
    // instructions (Sidecar::spawn has no data-dir access of its own)
    let _ = chat::SIDECAR_COLLECTIONS.set(
        read_collections(&args.data)
            .into_keys()
            .collect::<Vec<_>>()
            .join(","),
    );

    // --- WebTransport endpoint ---------------------------------------------
    let (endpoint, cert_hash) = wt::build_endpoint(args.wt_port)?;
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
        // slim library gestalt for the chat sidecar's library_overview tool:
        // collection names, sizes, example titles — sized for a 4k-context
        // model to orient with, unlike /api/collections' full id dump
        .route("/api/overview", get({
            let data = args.data.clone();
            move || async move { Json(library_core::tools::overview_tool(&data)) }
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
        // frequency-ranked word completions for the search box's type-ahead
        // dropdown: one prefix scan over the term dictionary, no embedding
        // and no image path. A plain route (not a WebTransport datagram mode)
        // keeps type-ahead off the seq/instant/full state machine.
        .route("/api/complete", get({
            let app = app.clone();
            move |axum::extract::Query(p): axum::extract::Query<CompleteParams>| {
                let app = app.clone();
                async move {
                    let out = tokio::task::spawn_blocking(move || {
                        let q = p.q.trim();
                        if q.is_empty() {
                            return Vec::<String>::new();
                        }
                        let k = p.k.unwrap_or(8);
                        app.lib.rtx(|(_, (_, terms))| terms.complete_ranked(q, k))
                    })
                    .await
                    .expect("complete task panicked");
                    Json(out)
                }
            }
        }))
        // hidden perf view (Cmd+.): the search ring (per-stage timings +
        // per-hit ranker provenance) plus the constants/corpus-counts header
        .route("/api/perf/searches", get({
            let app = app.clone();
            move || {
                let app = app.clone();
                async move {
                    let out = tokio::task::spawn_blocking(move || {
                        let chunks = app.lib.rtx(|((_, vec), _)| vec.len());
                        let figures = app.images.rtx(|(vec, _)| vec.len());
                        let docs = std::fs::read_dir(app.data.join("pages"))
                            .map(|d| d.filter_map(|e| e.ok()).count())
                            .unwrap_or(0);
                        serde_json::json!({
                            "meta": library_core::perf::meta(chunks, figures, docs),
                            "searches": library_core::perf::search_log(),
                        })
                    })
                    .await
                    .expect("perf searches task panicked");
                    Json(out)
                }
            }
        }))
        // per-doc ingest metrics; lazily backfills legibility for docs from
        // before metrics existed (first call on a big library takes seconds)
        .route("/api/perf/ingest", get({
            let data = args.data.clone();
            move || {
                let data = data.clone();
                async move {
                    let out = tokio::task::spawn_blocking(move || {
                        library_core::perf::ingest_rows(&data)
                    })
                    .await
                    .expect("perf ingest task panicked");
                    Json(out)
                }
            }
        }))
        // chat agent: relay the librarian sidecar's NDJSON as SSE. The
        // sidecar (apps/librarian) runs the Apple Foundation Models agent
        // loop; its tools call back into /api/search and /api/text here.
        .route("/api/chat", post(chat::chat))
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
    tokio::spawn(async move {
        axum::serve(listener, http)
            .await
            .expect("http server failed")
    });

    // --- accept loop ---------------------------------------------------------
    loop {
        let incoming = endpoint.accept().await;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = wt::serve_session(incoming, app).await {
                eprintln!("session ended: {e:#}");
            }
        });
    }
}
