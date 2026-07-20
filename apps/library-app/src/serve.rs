//! Static file serving for the `pages://` and `ocr://` custom protocols.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use percent_encoding::percent_decode_str;

/// Longest edge of a derived shelf cover — 2× the widest card the home grid
/// lays out, so covers stay retina-sharp without decoding the full scan.
const COVER_PX: u32 = 512;

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
            .header("cache-control", "public, max-age=31536000, immutable")
            .header("access-control-allow-origin", "*")
            .body(bytes)
            .unwrap(), // invariant: static status + valid header names cannot fail to build
        Err(_) => not_found(),
    }
}

/// `pages://` with one extra rule: `<doc>/cover.jpg` is derived on first
/// request — a small thumbnail of page 1 persisted next to the page images —
/// so shelves never decode multi-megabyte scans into 150px cards.
pub(crate) fn serve_pages(
    root: PathBuf,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    if let Some(rel) = decode_rel(&request)
        && rel.file_name().is_some_and(|f| f == "cover.jpg")
        && !root.join(&rel).exists()
    {
        build_cover(&root, &rel);
    }
    serve_static(root, "image/jpeg", request)
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
