//! Render source files to page JPEGs and extract their words, in-process.
//! PDF pages with an embedded text layer (see `pdftext`) skip OCR entirely;
//! the rest go through Apple Vision. Standalone images ([`ocr_image`]) are
//! one-page docs and always OCR. PDF work fans out across a small worker
//! pool — Vision and the renderer are the ingest's long pole, and pages are
//! independent.
//!
//! The on-disk contract is unchanged so existing caches stay valid:
//!
//!   <pages_dir>/page-NNNN.jpg    rendered page image, `width` px wide
//!   <ocr_dir>/page-NNNN.json     {"page": N, "words": [{"t","x","y","w","h"}]}
//!
//! Boxes are normalized 0..1 with a TOP-LEFT origin (Vision's bottom-left
//! coordinates are flipped here).
//!
//! Re-runs are incremental, and the two outputs are tracked apart: a page
//! with both files is skipped, a page missing only its JPEG is re-rendered
//! without going near Vision, and a page missing only its JSON is
//! recognized (rasterizing again only if there is no text layer to read).
//! See [`work_for_page`] — the split is what makes `data/pages` affordable
//! to treat as a cache.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use library_core::Word;
use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CFURL, CGPoint, CGRect, CGSize,
};
use objc2_core_foundation::{CFType, kCFBooleanTrue};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo, CGPDFBox, CGPDFDocument, CGPDFPage,
};
use objc2_foundation::{NSArray, NSDictionary, NSRange};
use objc2_image_io::{
    CGImageDestination, CGImageSource, kCGImageDestinationLossyCompressionQuality,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceThumbnailMaxPixelSize,
};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

use crate::{PageOcr, Progress, ProgressFn, pdftext};

const JPEG_QUALITY: f64 = 0.8;

/// Pages in flight at once. Vision serializes its own recognition work
/// internally (measured: 6 workers is no faster than 3), so extra workers
/// only exist to hide render/encode/write latency behind it; each holds
/// one page bitmap, so more just costs memory.
const OCR_WORKERS: usize = 3;

/// What a page still needs. The two halves are tracked apart because they
/// cost wildly different amounts and, once `data/pages` is an evictable
/// cache, go missing independently.
///
/// A page whose JSON survives but whose JPEG was swept needs `render` only:
/// rasterize and encode, ~160ms. Treating that as "not done" and sending it
/// back through Vision would cost minutes per book and would silently redo
/// work that can never be improved by redoing it — and it would break the
/// promise in `ingest.rs` that restoring a file must not cost an OCR pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageWork {
    pub render: bool,
    pub words: bool,
}

impl PageWork {
    fn any(&self) -> bool {
        self.render || self.words
    }
}

/// Decide from what is already on disk. Pure, so the matrix is testable
/// without a renderer or a PDF.
pub(crate) fn work_for_page(pages_dir: &Path, ocr_dir: &Path, i: usize) -> PageWork {
    PageWork {
        render: !pages_dir.join(format!("page-{i:04}.jpg")).exists(),
        words: !ocr_dir.join(format!("page-{i:04}.json")).exists(),
    }
}

