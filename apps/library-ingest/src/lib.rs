//! Ingest pipeline for The Library, callable in-process (desktop app) or from
//! the CLI in `src/main.rs`.
//!
//! The pipeline is split into prepare/commit phases so a host that shares its
//! stores with live searches only needs exclusive store access for the brief
//! atomic swap:
//!
//!   add_pdf        copy the source PDF into data/pdfs (the library owns it)
//!   prepare_text   render + OCR (Apple Vision) -> chunk -> embed    (no store)
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
pub mod ocr;
pub mod subdivide;
pub mod textout;

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
}

/// Pipeline progress, reported as work happens.
#[derive(Debug, Clone)]
pub enum Progress {
    /// A human-readable pipeline event (summaries, per-page warnings).
    Log(String),
    Ocr { done: u32, total: u32 },
    /// Model-backed OCR cleanup, counted in pages.
    Clean { done: usize, total: usize },
    Embed { done: usize, total: usize },
    /// Figure detection, counted in pages.
    Figures { done: usize, total: usize },
    /// CLIP embedding of figure crops.
    Clip { done: usize, total: usize },
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
    let mut pages: Vec<PageOcr> = std::fs::read_dir(ocr_dir)?
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "json").then(|| {
                serde_json::from_slice(&std::fs::read(&p).unwrap())
                    .context(format!("bad OCR json {}", p.display()))
                    .unwrap()
            })
        })
        .collect();
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
    let tmp = data.join("collections.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cols)?)?;
    std::fs::rename(&tmp, data.join("collections.json"))?;
    Ok(())
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

/// Render + OCR (cached per page), chunk, and embed a doc.
/// Touches no store — safe to run while searches are live.
pub fn prepare_text(
    ctx: &IngestCtx,
    pdf: &Path,
    doc: &str,
    limit: Option<usize>,
    progress: ProgressFn,
) -> Result<Vec<ChunkRec>> {
    let pages_dir = ctx.data.join("pages").join(doc);
    let ocr_dir = ctx.data.join("ocr").join(doc);
    std::fs::create_dir_all(&pages_dir)?;
    std::fs::create_dir_all(&ocr_dir)?;

    // 1. render + OCR (cached: pages that already have JSON are skipped)
    ocr::ocr_pdf(pdf, &pages_dir, &ocr_dir, ctx.width, limit, progress)?;

    // 2. OCR cleanup. The model pass is opt-in (ctx.clean) — it parks a
    // ~2GB model in memory for the whole run. Cached edits always get
    // (re)applied: that's file-local and costs nothing.
    if ctx.clean {
        clean::clean_doc(&ctx.data, doc, progress)?;
    } else if ctx.data.join("edits").join(doc).is_dir() {
        clean::apply_edits(&ctx.data, doc, progress)?;
    }

    // 3. read pages (cleaned where the cleanup pass ran, raw elsewhere)
    let mut pages = read_pages(&ctx.data, doc)?;
    if let Some(n) = limit {
        pages.truncate(n);
    }

    // 4. chunk: page-bounded sliding windows in reading order
    let mut chunks: Vec<(ChunkKey, Vec<Word>)> = Vec::new();
    for page in &pages {
        let mut idx = 0u32;
        let mut start = 0usize;
        while start < page.words.len() {
            let end = (start + CHUNK_WORDS).min(page.words.len());
            chunks.push((
                ChunkKey { doc: doc.to_string(), page: page.page, idx },
                page.words[start..end].to_vec(),
            ));
            if end == page.words.len() {
                break;
            }
            start += CHUNK_STRIDE;
            idx += 1;
        }
    }

    // 5. embed (ese: compile-time static embeddings, no model to load),
    // batched so progress stays visible
    let mut embs: Vec<Emb> = Vec::with_capacity(chunks.len());
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch
            .iter()
            .map(|(_, words)| words.iter().map(|w| w.t.as_str()).collect::<Vec<_>>().join(" "))
            .collect();
        embs.extend(ese::encode(&texts));
        progress(Progress::Embed { done: embs.len(), total: chunks.len() });
    }

    Ok(chunks
        .into_iter()
        .zip(embs)
        .map(|((key, words), emb)| ChunkRec { key, words, emb })
        .collect())
}

