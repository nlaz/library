//! Ingest pipeline for The Library, callable in-process (desktop app) or from
//! the CLI in `src/main.rs`.
//!
//! The pipeline is split into prepare/commit phases so a host that shares its
//! stores with live searches only needs exclusive store access for the brief
//! atomic swap:
//!
//!   add_pdf        copy the source PDF into data/pdfs (the library owns it)
//!   prepare_text   render + words (embedded text layer, else Apple Vision
//!                  OCR) -> chunk -> embed                          (no store)
//!   commit_text    upsert new chunks, remove vanished keys         (&mut Library)
//!   prepare_figures  layout detect -> subdivide -> CLIP embed       (no store)
//!   commit_figures   same swap for the figure index                (&mut Images)
//!
//! All progress is reported through a `FnMut(Progress)` callback — no printing
//! here. Nothing in this crate lowers process priority either; that's the
//! caller's call (the CLI drops the whole process to background QoS, the app
//! runs ingest on a utility-QoS worker thread that OCR and ort inherit).

pub mod agent;
pub mod clean;
pub mod layout;
pub mod ocr;
pub mod pdftext;
pub mod status;
pub mod subdivide;
pub mod textout;
pub mod worker;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fastembed::{ImageEmbedding, ImageEmbeddingModel, ImageInitOptions};
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

pub fn doc_id(pdf: &Path) -> String {
    let stem = pdf.file_stem().unwrap_or_default().to_string_lossy();
    let mut id: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    id.trim_matches('-').to_string()
}

type Collections = std::collections::BTreeMap<String, Vec<String>>;

fn load_collections(data: &Path) -> Result<Collections> {
    match std::fs::read(data.join("collections.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("bad collections.json"),
        Err(_) => Ok(Default::default()),
    }
}

/// Written via tmp + rename: searches read this file per query.
fn store_collections(data: &Path, cols: &Collections) -> Result<()> {
    status::write_json_atomic(&data.join("collections.json"), cols)
}

/// data/collections.json: {"archive": ["doc-a", "doc-b"], ...}
pub fn collect(data: &Path, collection: &str, doc: &str) -> Result<()> {
    let mut cols = load_collections(data)?;
    let docs = cols.entry(collection.to_string()).or_default();
    if !docs.iter().any(|d| d == doc) {
        docs.push(doc.to_string());
    }
    store_collections(data, &cols)
}

/// Replace `doc`'s collection membership wholesale: remove it everywhere,
/// then add it to each of `cols` (creating new collections as needed).
/// Collections left empty are pruned — an empty shelf is unreachable in
/// the UI, so keeping one around would just strand it.
pub fn set_collections(data: &Path, doc: &str, cols: &[String]) -> Result<()> {
    let mut all = load_collections(data)?;
    for docs in all.values_mut() {
        docs.retain(|d| d != doc);
    }
    for col in cols {
        let docs = all.entry(col.clone()).or_default();
        if !docs.iter().any(|d| d == doc) {
            docs.push(doc.to_string());
        }
    }
    all.retain(|_, docs| !docs.is_empty());
    store_collections(data, &all)
}

/// Copy the source PDF into `data/pdfs/<doc>.pdf` and return `(doc, copy)`.
/// The library owns its documents: the drop source may be unplugged, moved,
/// or deleted before a queued job runs.
pub fn add_pdf(ctx: &IngestCtx, pdf: &Path, name: Option<String>) -> Result<(String, PathBuf)> {
    if !pdf.exists() {
        bail!("no such file: {}", pdf.display());
    }
    let doc = name.unwrap_or_else(|| doc_id(pdf));
    if doc.is_empty() {
        bail!("cannot derive a doc id from {}", pdf.display());
    }
    let dir = ctx.data.join("pdfs");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{doc}.pdf"));
    if pdf.canonicalize().ok() != dest.canonicalize().ok() {
        std::fs::copy(pdf, &dest).context("copying PDF into the library")?;
    }
    Ok((doc, dest))
}

/// Whether two files hold identical bytes (size check first).
fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// Move the source PDF into `data/pdfs/<doc>.pdf` and return `(doc, dest)`.
/// Same-volume moves are a rename; across volumes it copies to a hidden
/// temp name, verifies, renames into place, and only then deletes the
/// source. If the doc already exists: identical bytes leave both files
/// alone (already in the library — the source is NOT deleted); different
/// bytes are an error rather than a silent overwrite.
pub fn move_pdf(ctx: &IngestCtx, pdf: &Path, name: Option<String>) -> Result<(String, PathBuf)> {
    if !pdf.exists() {
        bail!("no such file: {}", pdf.display());
    }
    let doc = name.unwrap_or_else(|| doc_id(pdf));
    if doc.is_empty() {
        bail!("cannot derive a doc id from {}", pdf.display());
    }
    let dir = ctx.data.join("pdfs");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{doc}.pdf"));

    if pdf.canonicalize().ok() == dest.canonicalize().ok() {
        return Ok((doc, dest)); // already home
    }
    if dest.exists() {
        if same_bytes(pdf, &dest)? {
            return Ok((doc, dest));
        }
        bail!("a different '{doc}' is already in the library — rename the file and try again");
    }

    match std::fs::rename(pdf, &dest) {
        Ok(()) => {}
        // EXDEV: source is on another volume (drive, DMG). Copy safely,
        // then remove the source only after the copy is in place.
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let tmp = dir.join(format!(".{doc}.pdf.tmp"));
            let n = std::fs::copy(pdf, &tmp).context("copying PDF into the library")?;
            if n != std::fs::metadata(pdf)?.len() {
                let _ = std::fs::remove_file(&tmp);
                bail!("short copy moving {} into the library", pdf.display());
            }
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &dest)?;
            std::fs::remove_file(pdf).context("removing the moved source PDF")?;
        }
        Err(e) => return Err(e).context("moving PDF into the library"),
    }
    Ok((doc, dest))
}

