//! Ingest pipeline for The Library, callable in-process (desktop app) or from
//! the CLI in `src/main.rs`.
//!
//! The pipeline is split into prepare/commit phases so a host that shares its
//! stores with live searches only needs exclusive store access for the brief
//! atomic swap:
//!
//!   add_doc        copy the source file (pdf/png/jpeg/heic) into data/pdfs
//!                  (the library owns it)
//!   prepare_text   render + words (embedded text layer, else Apple Vision
//!                  OCR; images render once and always OCR)
//!                                                                 (no store)
//!   commit_text    upsert new chunks, remove vanished keys         (&mut Library)
//!   prepare_figures  layout detect -> subdivide -> CLIP embed       (no store)
//!   commit_figures   same swap for the figure index                (&mut Images)
//!
//! All progress is reported through a `FnMut(Progress)` callback — no printing
//! here. Nothing in this crate lowers process priority either; that's the
//! caller's call (the CLI drops the whole process to background QoS, the app
//! runs ingest on a utility-QoS worker thread that OCR and ort inherit).

pub mod clean;
pub mod layout;
pub mod migrate;
pub mod models;
pub mod ocr;
pub mod pdftext;
pub mod status;
pub mod subdivide;
pub mod textout;
pub mod worker;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fastembed::{ImageEmbedding, ImageEmbeddingModel, ImageInitOptions};
use library_core::meta::Meta;
pub use library_core::records::{SOURCE_EXTS, SourceKind};
use library_core::{
    Bbox, ChunkKey, ChunkRec, ClipEmb, Emb, FxHashSet, ImageKey, ImageRec, Images, Library, Word,
};
use serde::{Deserialize, Serialize};

const CHUNK_WORDS: usize = 200;
const CHUNK_STRIDE: usize = 160; // 40 words of overlap between neighbors
// >= 16 lets ese's rayon path fan a batch across cores
const EMBED_BATCH: usize = 128;

/// Minimum figure height, as a fraction of the page (~4x a text line).
const FIG_MIN_H: f32 = 0.07;
/// A region covering this much of the page counts as "the whole page" for
/// the image-doc full-page guarantee.
const FULL_PAGE_AREA: f32 = 0.9;
/// Fraction of dark pixels a candidate region must contain.
const FIG_MIN_INK: f64 = 0.01;
const CLIP_BATCH: usize = 8;
/// Longest edge of the per-page grayscale downscale that ink checks and
/// subdivision profiles read — full-res pixels are only touched for crops.
pub const PAGE_LUMA_PX: u32 = 768;
/// Longest edge of a stored figure crop. CLIP resizes to 224px anyway;
/// keeping crops at render resolution swaps an 8GB machine on art books.
const CROP_MAX_PX: u32 = 448;

/// Everything the pipeline needs besides the stores and models. `data`
/// should be absolute when the caller's CWD is not the repo root (a .app
/// bundle launches at `/`).
#[derive(Clone)]
pub struct IngestCtx {
    pub data: PathBuf,
    /// The metadata database. Shared rather than owned: one process opens
    /// it once, and the ingest loop, the search surfaces and the commands
    /// all write through the same handle.
    pub meta: std::sync::Arc<library_core::meta::Meta>,
    /// Rendered page-image width in pixels.
    pub width: u32,
    /// Run the model-backed OCR cleanup during `prepare_text`. Off by
    /// default: the on-device model keeps ~2GB resident for the whole pass
    /// (an hour for a book), which a caller must opt into knowingly.
    /// Cached edits (data/edits) are applied either way — that part is
    /// cheap, local, and model-free.
    pub clean: bool,
    /// Prefer embedded PDF text layers over Vision OCR, page by page. On
    /// by default; turn off for PDFs whose producer embedded garbage OCR.
    pub text_layer: bool,
}