/// Atomic swap: upsert the doc's new chunks, remove keys that vanished,
/// checkpoint. The table retracts replaced records itself and byte-equal
/// upserts skip the graph, so an unchanged chunk costs one point read.
/// The only text-pipeline step that needs exclusive store access.
/// Returns (removed, added) — removed counts keys actually deleted.
pub fn commit_text(st: &mut Library, doc: &str, recs: &[ChunkRec]) -> (usize, usize) {
    let counts = st.wtx(|tx| {
        let old: Vec<ChunkKey> =
            tx.rtx(|(_, ((_, manifest), _))| manifest.search(&doc.to_string()));
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

/// Crop `bbox` out of the page render, keeping it only if it contains ink
/// (scans are full of legitimately blank gaps).
fn crop_if_inked(page: &image::DynamicImage, bbox: Bbox) -> Option<image::DynamicImage> {
    let (iw, ih) = (page.width() as f32, page.height() as f32);
    let crop = page.crop_imm(
        (bbox[0] * iw) as u32,
        (bbox[1] * ih) as u32,
        (bbox[2] * iw).max(1.0) as u32,
        (bbox[3] * ih).max(1.0) as u32,
    );
    let small = crop.thumbnail(96, 96).into_luma8();
    let dark = small.pixels().filter(|p| p.0[0] < 160).count();
    let frac = dark as f64 / (small.width() * small.height()).max(1) as f64;
    (frac >= FIG_MIN_INK).then_some(crop)
}

/// Detect and CLIP-embed a doc's figure regions from its cached OCR + page
/// renders. Touches no store. Loads the CLIP image encoder for the duration
/// of the call and drops it after (it's ~350MB resident).
pub fn prepare_figures(ctx: &IngestCtx, doc: &str, progress: ProgressFn) -> Result<Vec<ImageRec>> {
    let pages = read_ocr(&ctx.data.join("ocr").join(doc))?;
    let pages_dir = ctx.data.join("pages").join(doc);
    let model = layout::LayoutModel::load(&ctx.data)?;

    // 1. detect + crop
    let mut keys: Vec<(ImageKey, Bbox)> = Vec::new();
    let mut crops: Vec<image::DynamicImage> = Vec::new();
    for (pi, page) in pages.iter().enumerate() {
        progress(Progress::Figures { done: pi, total: pages.len() });
        let jpg = pages_dir.join(format!("page-{:04}.jpg", page.page));
        let (img, regions): (image::DynamicImage, Vec<Bbox>) = match &model {
            Some(m) => {
                let Ok(img) = image::open(&jpg) else { continue };
                let mut dets: Vec<layout::Detection> = match m.detect(&img) {
                    Ok(d) => d,
                    Err(e) => {
                        progress(Progress::Log(format!("layout failed on p.{}: {e:#}", page.page)));
                        continue;
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
                    continue;
                }
                let Ok(img) = image::open(&jpg) else { continue };
                (img, regions)
            }
        };
        // whole figures AND their component parts get indexed; the server
        // groups per page at query time so parts don't spam results
        let mut with_parts = regions.clone();
        for r in &regions {
            with_parts.extend(subdivide::subdivide(&img, *r));
        }
        let mut regions = with_parts;
        // stable idx assignment in reading order
        regions.sort_by(|a, b| (a[1], a[0]).partial_cmp(&(b[1], b[0])).unwrap());
        let mut idx = 0u32;
        for bbox in regions {
            if let Some(crop) = crop_if_inked(&img, bbox) {
                keys.push((ImageKey { doc: doc.to_string(), page: page.page, idx }, bbox));
                crops.push(crop);
                idx += 1;
            }
        }
    }
    progress(Progress::Figures { done: pages.len(), total: pages.len() });

    // 2. embed
    let model = ImageEmbedding::try_new(
        ImageInitOptions::new(ImageEmbeddingModel::ClipVitB32)
            .with_cache_dir(ctx.data.join("models"))
            .with_show_download_progress(true),
    )?;
    let mut recs: Vec<ImageRec> = Vec::with_capacity(keys.len());
    let mut it = keys.into_iter();
    for batch in crops.chunks(CLIP_BATCH) {
        for e in model.embed_images(batch.to_vec())? {
            let (key, bbox) = it.next().unwrap();
            let emb: ClipEmb = e.try_into().expect("CLIP emits 512-dim vectors");
            recs.push(ImageRec { key, bbox, emb });
        }
        progress(Progress::Clip { done: recs.len(), total: crops.len() });
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
