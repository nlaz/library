//! Static file serving for the `pages://` and `ocr://` custom protocols.
//!
//! `pages://` is no longer plain static serving. Page images are a bounded
//! cache, so a request can miss a page whose document is perfectly intact,
//! and the miss is repaired here — rendered from the user's file, written
//! where the reader looks, and served. See [`render`](crate::render) for
//! what stops that from becoming twenty concurrent rasterizations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use library_core::meta::Ctx;
use percent_encoding::percent_decode_str;

use crate::render::{Admit, RenderGate};

/// Longest edge of a derived shelf cover — 2× the widest card the home grid
/// lays out, so covers stay retina-sharp without decoding the full scan.
const COVER_PX: u32 = 512;

/// How often a document's `last_read_at` is actually written. A search can
/// ask for two dozen images at once and a scroll asks for hundreds; once a
/// minute is ample to order an eviction sweep by, and the alternative is a
/// database write per page view.
const TOUCH_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Page images are derived and can be rewritten under a live webview, so
/// they get a short window rather than the immutable year static assets get.
const PAGE_CACHE_CONTROL: &str = "private, max-age=600";

/// Everything the `pages://` handler needs. Cloned into the protocol
/// closure, which is why the gate is shared rather than owned.
#[derive(Clone)]
pub(crate) struct PageServer {
    /// `<data>/pages`.
    pub root: PathBuf,
    pub ctx: Ctx,
    /// Rendered page-image width, from settings — a re-render must match
    /// what the rest of the document was rendered at.
    pub width: u32,
    pub gate: Arc<RenderGate>,
    /// doc -> when we last wrote its `last_read_at`.
    touched: Arc<Mutex<HashMap<String, Instant>>>,
}