/// Render + extract words for every page of `pdf` (or the first `limit`).
/// Cached pages are skipped; `Progress::Ocr` counts completed pages, with a
/// final `Progress::Log` summary line. With `text_layer`, pages carrying an
/// embedded text layer skip Vision (the residual pages still OCR).
pub fn ocr_pdf(
    pdf: &Path,
    pages_dir: &Path,
    ocr_dir: &Path,
    width: u32,
    limit: Option<usize>,
    text_layer: bool,
    progress: ProgressFn,
) -> Result<()> {
    std::fs::create_dir_all(pages_dir)?;
    std::fs::create_dir_all(ocr_dir)?;

    let mut n = open_pdf(pdf)?.1;
    if let Some(lim) = limit {
        n = n.min(lim);
    }

    let todo: Vec<(usize, PageWork)> = (1..=n)
        .map(|i| (i, work_for_page(pages_dir, ocr_dir, i)))
        .filter(|(_, w)| w.any())
        .collect();
    let skipped = n - todo.len();

    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Result<PageDone>>();
    let (mut done, mut layered, mut ocred, mut rendered) = (0usize, 0usize, 0usize, 0usize);
    std::thread::scope(|s| -> Result<()> {
        for _ in 0..OCR_WORKERS.min(todo.len()) {
            let tx = tx.clone();
            let (next, todo) = (&next, &todo);
            s.spawn(move || {
                // per-worker documents: CGPDFDocument's thread safety is
                // undocumented, and reopening is nothing next to a page of
                // OCR. The progress callback isn't Send, so results go back
                // over the channel and the caller's thread reports them.
                let Some(doc) =
                    CFURL::from_file_path(pdf).and_then(|url| CGPDFDocument::with_url(Some(&url)))
                else {
                    let _ = tx.send(Err(anyhow::anyhow!("cannot open {}", pdf.display())));
                    return;
                };
                let text = text_layer.then(|| pdftext::TextLayer::open(pdf)).flatten();
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(i, work)) = todo.get(k) else { break };
                    let res = process_page(&doc, text.as_ref(), i, work, width, pages_dir, ocr_dir);
                    if tx.send(res).is_err() {
                        break; // receiver bailed on an earlier error
                    }
                }
            });
        }
        drop(tx);
        // moving `rx` into the loop makes an early `?` drop it, which stops
        // the workers at their next send instead of running out the queue
        for msg in rx {
            let d = msg?;
            layered += usize::from(d.layered);
            ocred += usize::from(d.ocred);
            rendered += usize::from(d.rendered);
            done += 1;
            progress(Progress::Ocr {
                done: (skipped + done) as u32,
                total: n as u32,
            });
        }
        Ok(())
    })?;
    // `rendered` counts JPEGs written, which overlaps the other two: a fresh
    // page is both rendered and ocr'd. It is called out separately because
    // pages that needed *only* a render are the ones an evicted cache
    // produces, and they cost ~160ms where an OCR costs seconds.
    let render_only = rendered.saturating_sub(layered + ocred);
    progress(Progress::Log(format!(
        "ocr complete: {layered} text-layer, {ocred} ocr'd, {render_only} re-rendered, \
         {skipped} cached, {n} total"
    )));
    progress(Progress::OcrSummary {
        text_layer: layered as u32,
        vision: ocred as u32,
        cached: skipped as u32,
    });
    Ok(())
}

/// What one page actually cost, for the summary counters.
#[derive(Debug, Clone, Copy, Default)]
struct PageDone {
    /// Words came from the embedded text layer.
    layered: bool,
    /// Vision ran.
    ocred: bool,
    /// A JPEG was written.
    rendered: bool,
}

/// Do whatever `work` says page `i` still needs: rasterize, encode, and
/// extract words — from the text layer when usable, else Vision.
fn process_page(
    doc: &CGPDFDocument,
    text: Option<&pdftext::TextLayer>,
    i: usize,
    work: PageWork,
    width: u32,
    pages_dir: &Path,
    ocr_dir: &Path,
) -> Result<PageDone> {
    let jpg = pages_dir.join(format!("page-{i:04}.jpg"));
    let js = ocr_dir.join(format!("page-{i:04}.json"));

    // Ask the text layer first: if it can answer, a page needing only words
    // never has to be rasterized at all.
    let layer_words = work.words.then(|| text.and_then(|t| t.words(i))).flatten();
    // Vision needs the bitmap, so words-without-a-layer forces a render even
    // when the JPEG is already on disk.
    let need_img = work.render || (work.words && layer_words.is_none());

    // drain autoreleased CGImages/Vision buffers every page, or a long
    // run accumulates hundreds of page bitmaps and exhausts memory
    let (words, done) = autoreleasepool(|_| -> Result<(Option<Vec<Word>>, PageDone)> {
        let mut done = PageDone::default();
        let img = need_img.then(|| render_page(doc, i, width)).transpose()?;
        if let (true, Some(img)) = (work.render, img.as_ref()) {
            save_jpeg(img, &jpg)?;
            done.rendered = true;
        }
        let words = match (work.words, layer_words) {
            (false, _) => None,
            (true, Some(w)) => {
                done.layered = true;
                Some(w)
            }
            (true, None) => {
                let img = img.as_ref().context("no bitmap to OCR")?;
                done.ocred = true;
                Some(ocr_words(img)?)
            }
        };
        Ok((words, done))
    })?;

    if let Some(words) = words {
        write_page_json(&js, i as u32, words)?;
    }
    Ok(done)
}

