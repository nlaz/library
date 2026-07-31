//! Getting the models onto the machine, and saying how far along that is.
//!
//! Two kinds of model, fetched two different ways.
//!
//! **fastembed's CLIP encoders** arrive when a model object is constructed.
//! fastembed exposes no progress callback — the only hook is
//! `with_show_download_progress`, an indicatif bar on stdout, which is
//! `/dev/null` inside a `.app` bundle and invisible to the user either way.
//! So progress is observed from the outside: the bytes land under
//! `data/models`, and [`watch_download`] polls that directory while the load
//! call blocks. Those totals are approximate by design — they exist to give
//! a bar a denominator, and the reporting clamps to them rather than
//! trusting them.
//!
//! **The page-layout detector** we fetch ourselves ([`ensure_layout`]),
//! because nothing else will: it is not vendored (AGPL-3.0 — see NOTICE) and
//! no dependency knows about it. Fetching it by hand means exact progress
//! and a real integrity check, so that path reports true bytes.
//!
//! All of it happens once per machine, and the app does all of it at launch
//! ([`ensure_layout`], the text encoder, then [`ensure_clip_vision`]) rather
//! than leaving a download to surface partway through someone's first
//! ingest. Everything here is written so that a failure degrades rather than
//! breaks: a missing layout model costs figure recall (see the union in
//! `lib.rs::page_figures`), and an unfetched image encoder just means ingest
//! fetches it itself later. Neither stops anything.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Approximate on-disk size of the fastembed CLIP ViT-B/32 **text** encoder
/// cache (`models--Qdrant--clip-ViT-B-32-text`).
pub const CLIP_TEXT_BYTES: u64 = 244 * 1024 * 1024;
/// ...and of the **vision** encoder (`models--Qdrant--clip-ViT-B-32-vision`).
pub const CLIP_VISION_BYTES: u64 = 335 * 1024 * 1024;

/// Where the page-layout detector comes from. Not vendored: the model is
/// AGPL-3.0, so it is fetched at runtime onto the user's own machine rather
/// than redistributed inside a build artifact. See NOTICE.
pub const LAYOUT_URL: &str = "https://huggingface.co/Oblix/yolov10m-doclaynet_ONNX_document-layout-analysis/resolve/main/onnx/model.onnx";
/// Exact size of that file — not an estimate, and checked after download.
pub const LAYOUT_BYTES: u64 = 61_542_666;
/// ...and its SHA-256. A model file is code we hand to ONNX Runtime, and it
/// arrives over the network from a host we don't control; it gets verified
/// before it is ever loaded.
const LAYOUT_SHA256: &str = "6c5d0f1e1a1b9059bc8351217dce35dd7b0cda0df2f7ec6f7a6b120eb4b3ca96";

/// A load that finishes inside this window was served from cache; reporting
/// a "download" for it would flash a progress line on every single launch.
const GRACE: Duration = Duration::from_millis(600);
const TICK: Duration = Duration::from_millis(250);

/// Total bytes in a directory tree. Missing or unreadable entries count as
/// zero — this only ever feeds a progress bar, never a decision.
pub fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Run `load` while reporting how much `models` has grown, as (done, total)
/// byte counts. `report` fires about four times a second, and never at all
/// when the model was already cached (see [`GRACE`]) — so a warm launch
/// stays silent and only a real download speaks.
///
/// Growth is measured against a baseline taken before the load starts, so a
/// cache that already holds three other models still reports *this*
/// download from zero.
///
/// `load` is what moves to a helper thread, not `report`: every caller's
/// reporter is a `&mut FnMut` on an ingest progress channel or a Tauri
/// handle, and those are neither `Send` nor re-entrant.
pub fn watch_download<T: Send>(
    models: &Path,
    total: u64,
    load: impl FnOnce() -> T + Send,
    mut report: impl FnMut(u64, u64),
) -> T {
    let baseline = dir_bytes(models);
    let finished = AtomicBool::new(false);
    std::thread::scope(|s| {
        let loader = s.spawn(|| {
            let out = load();
            finished.store(true, Ordering::Relaxed);
            out
        });
        let start = Instant::now();
        while !finished.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            if finished.load(Ordering::Relaxed) || start.elapsed() < GRACE {
                continue;
            }
            report(dir_bytes(models).saturating_sub(baseline).min(total), total);
        }
        loader.join().expect("model load thread panicked")
    })
}

/// Make sure the page-layout detector is on disk, downloading it once if
/// not. `report` receives (done, total) bytes as they arrive.
///
/// Idempotent and cheap when already present: a size check, no hashing and
/// no network. Errors are for the caller to log — every caller continues
/// without the model, which costs figure recall and nothing else.
pub fn ensure_layout(data: &Path, report: impl FnMut(u64, u64)) -> Result<()> {
    let dest = crate::layout::LayoutModel::model_path(data);
    if std::fs::metadata(&dest).is_ok_and(|m| m.len() == LAYOUT_BYTES) {
        return Ok(());
    }
    let dir = dest.parent().context("layout path has no parent")?;
    std::fs::create_dir_all(dir)?;
    // Download beside the destination, not into it: a half-written file at
    // the real path would be loaded on the next run and fail inside ONNX
    // Runtime instead of here, where it can simply be retried.
    let part = dest.with_extension("onnx.part");
    let digest = fetch_to(LAYOUT_URL, &part, LAYOUT_BYTES, report)?;
    if digest != LAYOUT_SHA256 {
        let _ = std::fs::remove_file(&part);
        bail!("layout model failed its checksum (got {digest}) — not installing it");
    }
    std::fs::rename(&part, &dest)?;
    Ok(())
}