impl PageServer {
    pub(crate) fn new(data: &Path, ctx: Ctx, width: u32) -> PageServer {
        PageServer {
            root: data.join("pages"),
            ctx,
            width,
            gate: Arc::new(RenderGate::new()),
            touched: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Note that a document was read, at most once per [`TOUCH_EVERY`].
    fn touch(&self, doc: &str) {
        {
            let mut seen = self.touched.lock().expect("touch map poisoned");
            match seen.get(doc) {
                Some(at) if at.elapsed() < TOUCH_EVERY => return,
                _ => seen.insert(doc.to_string(), Instant::now()),
            };
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // best-effort: an LRU stamp is not worth failing a page view over
        let _ = self.ctx.touch_read(doc, now);
    }
}

/// Percent-decode a request path and reject anything that isn't a plain
/// relative path (no `..`, no roots).
fn decode_rel(request: &tauri::http::Request<Vec<u8>>) -> Option<PathBuf> {
    let path = percent_decode_str(request.uri().path())
        .decode_utf8()
        .ok()?;
    let rel = Path::new(path.trim_start_matches('/'));
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(rel.to_path_buf())
}

/// Serve files under `root` for a custom URI scheme. The CORS header matters:
/// the webview page's origin (dev server or tauri://) is cross-origin to
/// these schemes, so fetch() needs it even though everything is local.
#[expect(clippy::unwrap_used)] // invariant: static status + literal header names cannot fail to build
pub(crate) fn serve_static(
    root: PathBuf,
    content_type: &'static str,
    cache_control: &'static str,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let not_found = || {
        tauri::http::Response::builder()
            .status(404)
            .header("access-control-allow-origin", "*")
            .body(Vec::new())
            .unwrap() // invariant: static status + valid header names cannot fail to build
    };
    let Some(rel) = decode_rel(&request) else {
        return not_found();
    };
    match std::fs::read(root.join(rel)) {
        Ok(bytes) => tauri::http::Response::builder()
            .status(200)
            .header("content-type", content_type)
            .header("cache-control", cache_control)
            .header("access-control-allow-origin", "*")
            .body(bytes)
            .unwrap(), // invariant: static status + valid header names cannot fail to build
        Err(_) => not_found(),
    }
}

/// An empty response with a status and a machine-readable reason, so the
/// reader's placeholder can say something true instead of "didn't load".
#[expect(clippy::unwrap_used)] // invariant: static status + literal header names cannot fail to build
fn refuse(
    status: u16,
    reason: &'static str,
    retry_after: Option<u32>,
) -> tauri::http::Response<Vec<u8>> {
    let mut b = tauri::http::Response::builder()
        .status(status)
        .header("access-control-allow-origin", "*")
        .header("x-library-reason", reason);
    if let Some(secs) = retry_after {
        b = b.header("retry-after", secs.to_string());
    }
    b.body(Vec::new()).unwrap()
}

/// `<doc>/page-NNNN.jpg` -> (doc, N).
fn page_request(rel: &Path) -> Option<(String, u32)> {
    let doc = rel.parent()?.to_str()?;
    let name = rel.file_name()?.to_str()?;
    let n = name.strip_prefix("page-")?.strip_suffix(".jpg")?;
    (!doc.is_empty()).then_some((doc.to_string(), n.parse().ok()?))
}

/// `pages://`, with two rules beyond static serving.
///
/// `<doc>/cover.jpg` is derived on first request — a small thumbnail of page
/// 1 persisted next to the page images — so shelves never decode
/// multi-megabyte scans into 150px cards.
///
/// And a missing `page-NNNN.jpg` is not a 404 any more. Page images are an
/// evictable cache, so the ordinary reason one is absent is that a sweep
/// took it; it gets rendered from the source file and written back. What a
/// 404 means now is narrower and worth saying out loud: there is no file to
/// render *from*.
pub(crate) fn serve_pages(
    server: &PageServer,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let Some(rel) = decode_rel(&request) else {
        return refuse(404, "bad-path", None);
    };
    let root = &server.root;

    // A cover is derived from page 1, so an evicted page 1 has to come back
    // first — otherwise the shelf loses its cover the moment a book is swept.
    let is_cover = rel.file_name().is_some_and(|f| f == "cover.jpg");
    if is_cover && !root.join(&rel).exists() {
        if let Some(dir) = rel.parent().and_then(|d| d.to_str())
            && !root.join(dir).join("page-0001.jpg").exists()
            && let Some(res) = ensure_page(server, dir, 1)
        {
            return res;
        }
        build_cover(root, &rel);
    }

    if let Some((doc, page)) = page_request(&rel)
        && !root.join(&rel).exists()
        && let Some(res) = ensure_page(server, &doc, page)
    {
        return res;
    }

    if let Some(doc) = rel.parent().and_then(|d| d.to_str()) {
        server.touch(doc);
    }
    // Not `immutable`, and not a year. A page image is derived, and it is
    // rewritten whenever the source file changes (roots::reconcile marks the
    // doc Edited and re-ingests) or the render width does — under the old
    // header every open webview kept the stale bytes for twelve months. Ten
    // minutes still covers a scroll, which is what the caching was for.
    serve_static(root.clone(), "image/jpeg", PAGE_CACHE_CONTROL, request)
}

/// Put `page` of `doc` back on disk, or explain why not.
///
/// `Some` is a response to send instead; `None` means the file is there now
/// (or was all along) and the caller should serve it normally.
fn ensure_page(
    server: &PageServer,
    doc: &str,
    page: u32,
) -> Option<tauri::http::Response<Vec<u8>>> {
    // Reserved ids ('~'-prefixed) are synthetic documents — note cards and
    // the like. They have no file, so there is nothing to render from.
    if library_core::records::is_reserved(doc) {
        return Some(refuse(404, "not-a-document", None));
    }
    // Bounds before anything else: page 5000 of a 200-page book is ordinary
    // URL input and must not reach the renderer, or the source lookup.
    let total = library_core::wire::count_pages(&server.ctx.data, doc);
    if page == 0 || page > total {
        return Some(refuse(404, "no-such-page", None));
    }

    // Admission next, so that under load we shed before doing any work at
    // all — and so everything below runs holding a slot.
    let out = server.root.join(doc).join(format!("page-{page:04}.jpg"));
    let _slot = match server.gate.admit((doc.to_string(), page)) {
        // Someone else was already making this page. Either they finished
        // it, or they failed and we decline to pile on.
        Admit::Followed => {
            return (!out.exists()).then(|| refuse(503, "render-busy", Some(1)));
        }
        Admit::Busy => return Some(refuse(503, "render-busy", Some(1))),
        Admit::Go(slot) => slot,
    };

    // Re-check under the slot: a leader we raced may have finished between
    // our miss and our admission.
    if out.exists() {
        return None;
    }
    // A delete racing this would recreate the directory it just removed,
    // resurrecting a document the user retracted.
    if library_ingest::status::read(&server.ctx, doc).map(|s| s.state)
        == Some(library_ingest::status::DocState::Deleted)
    {
        return Some(refuse(404, "not-a-document", None));
    }

    let src = match library_ingest::source_state(&server.ctx, doc) {
        library_ingest::Source::Ready(p) => p,
        // The file is gone, so this page cannot be remade. The eviction
        // sweep is supposed to make this unreachable by never sweeping a
        // document it cannot re-render — arriving here means the file went
        // missing after the sweep, not that the sweep was wrong.
        library_ingest::Source::NoRow | library_ingest::Source::Missing => {
            return Some(refuse(404, "no-source", None));
        }
        // Present but evicted to the cloud. Faulting it in behind a page
        // request would pull hundreds of megabytes over whatever connection
        // the laptop is on, silently, because someone scrolled.
        library_ingest::Source::Dataless => {
            return Some(refuse(503, "offline", Some(30)));
        }
    };

    match library_ingest::ocr::render_one(&src, page, server.width, &out) {
        Ok(()) => None,
        Err(e) => {
            eprintln!("page render failed ({doc} p.{page}): {e:#}");
            Some(refuse(502, "render-failed", None))
        }
    }
}

/// Best-effort: any failure just means the request 404s like a missing file
/// (e.g. page 1 not rendered yet — the next render retries).
fn build_cover(root: &Path, rel: &Path) {
    let Some(dir) = rel.parent() else { return };
    let Ok(img) = image::open(root.join(dir).join("page-0001.jpg")) else {
        return;
    };
    let thumb = img.thumbnail(COVER_PX, COVER_PX).into_rgb8();
    let mut bytes = Vec::new();
    if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 82)
        .encode_image(&thumb)
        .is_err()
    {
        return;
    }
    // unique tmp + rename: concurrent first requests may both build the
    // cover, but a reader can never observe a half-written file
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = root.join(dir).join(format!(
        ".cover-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, root.join(rel)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pages_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("serve-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("doc")).unwrap();
        root
    }

    /// A data dir with `ocr/doc` holding `pages` sidecars — the page count
    /// the bounds check reads — and an in-memory meta.db.
    fn server_fixture(name: &str, pages: u32) -> (PathBuf, PageServer) {
        let data = std::env::temp_dir().join(format!("serve-srv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let ocr = data.join("ocr").join("doc");
        std::fs::create_dir_all(&ocr).unwrap();
        std::fs::create_dir_all(data.join("pages").join("doc")).unwrap();
        for i in 1..=pages {
            std::fs::write(ocr.join(format!("page-{i:04}.json")), b"{}").unwrap();
        }
        let ctx = Ctx::in_memory(&data).unwrap();
        let server = PageServer::new(&data, ctx, 1600);
        (data, server)
    }

    fn reason(res: &tauri::http::Response<Vec<u8>>) -> &str {
        res.headers()
            .get("x-library-reason")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    // A page number arrives in a URL, so it is ordinary untrusted input.
    // Page 9999 of a 200-page book must be refused before anything opens the
    // source file — the bounds check is what stops a scroll from turning
    // into an unbounded stream of renders of nothing.
    #[test]
    fn a_page_past_the_end_is_refused_without_touching_the_file() {
        let (data, server) = server_fixture("bounds", 3);
        for page in [0u32, 4, 9999] {
            let res = ensure_page(&server, "doc", page).expect("must refuse");
            assert_eq!(res.status(), 404, "page {page}");
            assert_eq!(reason(&res), "no-such-page");
        }
        std::fs::remove_dir_all(&data).unwrap();
    }

    // Reserved ids are synthetic documents with no file behind them, so a
    // render request for one is meaningless — the same guard delete_doc
    // applies before it touches the filesystem.
    #[test]
    fn reserved_ids_are_not_documents() {
        let (data, server) = server_fixture("reserved", 3);
        let res = ensure_page(&server, "~notes", 1).expect("must refuse");
        assert_eq!(res.status(), 404);
        assert_eq!(reason(&res), "not-a-document");
        std::fs::remove_dir_all(&data).unwrap();
    }

    // No file to render from is the one case that is genuinely a 404 now,
    // and it has to be distinguishable from "the cache is busy" so the
    // reader can stop retrying and say something true.
    #[test]
    fn a_doc_with_no_source_says_so_rather_than_failing_vaguely() {
        let (data, server) = server_fixture("nosource", 3);
        let res = ensure_page(&server, "doc", 1).expect("must refuse");
        assert_eq!(res.status(), 404);
        assert_eq!(reason(&res), "no-source");
        std::fs::remove_dir_all(&data).unwrap();
    }

    // Shedding must be immediate rather than queued, and must carry a
    // retry-after — the reader's retry is the only thing that makes a
    // refused request backpressure instead of a lost page.
    #[test]
    fn a_saturated_render_queue_sheds_with_a_retry_hint() {
        use crate::render::{RENDER_QUEUE_MAX, RENDER_WORKERS};
        let (data, server) = server_fixture("shed", 3);

        // Fill the worker slots inline, then park the rest on threads:
        // admit *blocks* for a slot while there is queue room, so saturating
        // it from one thread would deadlock the test rather than shed.
        let mut held = Vec::new();
        for i in 0..RENDER_WORKERS {
            held.push(server.gate.admit(("other".to_string(), i as u32)));
        }
        let gate = Arc::clone(&server.gate);
        let parked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut threads = Vec::new();
        for i in 0..(RENDER_QUEUE_MAX - RENDER_WORKERS) {
            let (g, p) = (Arc::clone(&gate), Arc::clone(&parked));
            threads.push(std::thread::spawn(move || {
                p.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = g.admit(("other".to_string(), 100 + i as u32));
            }));
        }
        while parked.load(std::sync::atomic::Ordering::SeqCst) < RENDER_QUEUE_MAX - RENDER_WORKERS {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));

        let res = ensure_page(&server, "doc", 1).expect("must refuse");
        assert_eq!(res.status(), 503);
        assert_eq!(reason(&res), "render-busy");
        assert_eq!(
            res.headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap()),
            Some("1")
        );
        drop(held);
        for t in threads {
            let _ = t.join();
        }
        std::fs::remove_dir_all(&data).unwrap();
    }

    // A page image is derived and gets rewritten under a live webview — by a
    // re-ingest after the file changes, or a width change. The year-long
    // immutable header meant every open view kept the stale bytes for twelve
    // months.
    #[test]
    fn page_images_are_not_cached_immutably() {
        assert!(!PAGE_CACHE_CONTROL.contains("immutable"));
        assert!(PAGE_CACHE_CONTROL.contains("max-age=600"));
    }

    #[test]
    fn page_requests_are_parsed_and_junk_is_not() {
        assert_eq!(
            page_request(Path::new("doc/page-0042.jpg")),
            Some(("doc".to_string(), 42))
        );
        assert_eq!(page_request(Path::new("doc/cover.jpg")), None);
        assert_eq!(page_request(Path::new("page-0001.jpg")), None);
        assert_eq!(page_request(Path::new("doc/page-abc.jpg")), None);
    }

    #[test]
    fn cover_derived_from_page_one_and_capped() {
        let root = temp_pages_root("derive");
        // a tall "scan" — 800×1200, well over COVER_PX on both edges
        let page = image::RgbImage::from_pixel(800, 1200, image::Rgb([200, 180, 160]));
        page.save(root.join("doc/page-0001.jpg")).unwrap();

        build_cover(&root, Path::new("doc/cover.jpg"));

        let cover = image::open(root.join("doc/cover.jpg")).unwrap();
        assert!(cover.width() <= COVER_PX && cover.height() <= COVER_PX);
        // aspect preserved: still a 2:3 portrait after the fit
        assert_eq!(cover.height(), COVER_PX);
        assert_eq!(cover.width(), COVER_PX * 2 / 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cover_missing_page_is_a_noop() {
        let root = temp_pages_root("missing");
        build_cover(&root, Path::new("doc/cover.jpg"));
        assert!(!root.join("doc/cover.jpg").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