/// Write a page's words as OCR JSON, via tmp + rename so a crash can't
/// leave a half-written file.
fn write_page_json(js: &Path, page: u32, words: Vec<Word>) -> Result<()> {
    let tmp = js.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&PageOcr { page, words })?)?;
    std::fs::rename(&tmp, js)?;
    Ok(())
}

/// One image file is a one-page doc: decode via ImageIO (EXIF orientation
/// applied, longest edge capped at `width`, never upscaled), write
/// page-0001.jpg, Vision-OCR it, write page-0001.json. Same on-disk contract
/// as [`ocr_pdf`], including the incremental skip when both files exist.
pub fn ocr_image(
    src: &Path,
    pages_dir: &Path,
    ocr_dir: &Path,
    width: u32,
    progress: ProgressFn,
) -> Result<()> {
    std::fs::create_dir_all(pages_dir)?;
    std::fs::create_dir_all(ocr_dir)?;
    let jpg = pages_dir.join("page-0001.jpg");
    let js = ocr_dir.join("page-0001.json");

    let work = work_for_page(pages_dir, ocr_dir, 1);
    if work.any() {
        // drain autoreleased buffers like process_page does — callers may
        // ingest many images back to back
        let words = autoreleasepool(|_| -> Result<Option<Vec<Word>>> {
            let url = CFURL::from_file_path(src).context("bad image path")?;
            let source = unsafe { CGImageSource::with_url(&url, None) }
                .with_context(|| format!("cannot open {}", src.display()))?;
            // thumbnail_at_index rather than image_at_index: it applies the
            // EXIF orientation (a phone photo would otherwise OCR sideways)
            // and downscales to `width` in one decode, never upscaling
            let yes = unsafe { kCFBooleanTrue }.context("no kCFBooleanTrue")?;
            let max_px = CFNumber::new_i32(width as i32);
            let keys = unsafe {
                [
                    kCGImageSourceCreateThumbnailFromImageAlways,
                    kCGImageSourceCreateThumbnailWithTransform,
                    kCGImageSourceThumbnailMaxPixelSize,
                ]
            };
            let values: [&CFType; 3] = [yes, yes, &max_px];
            let opts = CFDictionary::from_slices(&keys, &values);
            let img = unsafe { source.thumbnail_at_index(0, Some(opts.as_opaque())) }
                .with_context(|| format!("cannot decode {}", src.display()))?;
            if work.render {
                save_jpeg(&img, &jpg)?;
            }
            // a re-render after eviction keeps the words it already has
            work.words.then(|| ocr_words(&img)).transpose()
        })?;
        if let Some(words) = words {
            write_page_json(&js, 1, words)?;
        }
    }
    progress(Progress::Ocr { done: 1, total: 1 });
    let (vision, skipped) = (u32::from(work.words), u32::from(!work.any()));
    let rendered = u32::from(work.render && !work.words);
    progress(Progress::Log(format!(
        "ocr complete: 0 text-layer, {vision} ocr'd, {rendered} re-rendered, \
         {skipped} cached, 1 total"
    )));
    progress(Progress::OcrSummary {
        text_layer: 0,
        vision,
        cached: skipped,
    });
    Ok(())
}

/// Open a PDF and report its page count, as one step.
///
/// Every render of a page from a cold start pays this, so it is separated
/// out to be *timed* separately — see `probe.rs`. On a scanned book the
/// document is hundreds of megabytes and this is not free.
pub(crate) fn open_pdf(pdf: &Path) -> Result<(CFRetained<CGPDFDocument>, usize)> {
    let url = CFURL::from_file_path(pdf).context("bad pdf path")?;
    let doc = CGPDFDocument::with_url(Some(&url))
        .with_context(|| format!("cannot open {}", pdf.display()))?;
    let n = CGPDFDocument::number_of_pages(Some(&doc));
    Ok((doc, n))
}

