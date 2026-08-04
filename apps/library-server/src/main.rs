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
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::Result;
use axum::http::StatusCode;
use axum::{
    Json, Router,
    extract::Path as UrlPath,
    routing::{get, post, put},
};
use clap::Parser;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::meta::Ctx;
use library_core::wire::count_pages;
use library_core::{ClipEmb, Images, Library, Query};
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

mod chat;
mod mem;
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
struct AtlasParams {
    /// Force a rebuild even when the sidecar reads as fresh — the escape
    /// hatch for the fingerprint's blind spot (content changed, counts
    /// identical). Lenient: `1` and `true` both count (curl-friendly).
    refresh: Option<String>,
}

/// Text-chunk embeddings come from ese's static model — free functions,
/// no loaded object (unlike CLIP).
fn embed(s: &str) -> library_core::Emb {
    ese::encode_single(s)
}

/// Uniform error mapping for the marginalia write routes: core returns
/// io errors with caller-actionable messages, clients get 400 + text.
fn bad<T: std::fmt::Display>(e: T) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
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
    /// Behind rwlocks so the marginalia write routes can commit while
    /// searches keep running — the same shape as the desktop Engine.
    lib: RwLock<Library>,
    images: RwLock<Images>,
    /// CLIP text encoder: embeds queries into the shared text/image space
    /// for figure search. Text-chunk queries use ese (no model object).
    clip: TextEmbedding,
    /// The cache dir plus the metadata db. Reads here run concurrently with
    /// the desktop app's writes — that is what WAL buys us.
    ctx: Ctx,
    /// doc -> unix seconds when its read recency was last written. Throttles
    /// the page-view stamps down to something a sweep can still order by.
    touched: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl App {
    fn answer(&self, q: &Query) -> library_core::wire::Response {
        let lib = self.lib.read().expect("library lock poisoned");
        let images = self.images.read().expect("images lock poisoned");
        library_core::answer(&lib, &images, &self.ctx, q, |s| {
            self.clip
                .embed(vec![s.to_string()], None)
                .ok()
                .and_then(|mut v| v.pop())
                .and_then(|v| v.try_into().ok())
        })
    }

    /// Note that a document was read, at most once a minute per document —
    /// a scroll asks for hundreds of pages and this only needs to be
    /// accurate enough to sort an eviction sweep by.
    fn touch_read(&self, doc: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let mut seen = self.touched.lock().expect("touch map poisoned");
            match seen.get(doc) {
                Some(at) if now.saturating_sub(*at) < 60 => return,
                _ => seen.insert(doc.to_string(), now),
            };
        }
        // best-effort: an LRU stamp is never worth failing a page view over
        let _ = self.ctx.touch_read(doc, now);
    }
}