/// Pipeline progress, reported as work happens.
#[derive(Debug, Clone)]
pub enum Progress {
    /// A human-readable pipeline event (summaries, per-page warnings).
    Log(String),
    Ocr {
        done: u32,
        total: u32,
    },
    /// End-of-OCR page-source split, for the persisted ingest metrics.
    OcrSummary {
        text_layer: u32,
        vision: u32,
        cached: u32,
    },
    /// Model-backed OCR cleanup, counted in pages.
    Clean {
        done: usize,
        total: usize,
    },
    Embed {
        done: usize,
        total: usize,
    },
    /// Figure detection, counted in pages.
    Figures {
        done: usize,
        total: usize,
    },
    /// CLIP embedding of figure crops.
    Clip {
        done: usize,
        total: usize,
    },
    /// First-run fetch of a model's weights, in **bytes** (every other
    /// variant counts pages or records). Only ever emitted once per machine
    /// per model — see [`models::watch_download`].
    Download {
        done: u64,
        total: u64,
    },
    /// Committing prepared records to a store (emitted by the worker loop).
    Indexing,
}

pub type ProgressFn<'a> = &'a mut dyn FnMut(Progress);

#[derive(Serialize, Deserialize)]
pub struct PageOcr {
    pub page: u32,
    pub words: Vec<Word>,
}

/// A doc's pages, preferring cleaned pages (`data/clean/<doc>`) over raw OCR
/// (`data/ocr/<doc>`) page by page. Both directories hold the same
/// `page-NNNN.json` `PageOcr` schema; `clean/` is sparse (only pages the
/// cleanup pass changed), so absence just means "raw is canonical".
pub fn read_pages(data: &Path, doc: &str) -> Result<Vec<PageOcr>> {
    let clean = data.join("clean").join(doc);
    let mut pages = read_ocr(&data.join("ocr").join(doc))?;
    for p in &mut pages {
        let f = clean.join(format!("page-{:04}.json", p.page));
        if let Ok(bytes) = std::fs::read(&f) {
            *p = serde_json::from_slice(&bytes)
                .context(format!("bad clean json {}", f.display()))?;
        }
    }
    Ok(pages)
}

pub fn read_ocr(ocr_dir: &Path) -> Result<Vec<PageOcr>> {
    let mut pages: Vec<PageOcr> = Vec::new();
    for entry in std::fs::read_dir(ocr_dir)? {
        let p = entry?.path();
        if p.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let bytes =
            std::fs::read(&p).with_context(|| format!("reading OCR json {}", p.display()))?;
        let page = serde_json::from_slice(&bytes)
            .with_context(|| format!("bad OCR json {}", p.display()))?;
        pages.push(page);
    }
    pages.sort_by_key(|p: &PageOcr| p.page);
    Ok(pages)
}

/// Where a document's file lives, according to the folder scanner.
///
/// A doc id no longer encodes a path — this is a lookup, and it returns
/// `None` for a document whose file has gone missing (which is a state the
/// caller must handle, not an error).
pub fn source_path(meta: &Meta, doc: &str) -> Option<PathBuf> {
    meta.doc_path(doc)
}