/// Rasterize page `i` (1-based) to `width` px wide, white background.
pub(crate) fn render_page(
    doc: &CGPDFDocument,
    i: usize,
    width: u32,
) -> Result<CFRetained<CGImage>> {
    let page = CGPDFDocument::page(Some(doc), i).context("no such page")?;
    let media = CGPDFPage::box_rect(Some(&page), CGPDFBox::MediaBox);
    let rot = CGPDFPage::rotation_angle(Some(&page)).rem_euclid(360);
    let (mut pw, mut ph) = (media.size.width, media.size.height);
    if rot % 180 == 90 {
        (pw, ph) = (ph, pw);
    }
    let scale = width as f64 / pw;
    let (w, h) = ((pw * scale) as usize, (ph * scale) as usize);

    let cs = CGColorSpace::new_device_rgb().context("no device RGB colorspace")?;
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            w,
            h,
            8,
            0,
            Some(&cs),
            CGImageAlphaInfo::NoneSkipLast.0,
        )
    }
    .context("cannot create bitmap context")?;
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: w as f64,
            height: h as f64,
        },
    };
    CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&ctx), rect);
    // scale the CTM ourselves: drawing_transform never scales a page UP, so
    // a media box narrower than `width` points would render at natural size,
    // a small island centered in a white canvas. With the context pre-scaled,
    // the transform's only job is rotation + origin (an exact fit, no scaling).
    CGContext::scale_ctm(Some(&ctx), scale, scale);
    let natural = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: pw,
            height: ph,
        },
    };
    let tf = CGPDFPage::drawing_transform(Some(&page), CGPDFBox::MediaBox, natural, 0, true);
    CGContext::concat_ctm(Some(&ctx), tf);
    CGContext::draw_pdf_page(Some(&ctx), Some(&page));
    CGBitmapContextCreateImage(Some(&ctx)).context("cannot snapshot bitmap context")
}

fn save_jpeg(img: &CGImage, path: &Path) -> Result<()> {
    save_jpeg_at(img, path, JPEG_QUALITY)
}

/// `save_jpeg` with the quality spelled out, so the probe can sweep it
/// without every ingest caller having to name the default.
pub(crate) fn save_jpeg_at(img: &CGImage, path: &Path, quality: f64) -> Result<()> {
    let url = CFURL::from_file_path(path).context("bad jpeg path")?;
    let quality = CFNumber::new_f64(quality);
    let opts = CFDictionary::from_slices(
        &[unsafe { kCGImageDestinationLossyCompressionQuality }],
        &[&*quality],
    );
    let dest = unsafe {
        CGImageDestination::with_url(&url, &CFString::from_static_str("public.jpeg"), 1, None)
    }
    .context("cannot create jpeg destination")?;
    unsafe { dest.add_image(img, Some(opts.as_opaque())) };
    if !unsafe { dest.finalize() } {
        bail!("failed to write {}", path.display());
    }
    Ok(())
}

/// OCR one rendered page. Word boxes come back normalized with a top-left
/// origin; recognized lines are split on whitespace with per-token boxes via
/// `boundingBoxForRange` (Vision ranges are UTF-16 code units).
fn ocr_words(img: &CGImage) -> Result<Vec<Word>> {
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            img,
            &NSDictionary::new(),
        )
    };
    let req = VNRecognizeTextRequest::new();
    req.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    req.setUsesLanguageCorrection(true);
    let reqs: [&VNRequest; 1] = [&req];
    handler
        .performRequests_error(&NSArray::from_slice(&reqs))
        .map_err(|e| anyhow::anyhow!("vision request failed: {e}"))?;

    let mut words = Vec::new();
    let Some(observations) = req.results() else {
        return Ok(words);
    };
    for obs in &observations {
        let cands = obs.topCandidates(1);
        let Some(cand) = cands.firstObject() else {
            continue;
        };
        let line = cand.string().to_string();
        for (loc, tok) in split_tokens(&line) {
            let range = NSRange::new(utf16_len(&line[..loc]), utf16_len(tok));
            let Ok(rect_obs) = (unsafe { cand.boundingBoxForRange_error(range) }) else {
                continue;
            };
            let bb = unsafe { rect_obs.boundingBox() };
            words.push(Word {
                t: tok.to_string(),
                x: round5(bb.origin.x),
                // flip to top-left origin
                y: round5(1.0 - bb.origin.y - bb.size.height),
                w: round5(bb.size.width),
                h: round5(bb.size.height),
            });
        }
    }
    Ok(words)
}

