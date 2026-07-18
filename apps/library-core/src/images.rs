//! Image store: a second stream (images.db) over CLIP embeddings of figure
//! regions detected on the page scans. Same shape as the text graph, minus
//! the lexical sinks: HNSW for search, meta for key->bbox, manifest for
//! doc->keys (diff-based re-ingest).

use anny::metric::Cosine;
use fold::pipeline::terminal::search::{Hnsw, HnswReader};
use fold::pipeline::terminal::{InvertedIndex, InvertedIndexReader};
use fold::pipeline::{Keyed, Map};
use fold::stream::{KeyedStream, Readable};
use serde::{Deserialize, Serialize};

use crate::records::f32_array;
use crate::{CLIP_DIM, ClipEmb, FxHashSet};

/// Image analogue of [`MIN_REL`], but normalized differently: raw CLIP
/// text→image cosines cluster tightly (measured: top ≈ 0.30–0.33 with even
/// the 256th neighbor at 0.85·top — the index always returns *nearest*
/// figures, relevant or not), so a plain ratio to the top barely
/// discriminates. Instead each figure is measured on the spread between the
/// query's best figure and the fetch-depth noise floor (the IMG_FETCH-th
/// sim): keep figures in the upper `IMG_MIN_REL` fraction of that spread.
/// See the `[perf] image_search … sims` debug line for real distributions
/// (measured on this corpus: 0.5 keeps ~12–34 figures and dries up by page
/// 3; 0.35 keeps ~43–67 and sustains figures through the first several
/// pages; 0.2 admits a visibly weak tail).
pub const IMG_MIN_REL: f32 = 0.35;

/// Stable identity of one figure region on one page.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ImageKey {
    pub doc: String,
    pub page: u32,
    pub idx: u32,
}

/// Normalized [x, y, w, h], top-left origin, 0..1.
pub type Bbox = [f32; 4];

/// The record stored under an [`ImageKey`] in the image store's primary-key
/// table and pushed through the graph as `Keyed<ImageKey, ImageRec>`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ImageRec {
    pub key: ImageKey,
    pub bbox: Bbox,
    #[serde(with = "f32_array")]
    pub emb: ClipEmb,
}

/// How deep the CLIP index fetches per query. Like `LEX_FETCH` this is a
/// pinned depth so paginated slices tile deterministically — it's the
/// compile-time TOP_K below (search-time only; stored graphs stay valid).
pub const IMG_FETCH: usize = 256;

pub type ImgVecIndex = Hnsw<ImageKey, f32, Cosine, CLIP_DIM, 32, 256, 256, 80, 16>;

/// What the image store's table feeds the graph.
pub type ImageIn = Keyed<ImageKey, ImageRec>;

pub type ImgVecSink =
    Map<fn(&ImageIn) -> Keyed<ImageKey, ClipEmb>, ImgVecIndex, ImageIn, Keyed<ImageKey, ClipEmb>>;
pub type ImgMetaSink = Map<
    fn(&ImageIn) -> Keyed<Bbox, ImageKey>,
    InvertedIndex<Bbox, ImageKey>,
    ImageIn,
    Keyed<Bbox, ImageKey>,
>;
pub type ImgManifestSink = Map<
    fn(&ImageIn) -> Keyed<ImageKey, String>,
    InvertedIndex<ImageKey, String>,
    ImageIn,
    Keyed<ImageKey, String>,
>;

pub type ImgGraph = (ImgVecSink, (ImgMetaSink, ImgManifestSink));
pub type Images = KeyedStream<ImageKey, ImageRec, ImgGraph>;

pub type ImgReaders<'tx, R> = (
    HnswReader<'tx, R, ImageKey, f32, Cosine, CLIP_DIM, 32, 256, 256, 80, 16>,
    (
        InvertedIndexReader<'tx, R, Bbox, ImageKey>,
        InvertedIndexReader<'tx, R, ImageKey, String>,
    ),
);

pub fn img_graph() -> ImgGraph {
    fn to_vec(r: &ImageIn) -> Keyed<ImageKey, ClipEmb> {
        Keyed::new(r.key.clone(), r.val.emb)
    }
    fn to_meta(r: &ImageIn) -> Keyed<Bbox, ImageKey> {
        Keyed::new(r.val.bbox, r.key.clone())
    }
    fn to_manifest(r: &ImageIn) -> Keyed<ImageKey, String> {
        Keyed::new(r.key.clone(), r.key.doc.clone())
    }

    (
        Map::new(
            to_vec as fn(&ImageIn) -> Keyed<ImageKey, ClipEmb>,
            ImgVecIndex::new("imgvec", Cosine, 42),
        ),
        (
            Map::new(
                to_meta as fn(&ImageIn) -> Keyed<Bbox, ImageKey>,
                InvertedIndex::new("imgmeta"),
            ),
            Map::new(
                to_manifest as fn(&ImageIn) -> Keyed<ImageKey, String>,
                InvertedIndex::new("imgmanifest"),
            ),
        ),
    )
}

pub fn open_images(path: impl AsRef<std::path::Path>) -> Images {
    KeyedStream::new(path, img_graph())
}

/// Fallible [`open_images`]; see [`try_open`].
pub fn try_open_images(path: impl AsRef<std::path::Path>) -> Result<Images, fjall::Error> {
    KeyedStream::try_new(path, img_graph())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageHit {
    pub score: f32,
    pub key: ImageKey,
    pub bbox: Bbox,
}

/// Nearest figure regions to a CLIP query embedding (usually from the text
/// encoder — the shared space is what makes that legal).
pub fn image_search<R: Readable>(
    r: &ImgReaders<'_, R>,
    qemb: &ClipEmb,
    k: usize,
    filter: Option<&FxHashSet<String>>,
) -> Vec<ImageHit> {
    let (vec, (meta, _)) = r;
    let t = std::time::Instant::now();
    let found = match filter {
        Some(f) => vec.search_filtered(qemb, |key: &ImageKey| f.contains(&key.doc)),
        None => vec.search(qemb),
    };
    let hits: Vec<ImageHit> = found
        .into_iter()
        .take(k)
        .filter_map(|hit| {
            let bbox = meta.search(&hit.val).into_iter().next()?;
            // cosine distance -> similarity, so higher is better like text
            Some(ImageHit {
                score: 1.0 - hit.score,
                key: hit.val,
                bbox,
            })
        })
        .collect();
    if cfg!(debug_assertions) {
        let (top, last) = (
            hits.first().map(|h| h.score).unwrap_or(0.0),
            hits.last().map(|h| h.score).unwrap_or(0.0),
        );
        // how many figures survive the spread cutoff at various strengths —
        // the data behind the IMG_MIN_REL choice
        let above = |f: f32| {
            hits.iter()
                .filter(|h| h.score >= last + f * (top - last))
                .count()
        };
        eprintln!(
            "[perf] image_search elapsed={}us n={} sims top={top:.3} floor={last:.3} above .2/.35/.5/.65={}/{}/{}/{}",
            t.elapsed().as_micros(),
            hits.len(),
            above(0.2),
            above(0.35),
            above(0.5),
            above(0.65),
        );
    }
    hits
}
