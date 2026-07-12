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
use axum::{Json, Router, routing::get};
use clap::Parser;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use library_core::wire::{
    Response, WireHit, group_image_hits, read_collections, wire_hit,
};
use library_core::{ClipEmb, Emb, FxHashSet, Images, Library, tokenize};
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use wtransport::{Endpoint, Identity, ServerConfig};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "data")]
    data: PathBuf,
    /// Directory of built web assets to serve at `/`.
    #[arg(long, default_value = "web/dist")]
    web: PathBuf,
    #[arg(long, default_value_t = 4433)]
    wt_port: u16,
    #[arg(long, default_value_t = 8080)]
    http_port: u16,
}

#[derive(Deserialize)]
struct Query {
    seq: u64,
    q: String,
    /// "instant" = lexical only (every keystroke), "full" = hybrid (debounced)
    #[serde(default)]
    mode: String,
    /// restrict to a collection from data/collections.json; empty = everything
    #[serde(default)]
    col: String,
    /// "all" | "text" | "images" (empty = "all")
    #[serde(default)]
    kind: String,
}

struct App {
    lib: Library,
    images: Images,
    model: TextEmbedding,
    clip: TextEmbedding,
    data: PathBuf,
}

const K: usize = 20;
/// Image hits appended to an "all" response (kind=images gets a full K).
const K_IMG_BLEND: usize = 6;

impl App {
    fn answer(&self, q: &Query) -> Response {
        let start = Instant::now();
        let want_text = q.kind != "images";
        let want_imgs = q.kind == "images" || (q.kind != "text" && q.mode == "full");

        // collection filter, pushed down into every ranker
        let member: Option<FxHashSet<String>> = (!q.col.is_empty())
            .then(|| read_collections(&self.data).remove(&q.col))
            .flatten()
            .map(|docs| docs.into_iter().collect());

        let mut phase = "lex";
        let mut hits: Vec<WireHit> = Vec::new();

        if want_text {
            let qemb: Option<Emb> = (q.mode == "full")
                .then(|| {
                    self.model
                        .embed(vec![q.q.clone()], None)
                        .ok()
                        .and_then(|mut v| v.pop())
                        .and_then(|v| v.try_into().ok())
                })
                .flatten();
            if qemb.is_some() {
                phase = "hybrid";
            }
            let qtoks = tokenize(&q.q);
            let found = self
                .lib
                .rtx(|r| library_core::search(&r, &q.q, qemb.as_ref(), K, member.as_ref()));
            hits.extend(found.iter().map(|h| wire_hit(h, &qtoks)));
        }

        if want_imgs {
            let k = if q.kind == "images" { K } else { K_IMG_BLEND };
            let qemb: Option<ClipEmb> = self
                .clip
                .embed(vec![q.q.clone()], None)
                .ok()
                .and_then(|mut v| v.pop())
                .and_then(|v| v.try_into().ok());
            if let Some(e) = qemb {
                phase = if want_text { "hybrid+img" } else { "img" };
                let found = self
                    .images
                    .rtx(|r| library_core::image_search(&r, &e, 40, member.as_ref()));
                hits.extend(group_image_hits(&found, k));
            }
        }

        Response { seq: q.seq, phase, us: start.elapsed().as_micros(), hits }
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
    let model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::BGESmallENV15).with_cache_dir(args.data.join("models")),
    )?;
    let clip = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::ClipVitB32).with_cache_dir(args.data.join("models")),
    )?;
    println!("embedding models ready (bge + clip-text): {:?}", t.elapsed());

    let app = Arc::new(App { lib, images, model, clip, data: args.data.clone() });

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