/// Whitespace-split `line`, yielding each token with its byte offset.
pub(crate) fn split_tokens(line: &str) -> impl Iterator<Item = (usize, &str)> {
    line.split_whitespace().scan(0usize, |pos, tok| {
        let loc = line[*pos..].find(tok).expect("token comes from this line") + *pos;
        *pos = loc + tok.len();
        Some((loc, tok))
    })
}

pub(crate) fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

pub(crate) fn round5(v: f64) -> f32 {
    ((v * 1e5).round() / 1e5) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pages/ocr pair where `jpg` and `json` say which page-0001 files
    /// exist. No renderer, no PDF — the decision is pure.
    fn work_fixture(name: &str, jpg: bool, json: bool) -> (std::path::PathBuf, PageWork) {
        let dir = std::env::temp_dir().join(format!("ocr-work-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (p, o) = (dir.join("pages"), dir.join("ocr"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&o).unwrap();
        if jpg {
            std::fs::write(p.join("page-0001.jpg"), b"x").unwrap();
        }
        if json {
            std::fs::write(o.join("page-0001.json"), b"{}").unwrap();
        }
        let w = work_for_page(&p, &o, 1);
        (dir, w)
    }

    // The reason this split exists: an evicted page still has its OCR, and
    // asking for it back must not re-run Apple Vision. Under the old single
    // `json && jpg` predicate this page read as "not done" and cost a full
    // recognition pass — minutes per book, redoing work that cannot be
    // improved by redoing it.
    #[test]
    fn an_evicted_page_needs_a_render_and_no_vision() {
        let (dir, w) = work_fixture("evicted", false, true);
        assert_eq!(
            w,
            PageWork {
                render: true,
                words: false
            }
        );
        assert!(w.any(), "it is still work; it is just cheap work");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn work_for_page_covers_the_matrix() {
        // (jpg, json) -> (render, words)
        for (jpg, json, render, words) in [
            (false, false, true, true), // fresh page: everything
            (true, false, false, true), // mid-ingest crash after the jpg
            (false, true, true, false), // evicted render
            (true, true, false, false), // settled: skipped entirely
        ] {
            let (dir, w) = work_fixture(&format!("m{jpg}{json}"), jpg, json);
            assert_eq!(w, PageWork { render, words }, "jpg={jpg} json={json}");
            assert_eq!(w.any(), render || words);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn tokens_carry_byte_offsets() {
        let toks: Vec<_> = split_tokens("  naïve fiancé  x").collect();
        assert_eq!(toks, vec![(2, "naïve"), (9, "fiancé"), (18, "x")]);
    }

    #[test]
    fn utf16_offsets_match_python() {
        // python: len("naïve ".encode("utf-16-le")) // 2 == 6
        assert_eq!(utf16_len("naïve "), 6);
        assert_eq!(utf16_len("𝄞 clef "), 8); // surrogate pair counts as 2
    }

    #[test]
    fn round5_matches_contract() {
        assert_eq!(round5(0.123456789), 0.12346);
        assert_eq!(round5(1.0), 1.0);
    }

    #[test]
    fn ocr_image_skips_cached_page() {
        let dir = std::env::temp_dir().join(format!("ocr-imgcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (pages, ocr) = (dir.join("pages"), dir.join("ocr"));
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::create_dir_all(&ocr).unwrap();
        std::fs::write(pages.join("page-0001.jpg"), b"jpg").unwrap();
        std::fs::write(ocr.join("page-0001.json"), br#"{"page":1,"words":[]}"#).unwrap();

        let mut summary = None;
        let mut cb = |p: Progress| {
            if let Progress::OcrSummary {
                text_layer,
                vision,
                cached,
            } = p
            {
                summary = Some((text_layer, vision, cached));
            }
        };
        // a nonexistent src proves the cache check runs before any decode:
        // reaching ImageIO or Vision here would error, not skip
        ocr_image(Path::new("/nonexistent.png"), &pages, &ocr, 1600, &mut cb).unwrap();
        assert_eq!(summary, Some((0, 0, 1)));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