/// Render + extract words (cached per page), chunk, and embed a doc.
/// Touches no store — safe to run while searches are live. Also returns
/// the doc's pages (cleaned where cleanup ran, raw elsewhere) so callers
/// like the markdown edition don't re-read the whole doc.
pub fn prepare_text(
    ctx: &IngestCtx,
    pdf: &Path,
    doc: &str,
    limit: Option<usize>,
    progress: ProgressFn,
) -> Result<(Vec<ChunkRec>, Vec<PageOcr>)> {
    let pages_dir = ctx.data.join("pages").join(doc);
    let ocr_dir = ctx.data.join("ocr").join(doc);
    std::fs::create_dir_all(&pages_dir)?;
    std::fs::create_dir_all(&ocr_dir)?;

    // 1. render + words (cached: pages that already have JSON are skipped)
    ocr::ocr_pdf(
        pdf,
        &pages_dir,
        &ocr_dir,
        ctx.width,
        limit,
        ctx.text_layer,
        progress,
    )?;

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
    let counts = st.wtx(|tx| {
        let old: Vec<ChunkKey> = tx.rtx(|(_, (manifest, _))| manifest.search(&doc.to_string()));
        let new: FxHashSet<&ChunkKey> = recs.iter().map(|r| &r.key).collect();
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

fn inter_area(a: Bbox, b: Bbox) -> f32 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    w.max(0.0) * h.max(0.0)
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
            if regions.is_empty() {
                return out;
            }
            let Ok(img) = image::open(&jpg) else {
                return out;
            };
            (img, regions)
        }
    };
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
        if region_inked(&luma, bbox) {
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
    let model = layout::LayoutModel::load(&ctx.data)?;

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
            .map(|page| page_figures(doc, &pages_dir, model.as_ref(), page))
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

    // 2. embed, draining so crops free as batches complete
    let model = ImageEmbedding::try_new(
        ImageInitOptions::new(ImageEmbeddingModel::ClipVitB32)
            .with_cache_dir(ctx.data.join("models"))
            .with_show_download_progress(true),
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
    fn move_pdf_relocates_dedups_and_rejects_conflicts() {
        let dir = std::env::temp_dir().join(format!("fold-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let ctx = IngestCtx {
            data: dir.join("data"),
            width: 1600,
            clean: false,
            text_layer: true,
        };

        // move: source disappears, dest exists
        let src = dir.join("src/My Book.pdf");
        std::fs::write(&src, b"%PDF-alpha").unwrap();
        let (doc, dest) = move_pdf(&ctx, &src, None).unwrap();
        assert_eq!(doc, "my-book");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"%PDF-alpha");

        // identical bytes already in the library: no-op, source kept
        std::fs::write(&src, b"%PDF-alpha").unwrap();
        let (doc2, _) = move_pdf(&ctx, &src, None).unwrap();
        assert_eq!(doc2, "my-book");
        assert!(src.exists(), "duplicate source must not be deleted");

        // different bytes under the same doc id: refuse, don't overwrite
        std::fs::write(&src, b"%PDF-beta").unwrap();
        assert!(move_pdf(&ctx, &src, None).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"%PDF-alpha");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_collections_replaces_and_prunes() {
        let dir = std::env::temp_dir().join(format!("fold-cols-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        collect(&dir, "a", "doc-1").unwrap();
        collect(&dir, "a", "doc-2").unwrap();
        collect(&dir, "b", "doc-1").unwrap();

        // move doc-1 from {a, b} to {b, c}; c is created on the fly
        set_collections(&dir, "doc-1", &["b".into(), "c".into()]).unwrap();
        let cols = load_collections(&dir).unwrap();
        assert_eq!(cols["a"], vec!["doc-2"]);
        assert_eq!(cols["b"], vec!["doc-1"]);
        assert_eq!(cols["c"], vec!["doc-1"]);

        // removing the last member prunes the collection entirely
        set_collections(&dir, "doc-1", &[]).unwrap();
        let cols = load_collections(&dir).unwrap();
        assert_eq!(cols.keys().collect::<Vec<_>>(), vec!["a"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