/// `/pages/<doc>/page-NNNN.jpg` -> `<doc>`. Covers count too: opening a
/// shelf is not reading a book, but it is the same document being looked
/// at, and a cover request is one per doc rather than one per page.
fn page_doc(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/pages/")?;
    let (doc, file) = rest.split_once('/')?;
    (!doc.is_empty() && file.ends_with(".jpg")).then(|| doc.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let ctx = Ctx::open(&args.data)?;

    let t = Instant::now();
    let mut lib = library_core::open(args.data.join("library.db"));
    let n = lib.rtx(|((_, vec), _)| vec.len());
    println!(
        "store open: {n} chunks, {:?} (incl. hnsw rebuild)",
        t.elapsed()
    );

    // one-time: noted annotations become notebox cards; a failure must
    // not brick startup (the marker is only written on success, so the
    // next launch retries)
    match library_core::annots::migrate_annots_to_cards(&mut lib, &ctx, &embed) {
        Ok(0) => {}
        Ok(n) => println!("migrated {n} margin notes into cards"),
        Err(e) => eprintln!("annotation migration skipped: {e}"),
    }

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
        lib: RwLock::new(lib),
        images: RwLock::new(images),
        clip,
        ctx: ctx.clone(),
        touched: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    // real collection names ride into the sidecar's tool schema +
    // instructions (Sidecar::spawn has no data-dir access of its own)
    let _ = chat::SIDECAR_COLLECTIONS.set(ctx.shelves().into_keys().collect::<Vec<_>>().join(","));

    // --- WebTransport endpoint ---------------------------------------------
    let (endpoint, cert_hash) = wt::build_endpoint(args.wt_port)?;
    println!("webtransport: https://127.0.0.1:{}", args.wt_port);

    // --- HTTP: web app, page images, cert hash ------------------------------
    let http = Router::new()
        .route(
            "/api/cert_hash",
            get({
                let h = cert_hash.clone();
                move || async move { Json(h) }
            }),
        )
        .route(
            "/api/collections",
            get({
                let ctx = ctx.clone();
                move || async move { Json(ctx.shelves()) }
            }),
        )
        // slim library gestalt for the chat sidecar's library_overview tool:
        // collection names, sizes, example titles — sized for a 4k-context
        // model to orient with, unlike /api/collections' full id dump
        .route(
            "/api/overview",
            get({
                let ctx = ctx.clone();
                move || async move { Json(library_core::tools::overview_tool(&ctx)) }
            }),
        )
        // plain-JSON search for programmatic callers (the chat sidecar's
        // search_library tool, the eval harness). Delegates to the shared
        // agent tools in library_core::tools: complete=false, absolute
        // confidence, top-hit page text — this feeds a 4k-context model.
        .route(
            "/api/search",
            get({
                let app = app.clone();
                move |axum::extract::Query(p): axum::extract::Query<SearchParams>| {
                    let app = app.clone();
                    async move {
                        let out = tokio::task::spawn_blocking(move || {
                            let k = p.k.unwrap_or(library_core::tools::TOOL_K);
                            if p.kind == "images" {
                                let member =
                                    match library_core::tools::resolve_collection(&app.ctx, &p.col)
                                    {
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
                                        let images =
                                            app.images.read().expect("images lock poisoned");
                                        images.rtx(|r| {
                                            library_core::image_search(
                                                &r,
                                                &e,
                                                library_core::IMG_FETCH,
                                                member.as_ref(),
                                            )
                                        })
                                    })
                                    .unwrap_or_default();
                                library_core::tools::image_hits_for_tool(&found, &app.ctx, k)
                            } else {
                                let lib = app.lib.read().expect("library lock poisoned");
                                lib.rtx(|r| {
                                    library_core::tools::search_tool(
                                        &r, &lib, &app.ctx, &p.q, &p.col, k,
                                    )
                                })
                            }
                        })
                        .await
                        .expect("search task panicked");
                        Json(out)
                    }
                }
            }),
        )
        // frequency-ranked word completions for the search box's type-ahead
        // dropdown: one prefix scan over the term dictionary, no embedding
        // and no image path. A plain route (not a WebTransport datagram mode)
        // keeps type-ahead off the seq/instant/full state machine.
        .route(
            "/api/complete",
            get({
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
                            let lib = app.lib.read().expect("library lock poisoned");
                            lib.rtx(|(_, (_, terms))| terms.complete_ranked(q, k))
                        })
                        .await
                        .expect("complete task panicked");
                        Json(out)
                    }
                }
            }),
        )
        // hidden perf view (Cmd+.): the search ring (per-stage timings +
        // per-hit ranker provenance) plus the constants/corpus-counts header
        .route(
            "/api/perf/searches",
            get({
                let app = app.clone();
                move || {
                    let app = app.clone();
                    async move {
                        let out = tokio::task::spawn_blocking(move || {
                            let chunks = app
                                .lib
                                .read()
                                .expect("library lock poisoned")
                                .rtx(|((_, vec), _)| vec.len());
                            let figures = app
                                .images
                                .read()
                                .expect("images lock poisoned")
                                .rtx(|(vec, _)| vec.len());
                            // ocr/, not pages/: renders are an evictable cache
                            // and would undercount a library that has evicted any
                            let docs = std::fs::read_dir(app.ctx.data.join("ocr"))
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
            }),
        )
        // memory provenance: where RAM goes (indexes, caches, models,
        // stores) against process RSS, with an explicit unaccounted gap
        .route(
            "/api/perf/memory",
            get({
                let app = app.clone();
                move || {
                    let app = app.clone();
                    async move {
                        let out = tokio::task::spawn_blocking(move || {
                            let host = crate::mem::host_mem();
                            let lib = app.lib.read().expect("library lock poisoned");
                            let images = app.images.read().expect("images lock poisoned");
                            library_core::perf::memory(&lib, &images, &app.ctx.data, host)
                        })
                        .await
                        .expect("perf memory task panicked");
                        Json(out)
                    }
                }
            }),
        )
        // per-doc ingest metrics; lazily backfills legibility for docs from
        // before metrics existed (first call on a big library takes seconds)
        .route(
            "/api/perf/ingest",
            get({
                let ctx = ctx.clone();
                move || {
                    let ctx = ctx.clone();
                    async move {
                        let out = tokio::task::spawn_blocking(move || {
                            library_core::perf::ingest_rows(&ctx)
                        })
                        .await
                        .expect("perf ingest task panicked");
                        Json(out)
                    }
                }
            }),
        )
        // hidden corpus-atlas view: the cached themes/throughlines sidecar.
        // Stale or missing kicks ONE background build (the claim is the
        // process-wide guard) and reports "building"; the client polls.
        .route(
            "/api/atlas",
            get({
                let app = app.clone();
                move |axum::extract::Query(p): axum::extract::Query<AtlasParams>| {
                    let app = app.clone();
                    async move {
                        let refresh = matches!(p.refresh.as_deref(), Some("1") | Some("true"));
                        let out = tokio::task::spawn_blocking(move || {
                        let fp = {
                            let lib = app.lib.read().expect("library lock poisoned");
                            library_core::atlas::fingerprint(&lib, &app.ctx.data)
                        };
                        if !refresh
                            && let Some(a) = library_core::atlas::load_fresh(&app.ctx.data, &fp)
                        {
                            // a manual rebuild may be running behind a still-
                            // fresh sidecar; the flag keeps the client polling
                            return serde_json::json!({
                                "status": "ready",
                                "rebuilding": library_core::atlas::building().is_some(),
                                "atlas": a,
                            });
                        }
                        if let Some(claim) = library_core::atlas::try_claim() {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                // same resolution as the chat sidecar; core
                                // skips labeling if the binary is absent
                                let librarian = PathBuf::from(
                                    std::env::var("LIBRARIAN_BIN").unwrap_or_else(|_| {
                                        "apps/librarian/.build/release/librarian".into()
                                    }),
                                );
                                let lib = app.lib.read().expect("library lock poisoned");
                                if let Err(e) = library_core::atlas::build(
                                    claim,
                                    &lib,
                                    &app.ctx,
                                    Some(&librarian),
                                ) {
                                    eprintln!("atlas build failed: {e:#}");
                                }
                            });
                        }
                        let (since, stage) =
                            library_core::atlas::building().unwrap_or((0, "starting"));
                        serde_json::json!({"status": "building", "since": since, "stage": stage})
                    })
                    .await
                    .expect("atlas task panicked");
                        Json(out)
                    }
                }
            }),
        )
        // chat agent: relay the librarian sidecar's NDJSON as SSE. The
        // sidecar (apps/librarian) runs the Apple Foundation Models agent
        // loop; its tools call back into /api/search and /api/text here.
        .route("/api/chat", post(chat::chat))
        // reading-order text, sliced by page — what an agent reads after
        // search points it at a page. Capped small: the reader is a model
        // with a 4k-token context, and errors go back as content it can act
        // on, never a bare status code.
        .route(
            "/api/text/{doc}",
            get({
                let ctx = ctx.clone();
                move |UrlPath(doc): UrlPath<String>,
                      axum::extract::Query(p): axum::extract::Query<TextParams>| {
                    let ctx = ctx.clone();
                    async move {
                        Json(library_core::tools::read_pages_tool(
                            &ctx, &doc, p.from, p.to,
                        ))
                    }
                }
            }),
        )
        // a random readable page — the browse affordance behind the sidecar's
        // sample_page tool ("tell me something interesting"). `seed` is a
        // test hook for the eval harness.
        .route(
            "/api/sample",
            get({
                let ctx = ctx.clone();
                move |axum::extract::Query(p): axum::extract::Query<SampleParams>| {
                    let ctx = ctx.clone();
                    async move {
                        let avoid: Vec<String> = p
                            .avoid
                            .split(',')
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect();
                        Json(library_core::tools::sample_page_tool(
                            &ctx, &p.col, p.seed, &avoid,
                        ))
                    }
                }
            }),
        )
        // metadata for the reader drawer in the plain-web build (read-only;
        // the desktop build gets the same facts from the `docs` command)
        .route(
            "/api/doc/{doc}",
            get({
                let ctx = ctx.clone();
                move |UrlPath(doc): UrlPath<String>| {
                    let ctx = ctx.clone();
                    async move {
                        let collections: Vec<String> = ctx
                            .shelves()
                            .into_iter()
                            .filter(|(_, docs)| docs.iter().any(|d| d == &doc))
                            .map(|(name, _)| name)
                            .collect();
                        Json(serde_json::json!({
                            "id": doc,
                            "title": ctx.titles().get(&doc),
                            "pages": count_pages(&ctx.data, &doc),
                            "collections": collections,
                            "status": ctx.doc_status_json(&doc),
                        }))
                    }
                }
            }),
        )
        // the reader has no other way to learn a doc's page count in the
        // plain-web build (the desktop build gets it for free from `docs`)
        .route(
            "/api/pages/{doc}",
            get({
                let data = args.data.clone();
                move |UrlPath(doc): UrlPath<String>| {
                    let data = data.clone();
                    async move { Json(serde_json::json!({ "pages": count_pages(&data, &doc) })) }
                }
            }),
        )
        // --- marginalia: note-box cards (write-capable; the desktop
        // build reaches the same core logic via Tauri commands) -----------
        .route(
            "/api/cards",
            get({
                let app = app.clone();
                move || {
                    let app = app.clone();
                    async move {
                        let out = tokio::task::spawn_blocking(move || {
                            library_core::notes::load_cards(&app.ctx)
                        })
                        .await
                        .expect("cards task panicked");
                        Json(out)
                    }
                }
            }),
        )
        .route(
            "/api/cards",
            post({
                let app = app.clone();
                move |Json(input): Json<library_core::notes::NewCard>| {
                    let app = app.clone();
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let mut lib = app.lib.write().expect("library lock poisoned");
                            library_core::notes::create_card(&mut lib, &app.ctx, input, &embed)
                                .map(Json)
                                .map_err(bad)
                        })
                        .await
                        .expect("create card task panicked")
                    }
                }
            }),
        )
        .route(
            "/api/cards",
            put({
                let app = app.clone();
                move |Json(card): Json<library_core::notes::CardRec>| {
                    let app = app.clone();
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let mut lib = app.lib.write().expect("library lock poisoned");
                            library_core::notes::update_card(&mut lib, &app.ctx, card, &embed)
                                .map(Json)
                                .map_err(bad)
                        })
                        .await
                        .expect("update card task panicked")
                    }
                }
            }),
        )
        .nest_service("/pages", ServeDir::new(args.data.join("pages")))
        .nest_service("/ocr", ServeDir::new(args.data.join("ocr")))
        .fallback_service(ServeDir::new(&args.web))
        // Feed page views into the same read-recency the desktop app's
        // eviction sweep orders by. meta.db is multi-process — that is what
        // it is for — and without this, a book read only through the web UI
        // looks untouched to the app, which would then evict it first.
        //
        // Observation only: the request goes to ServeDir exactly as before,
        // so ETags, ranges and content types are untouched. This server
        // cannot re-render an evicted page (it has no renderer, by design),
        // so the least it can do is not cause one.
        .layer(axum::middleware::from_fn({
            let app = app.clone();
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let app = app.clone();
                async move {
                    if let Some(doc) = page_doc(req.uri().path()) {
                        app.touch_read(&doc);
                    }
                    next.run(req).await
                }
            }
        }))
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

#[cfg(test)]
mod tests {
    use super::*;

    // The middleware runs on every request, so what it does *not* match
    // matters as much as what it does: an API call must never be read as
    // somebody opening a book.
    #[test]
    fn page_doc_matches_page_requests_and_nothing_else() {
        assert_eq!(
            page_doc("/pages/kant/page-0004.jpg").as_deref(),
            Some("kant")
        );
        assert_eq!(page_doc("/pages/kant/cover.jpg").as_deref(), Some("kant"));

        for path in [
            "/api/search?q=x",
            "/pages/",
            "/pages/kant",
            "/pages//page-0001.jpg",
            "/ocr/kant/page-0001.json",
            "/pages/kant/page-0001.json",
        ] {
            assert_eq!(page_doc(path), None, "{path} must not count as a read");
        }
    }
}