/// Bring a dropped or picked file into the library by copying it into
/// `dest_dir` (a watched root). The scanner mints the document on its next
/// pass — this only puts the bytes somewhere it will look.
///
/// Name collisions get a numeric suffix rather than an error: two unrelated
/// `scan.pdf`s are an ordinary thing to have, and the doc id no longer comes
/// from the filename, so they cost nothing but a distinguishable name.
pub fn copy_into(src: &Path, dest_dir: &Path) -> Result<PathBuf> {
    if !src.exists() {
        bail!("no such file: {}", src.display());
    }
    if SourceKind::of(src).is_none() {
        bail!(
            "unsupported file type: {} (want pdf, png, jpg, jpeg, or heic)",
            src.display()
        );
    }
    std::fs::create_dir_all(dest_dir)?;
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut dest = dest_dir.join(src.file_name().unwrap_or_default());
    let mut n = 1;
    while dest.exists() {
        // already the same bytes: it is in the library, leave it alone
        if same_bytes(src, &dest).unwrap_or(false) {
            return Ok(dest);
        }
        dest = dest_dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    std::fs::copy(src, &dest).context("copying the file into the library")?;
    Ok(dest)
}

/// Whether two files hold identical bytes (size check first).
fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// Render + extract words (cached per page), chunk, and embed a doc.
/// Touches no store — safe to run while searches are live. Also returns
/// the doc's pages (cleaned where cleanup ran, raw elsewhere) so callers
/// like the markdown edition don't re-read the whole doc.
pub fn prepare_text(
    ctx: &IngestCtx,
    src: &Path,
    doc: &str,
    limit: Option<usize>,
    progress: ProgressFn,
) -> Result<(Vec<ChunkRec>, Vec<PageOcr>)> {
    let pages_dir = ctx.data.join("pages").join(doc);
    let ocr_dir = ctx.data.join("ocr").join(doc);
    std::fs::create_dir_all(&pages_dir)?;
    std::fs::create_dir_all(&ocr_dir)?;

    // 1. render + words (cached: pages that already have JSON are skipped)
    match SourceKind::of(src) {
        Some(SourceKind::Pdf) => ocr::ocr_pdf(
            src,
            &pages_dir,
            &ocr_dir,
            ctx.width,
            limit,
            ctx.text_layer,
            progress,
        )?,
        // an image is a one-page doc; no text layer, always Vision
        Some(SourceKind::Image) => ocr::ocr_image(src, &pages_dir, &ocr_dir, ctx.width, progress)?,
        None => bail!("unsupported source file: {}", src.display()),
    }

    prepare_text_cached(ctx, doc, limit, progress)
}

/// [`prepare_text`] from the cached page words alone — no source PDF, no
/// render/OCR pass. For rebuilding a doc's index entries when only the
/// caches survive (or after a store-schema change).
pub fn prepare_text_cached(
    ctx: &IngestCtx,
    doc: &str,
    limit: Option<usize>,
    progress: ProgressFn,
) -> Result<(Vec<ChunkRec>, Vec<PageOcr>)> {
    // 2. OCR cleanup + read. The model pass is opt-in (ctx.clean) — it
    // parks a ~2GB model in memory for the whole run. Cached edits always
    // get (re)applied: that's file-local and costs nothing. Both cleanup
    // paths hand back the final pages, so the doc is read exactly once.
    let pages = if ctx.clean {
        clean::clean_doc(&ctx.data, doc, progress)?.1
    } else if ctx.data.join("edits").join(doc).is_dir() {
        clean::apply_edits(&ctx.data, doc, progress)?.1
    } else {
        read_pages(&ctx.data, doc)?
    };

    // 3. chunk: page-bounded sliding windows in reading order. Only the
    // first `limit` pages chunk; the full set is still returned.
    let upto = limit.unwrap_or(pages.len()).min(pages.len());
    let mut chunks: Vec<(ChunkKey, Vec<Word>)> = Vec::new();
    for page in &pages[..upto] {
        let mut idx = 0u32;
        let mut start = 0usize;
        while start < page.words.len() {
            let end = (start + CHUNK_WORDS).min(page.words.len());
            chunks.push((
                ChunkKey {
                    doc: doc.to_string(),
                    page: page.page,
                    idx,
                },
                page.words[start..end].to_vec(),
            ));
            if end == page.words.len() {
                break;
            }
            start += CHUNK_STRIDE;
            idx += 1;
        }
    }

    // 4. embed (ese: compile-time static embeddings, no model to load),
    // batched so progress stays visible
    let mut embs: Vec<Emb> = Vec::with_capacity(chunks.len());
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch
            .iter()
            .map(|(_, words)| {
                words
                    .iter()
                    .map(|w| w.t.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        embs.extend(ese::encode(&texts));
        progress(Progress::Embed {
            done: embs.len(),
            total: chunks.len(),
        });
    }

    let recs = chunks
        .into_iter()
        .zip(embs)
        .map(|((key, words), emb)| ChunkRec { key, words, emb })
        .collect();
    Ok((recs, pages))
}

/// Atomic swap: upsert the doc's new chunks, remove keys that vanished,
/// checkpoint. The table retracts replaced records itself and byte-equal
/// upserts skip the graph, so an unchanged chunk costs one point read.
/// The only text-pipeline step that needs exclusive store access.
/// Returns (removed, added) — removed counts keys actually deleted.
pub fn commit_text(st: &mut Library, doc: &str, recs: &[ChunkRec]) -> (usize, usize) {
    library_core::store::commit_chunks(st, doc, recs)
}

// ---------------------------------------------------------------------------
// Figure regions: layout model + vertical gaps in the OCR word layout.
// ---------------------------------------------------------------------------

/// Candidate figure bboxes on a page: bands of the text column with no words.
fn detect_regions(words: &[Word]) -> Vec<Bbox> {
    // merge word boxes into occupied y-bands
    let mut spans: Vec<(f32, f32)> = words.iter().map(|w| (w.y, w.y + w.h)).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut bands: Vec<(f32, f32)> = Vec::new();
    for (a, b) in spans {
        match bands.last_mut() {
            Some(last) if a <= last.1 + 0.012 => last.1 = last.1.max(b),
            _ => bands.push((a, b)),
        }
    }

    // figure x-extent: the text column if there is one, else trimmed margins
    let (x0, x1) = if words.is_empty() {
        (0.06, 0.94)
    } else {
        (
            words.iter().map(|w| w.x).fold(1f32, f32::min),
            words.iter().map(|w| w.x + w.w).fold(0f32, f32::max),
        )
    };

    let (top, bot) = (0.04f32, 0.96f32);
    let mut regions = Vec::new();
    let mut prev = top;
    for (a, b) in bands.into_iter().chain(std::iter::once((bot, bot))) {
        if a - prev >= FIG_MIN_H {
            regions.push([x0, prev, x1 - x0, a - prev]);
        }
        prev = prev.max(b);
    }
    regions
}

/// Whether `bbox` contains ink, judged on the page's shared grayscale
/// downscale (scans are full of legitimately blank gaps).
fn region_inked(luma: &image::GrayImage, bbox: Bbox) -> bool {
    let (lw, lh) = (luma.width() as f32, luma.height() as f32);
    let x0 = (bbox[0] * lw) as u32;
    let y0 = (bbox[1] * lh) as u32;
    let x1 = (((bbox[0] + bbox[2]) * lw).ceil() as u32).min(luma.width());
    let y1 = (((bbox[1] + bbox[3]) * lh).ceil() as u32).min(luma.height());
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    let mut dark = 0usize;
    for y in y0..y1 {
        for x in x0..x1 {
            dark += usize::from(luma.get_pixel(x, y).0[0] < 160);
        }
    }
    dark as f64 / ((x1 - x0) as u64 * (y1 - y0) as u64) as f64 >= FIG_MIN_INK
}

/// Crop `bbox` for CLIP, downscaled right away: the encoder resizes to
/// 224px, so render-resolution crops are pure memory pressure.
fn crop_for_clip(page: &image::DynamicImage, bbox: Bbox) -> image::DynamicImage {
    let (iw, ih) = (page.width() as f32, page.height() as f32);
    page.crop_imm(
        (bbox[0] * iw) as u32,
        (bbox[1] * ih) as u32,
        (bbox[2] * iw).max(1.0) as u32,
        (bbox[3] * ih).max(1.0) as u32,
    )
    .thumbnail(CROP_MAX_PX, CROP_MAX_PX)
}

fn inter_area(a: Bbox, b: Bbox) -> f32 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    w.max(0.0) * h.max(0.0)
}

/// One page's contribution to the figure index, produced off-thread.
struct PageFigures {
    keys: Vec<(ImageKey, Bbox)>,
    crops: Vec<image::DynamicImage>,
    log: Option<String>,
}

fn page_figures(
    doc: &str,
    pages_dir: &Path,
    model: Option<&layout::LayoutModel>,
    page: &PageOcr,
    ensure_full: bool,
) -> PageFigures {
    let mut out = PageFigures {
        keys: Vec::new(),
        crops: Vec::new(),
        log: None,
    };
    let jpg = pages_dir.join(format!("page-{:04}.jpg", page.page));
    let (img, regions): (image::DynamicImage, Vec<Bbox>) = match model {
        Some(m) => {
            let Ok(img) = image::open(&jpg) else {
                return out;
            };
            let mut dets: Vec<layout::Detection> = match m.detect(&img) {
                Ok(d) => d,
                Err(e) => {
                    out.log = Some(format!("layout failed on p.{}: {e:#}", page.page));
                    return out;
                }
            };
            dets.retain(|d| d.class.is_figure() && d.bbox[2] * d.bbox[3] >= layout::AREA_MIN);
            let mut regions: Vec<Bbox> = dets.into_iter().map(|d| d.bbox).collect();
            // union: keep heuristic gap-bands the model didn't cover, so a
            // whiffed full-bleed spread still gets indexed
            for hb in detect_regions(&page.words) {
                let covered = regions
                    .iter()
                    .any(|mb| inter_area(hb, *mb) > 0.3 * (hb[2] * hb[3]));
                if !covered {
                    regions.push(hb);
                }
            }
            (img, regions)
        }
        None => {
            let regions = detect_regions(&page.words);
            if regions.is_empty() && !ensure_full {
                return out;
            }
            let Ok(img) = image::open(&jpg) else {
                return out;
            };
            (img, regions)
        }
    };
    // image-sourced docs must always be CLIP-findable: detection is tuned
    // for document pages (YOLO/DocLayNet, word-gap bands) and can come up
    // empty or partial on a photo, so guarantee a whole-page region
    let mut regions = regions;
    if ensure_full && !regions.iter().any(|r| r[2] * r[3] >= FULL_PAGE_AREA) {
        regions.push([0.0, 0.0, 1.0, 1.0]);
    }
    // ink checks and subdivision profiles read this shared downscale;
    // full-res pixels are only touched for accepted crops
    let luma = img.thumbnail(PAGE_LUMA_PX, PAGE_LUMA_PX).into_luma8();
    let full = (img.width(), img.height());
    // whole figures AND their component parts get indexed; the server
    // groups per page at query time so parts don't spam results
    let mut with_parts = regions.clone();
    for r in &regions {
        with_parts.extend(subdivide::subdivide(&luma, full, *r));
    }
    let mut regions = with_parts;
    // stable idx assignment in reading order (total_cmp: a NaN coordinate
    // from the layout model must not panic the ingest worker)
    regions.sort_by(|a, b| a[1].total_cmp(&b[1]).then(a[0].total_cmp(&b[0])));
    let mut idx = 0u32;
    for bbox in regions {
        // the guaranteed whole-page region skips the ink gate: an
        // overexposed white-background photo must still index
        let full_page = ensure_full && bbox[2] * bbox[3] >= FULL_PAGE_AREA;
        if full_page || region_inked(&luma, bbox) {
            out.keys.push((
                ImageKey {
                    doc: doc.to_string(),
                    page: page.page,
                    idx,
                },
                bbox,
            ));
            out.crops.push(crop_for_clip(&img, bbox));
            idx += 1;
        }
    }
    out
}

/// Detect and CLIP-embed a doc's figure regions from its cached OCR + page
/// renders. Touches no store. Loads the CLIP image encoder only when there
/// is something to embed and drops it after (it's ~350MB resident).
pub fn prepare_figures(ctx: &IngestCtx, doc: &str, progress: ProgressFn) -> Result<Vec<ImageRec>> {
    use rayon::prelude::*;

    let pages = read_ocr(&ctx.data.join("ocr").join(doc))?;
    let pages_dir = ctx.data.join("pages").join(doc);
    // Once per machine. The app fetches this at launch, so normally this is
    // a size check; the path that matters is the `worker` CLI running on a
    // machine whose app has never been opened. Log-and-continue: without the
    // detector, figures come from word gaps alone (see `page_figures`), which
    // is a recall loss and not a failure.
    if let Err(e) = models::ensure_layout(&ctx.data, |done, total| {
        progress(Progress::Download { done, total })
    }) {
        progress(Progress::Log(format!(
            "page-layout model unavailable, using word gaps only: {e:#}"
        )));
    }
    let model = layout::LayoutModel::load(&ctx.data)?;
    // PDFs (including cache-only reindexes whose source is gone) keep
    // their exact pre-image behavior — the guarantee is image-docs only
    let ensure_full = matches!(
        source_path(&ctx.meta, doc)
            .as_deref()
            .and_then(SourceKind::of),
        Some(SourceKind::Image)
    );

    // 1. detect + crop, page-parallel (ort sessions run concurrently).
    // Chunked because the progress callback isn't Send: workers hand
    // results back and this thread reports between batches.
    let chunk = 2 * rayon::current_num_threads().max(1);
    let mut keys: Vec<(ImageKey, Bbox)> = Vec::new();
    let mut crops: Vec<image::DynamicImage> = Vec::new();
    let mut done = 0usize;
    for group in pages.chunks(chunk) {
        progress(Progress::Figures {
            done,
            total: pages.len(),
        });
        let results: Vec<PageFigures> = group
            .par_iter()
            .map(|page| page_figures(doc, &pages_dir, model.as_ref(), page, ensure_full))
            .collect();
        for mut r in results {
            if let Some(line) = r.log.take() {
                progress(Progress::Log(line));
            }
            keys.append(&mut r.keys);
            crops.append(&mut r.crops);
        }
        done += group.len();
    }
    progress(Progress::Figures {
        done: pages.len(),
        total: pages.len(),
    });

    if crops.is_empty() {
        return Ok(Vec::new()); // nothing to embed: skip the CLIP load
    }

    // 2. embed, draining so crops free as batches complete. The app caches
    // this encoder at launch (`models::ensure_clip_vision`), so normally the
    // construction below is a load from disk. It stays wrapped because the
    // `worker` CLI can run on a machine whose app has never been opened,
    // and there the same call *downloads* ~335 MB first — minutes of
    // otherwise unexplained stall partway through someone's first ingest.
    let models_dir = ctx.data.join("models");
    let model = models::watch_download(
        &models_dir,
        models::CLIP_VISION_BYTES,
        || {
            ImageEmbedding::try_new(
                ImageInitOptions::new(ImageEmbeddingModel::ClipVitB32)
                    .with_cache_dir(models_dir.clone())
                    .with_show_download_progress(true),
            )
        },
        |done, total| progress(Progress::Download { done, total }),
    )?;
    let total = crops.len();
    let mut recs: Vec<ImageRec> = Vec::with_capacity(keys.len());
    let mut it = keys.into_iter();
    while !crops.is_empty() {
        let batch: Vec<_> = crops.drain(..CLIP_BATCH.min(crops.len())).collect();
        for e in model.embed_images(batch)? {
            let (key, bbox) = it.next().expect("one key per crop");
            let emb: ClipEmb = e.try_into().expect("CLIP emits 512-dim vectors");
            recs.push(ImageRec { key, bbox, emb });
        }
        progress(Progress::Clip {
            done: recs.len(),
            total,
        });
    }
    Ok(recs)
}

/// Atomic swap for the figure index; see [`commit_text`].
/// Returns (removed, added).
pub fn commit_figures(st: &mut Images, doc: &str, recs: &[ImageRec]) -> (usize, usize) {
    let counts = st.wtx(|tx| {
        let old: Vec<ImageKey> = tx.rtx(|(_, (_, manifest))| manifest.search(&doc.to_string()));
        let new: FxHashSet<&ImageKey> = recs.iter().map(|r| &r.key).collect();
        for rec in recs {
            tx.upsert(&rec.key, rec);
        }
        let mut removed = 0;
        for key in old {
            if !new.contains(&key) {
                tx.remove(&key);
                removed += 1;
            }
        }
        (removed, recs.len())
    });
    st.checkpoint();
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_classifies_by_extension() {
        assert_eq!(SourceKind::of(Path::new("a.pdf")), Some(SourceKind::Pdf));
        assert_eq!(SourceKind::of(Path::new("a.PDF")), Some(SourceKind::Pdf));
        assert_eq!(SourceKind::of(Path::new("a.png")), Some(SourceKind::Image));
        assert_eq!(SourceKind::of(Path::new("a.JPG")), Some(SourceKind::Image));
        assert_eq!(SourceKind::of(Path::new("a.jpeg")), Some(SourceKind::Image));
        assert_eq!(SourceKind::of(Path::new("a.HEIC")), Some(SourceKind::Image));
        assert_eq!(SourceKind::of(Path::new("a.txt")), None);
        assert_eq!(SourceKind::of(Path::new("no-extension")), None);
    }

    #[test]
    fn source_path_follows_the_file_to_wherever_it_lives() {
        let dir = std::env::temp_dir().join(format!("fold-srcpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root_dir = dir.join("Library");
        std::fs::create_dir_all(&root_dir).expect("root");
        std::fs::write(root_dir.join("book.pdf"), b"%PDF").expect("book");

        let ctx = library_core::meta::Ctx::in_memory(&dir).expect("meta");
        let root = ctx.add_root(&root_dir, 1).expect("link");
        library_core::roots::sync_root(&ctx.meta, &root, 2);

        let doc = ctx
            .files_in_root(&root.id)
            .first()
            .expect("one file")
            .doc
            .clone();
        // the root's own path, not the one we passed in: linking
        // canonicalizes, and on macOS /var is a symlink to /private/var
        assert_eq!(
            source_path(&ctx.meta, &doc),
            Some(root.path.join("book.pdf"))
        );
        // a doc we have never heard of resolves to nothing, not an error
        assert_eq!(source_path(&ctx.meta, "dNOPE"), None);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn copy_into_never_clobbers_and_dedups_identical_bytes() {
        let dir = std::env::temp_dir().join(format!("fold-copyinto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src_dir = dir.join("Desktop");
        let lib = dir.join("Library");
        std::fs::create_dir_all(&src_dir).expect("src dir");

        let a = src_dir.join("scan.pdf");
        std::fs::write(&a, b"%PDF-alpha").expect("write a");
        assert_eq!(copy_into(&a, &lib).expect("copy"), lib.join("scan.pdf"));
        assert!(a.exists(), "the source is copied, never moved");

        // the same bytes again: already in the library, no second file
        assert_eq!(copy_into(&a, &lib).expect("copy"), lib.join("scan.pdf"));
        assert_eq!(std::fs::read_dir(&lib).expect("ls").count(), 1);

        // a *different* file that happens to share a name gets a suffix
        // rather than an error — the doc id no longer comes from the name
        std::fs::write(&a, b"%PDF-beta").expect("rewrite a");
        assert_eq!(copy_into(&a, &lib).expect("copy"), lib.join("scan (1).pdf"));
        assert_eq!(
            std::fs::read(lib.join("scan.pdf")).expect("read"),
            b"%PDF-alpha",
            "the first file is never overwritten"
        );

        // not an ingestible type at all
        let txt = src_dir.join("notes.txt");
        std::fs::write(&txt, b"hi").expect("write txt");
        assert!(copy_into(&txt, &lib).is_err());
        assert!(copy_into(&src_dir.join("nope.pdf"), &lib).is_err());

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// The full-page guarantee: an image-sourced doc always yields at least
    /// one CLIP-indexable region, even when detection finds nothing and the
    /// ink gate would reject a blank-looking photo.
    #[test]
    fn page_figures_full_page_guarantee() {
        let dir = std::env::temp_dir().join(format!("fold-figfull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let page = PageOcr {
            page: 1,
            words: Vec::new(),
        };

        // white page: no ink anywhere, so PDF behavior finds nothing
        image::RgbImage::from_pixel(200, 200, image::Rgb([255, 255, 255]))
            .save(dir.join("page-0001.jpg"))
            .unwrap();
        let out = page_figures("d", &dir, None, &page, false);
        assert!(out.keys.is_empty(), "pdf docs keep the ink gate");

        let out = page_figures("d", &dir, None, &page, true);
        assert_eq!(out.keys.len(), 1, "image docs index the whole page");
        let (key, bbox) = &out.keys[0];
        assert_eq!((key.page, key.idx), (1, 0));
        assert_eq!(*bbox, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(out.crops.len(), 1);

        // dark page: heuristic band (~0.81 area) passes the ink gate but is
        // below FULL_PAGE_AREA, so the true full page is still added — once
        image::RgbImage::from_pixel(200, 200, image::Rgb([10, 10, 10]))
            .save(dir.join("page-0001.jpg"))
            .unwrap();
        let out = page_figures("d", &dir, None, &page, true);
        let full: Vec<_> = out
            .keys
            .iter()
            .filter(|(_, b)| b[2] * b[3] >= FULL_PAGE_AREA)
            .collect();
        assert_eq!(full.len(), 1, "exactly one whole-page region");
        assert_eq!(full[0].1, [0.0, 0.0, 1.0, 1.0]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
