//! Static file serving for the `pages://` and `ocr://` custom protocols.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// Serve files under `root` for a custom URI scheme. The CORS header matters:
/// the webview page's origin (dev server or tauri://) is cross-origin to
/// these schemes, so fetch() needs it even though everything is local.
#[expect(clippy::unwrap_used)] // invariant: static status + literal header names cannot fail to build
pub(crate) fn serve_static(
    root: PathBuf,
    content_type: &'static str,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let not_found = || {
        tauri::http::Response::builder()
            .status(404)
            .header("access-control-allow-origin", "*")
            .body(Vec::new())
            .unwrap() // invariant: static status + valid header names cannot fail to build
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
            .unwrap(), // invariant: static status + valid header names cannot fail to build
        Err(_) => not_found(),
    }
}