/// Pull the CLIP **image** encoder into the cache and drop it again, so the
/// first document a user adds isn't stalled mid-ingest by a 335 MB download.
///
/// Constructing the model is the only way to make fastembed fetch its
/// weights, and the object is released immediately: it is ~350 MB resident,
/// and [`crate::prepare_figures`] deliberately loads it per document and
/// drops it after. What outlives this call is the cache on disk, which is
/// the whole point.
pub fn ensure_clip_vision(data: &Path, report: impl FnMut(u64, u64)) -> Result<()> {
    use fastembed::{ImageEmbedding, ImageEmbeddingModel, ImageInitOptions};

    let models = data.join("models");
    watch_download(
        &models,
        CLIP_VISION_BYTES,
        || {
            ImageEmbedding::try_new(
                ImageInitOptions::new(ImageEmbeddingModel::ClipVitB32)
                    .with_cache_dir(models.clone()),
            )
        },
        report,
    )
    .context("fetching the CLIP image encoder")?;
    Ok(())
}

/// Stream `url` into `dest`, reporting bytes as they land, and return the
/// SHA-256 of what was written. `expect` is the size the caller demands: a
/// short read is an error here rather than a corrupt model later.
fn fetch_to(
    url: &str,
    dest: &Path,
    expect: u64,
    mut report: impl FnMut(u64, u64),
) -> Result<String> {
    use sha2::{Digest, Sha256};

    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    // HuggingFace redirects to a CDN that reports the real length; fall back
    // to the caller's figure so the bar always has a denominator
    let total: u64 = resp
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(expect);

    let mut out =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut src = resp.into_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = src.read(&mut buf).context("reading the model stream")?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut out, &buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        report(done.min(total), total);
    }
    drop(out);

    if done != expect {
        let _ = std::fs::remove_file(dest);
        bail!("short download: got {done} bytes, expected {expect}");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("models-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Network + 59 MB; run explicitly:
    ///   cargo test -p library-ingest --release ensure_layout_really -- --ignored --nocapture
    #[test]
    #[ignore = "hits the network and downloads 59 MB"]
    fn ensure_layout_really_downloads_and_verifies() {
        let d = tmp("fetch");
        let mut last = (0u64, 0u64);
        let mut ticks = 0usize;
        super::ensure_layout(&d, |done, total| {
            last = (done, total);
            ticks += 1;
        })
        .expect("download");
        let path = crate::layout::LayoutModel::model_path(&d);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), LAYOUT_BYTES);
        assert!(
            ticks > 10,
            "progress must be reported as it streams: {ticks}"
        );
        assert_eq!(last, (LAYOUT_BYTES, LAYOUT_BYTES));
        // and it must load in ONNX Runtime, which is the whole point
        assert!(crate::layout::LayoutModel::load(&d).unwrap().is_some());
        // second call is a no-op: no network, no progress
        let mut again = 0usize;
        super::ensure_layout(&d, |_, _| again += 1).expect("cached");
        assert_eq!(again, 0, "an installed model must not re-download");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn dir_bytes_sums_nested_files() {
        let d = tmp("sum");
        std::fs::write(d.join("a"), [0u8; 10]).unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/b"), [0u8; 32]).unwrap();
        assert_eq!(dir_bytes(&d), 42);
    }

    #[test]
    fn dir_bytes_of_missing_dir_is_zero() {
        assert_eq!(dir_bytes(Path::new("/nonexistent/for/this/test")), 0);
    }

    #[test]
    fn cached_load_reports_nothing() {
        let d = tmp("cached");
        let mut hits = 0usize;
        let out = watch_download(&d, 100, || 7, |_, _| hits += 1);
        assert_eq!(out, 7);
        assert_eq!(hits, 0, "a fast load is a cache hit");
    }

    #[test]
    fn slow_load_reports_growth_against_the_baseline() {
        let d = tmp("growth");
        // a model already in this cache dir must not count toward ours
        std::fs::write(d.join("other"), vec![0u8; 500]).unwrap();
        let mut seen = Vec::new();
        watch_download(
            &d,
            1000,
            || {
                std::thread::sleep(GRACE + TICK);
                std::fs::write(d.join("ours"), vec![0u8; 250]).unwrap();
                std::thread::sleep(TICK * 3);
            },
            |done, total| seen.push((done, total)),
        );
        assert!(!seen.is_empty(), "a slow load must report");
        assert!(
            seen.iter()
                .all(|&(done, total)| done <= 250 && total == 1000),
            "growth is measured from the baseline, not the whole dir: {seen:?}"
        );
    }
}
