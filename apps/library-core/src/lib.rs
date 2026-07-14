//! Shared types, the fold graph, and hybrid search for The Library.

use anny::metric::Cosine;
use fold::pipeline::terminal::search::{Bm25, Bm25Reader, Hnsw, HnswReader};
use fold::pipeline::terminal::{InvertedIndex, InvertedIndexReader};
use fold::pipeline::{FlatMap, Keyed, Map, Push};
use fold::stream::{KeyedStream, PipelineInitCtx, Readable, WriteTx};
use fxhash::FxHashMap;
pub use fxhash::FxHashSet;
use serde::{Deserialize, Serialize};

pub mod wire;

/// Text embeddings come from ese's compile-time static model; the dimension
/// follows its `dim-*` cargo feature. NOTE: with `dim-512` this equals
/// CLIP_DIM, so the type system no longer catches a text/CLIP embedding
/// mix-up — keep the two paths visibly separate.
pub const EMB_DIM: usize = ese::DIMENSIONS;
pub type Emb = [f32; EMB_DIM];

/// CLIP ViT-B/32 shared text/image space.
pub const CLIP_DIM: usize = 512;
pub type ClipEmb = [f32; CLIP_DIM];

/// Stable identity of one chunk: a contiguous run of words on one page.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChunkKey {
    pub doc: String,
    pub page: u32,
    pub idx: u32,
}

/// One OCR'd word with its normalized bounding box (top-left origin, 0..1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub t: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// serde for f32 arrays past serde's 32-element impls: out as a slice, back
/// through a Vec (the same shape fold's Hnsw sink persists).
mod f32_array {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[f32; N], s: S) -> Result<S::Ok, S::Error> {
        v[..].serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[f32; N], D::Error> {
        let v = Vec::<f32>::deserialize(d)?;
        v.try_into()
            .map_err(|v: Vec<f32>| serde::de::Error::custom(format!("expected {N} floats, got {}", v.len())))
    }
}

/// The record stored under a [`ChunkKey`] in the library's primary-key
/// table and pushed through the fold graph as `Keyed<ChunkKey, ChunkRec>`.
/// The [`KeyedStream`] retracts the stored copy on upsert/remove, so
/// records never need to be reconstructed to delete them.
#[derive(Clone, Serialize, Deserialize)]
pub struct ChunkRec {
    pub key: ChunkKey,
    pub words: Vec<Word>,
    #[serde(with = "f32_array")]
    pub emb: Emb,
}

impl ChunkRec {
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.words.len() * 6);
        for w in &self.words {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&w.t);
        }
        s
    }
}

pub fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace()
        .filter_map(|t| {
            let t: String = t
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();
            (t.len() > 1).then_some(t)
        })
        .collect()
}

/// [`tokenize`] in fold's Bm25 buffer convention (`\0`-terminated tokens).
/// The Bm25 sink MUST tokenize exactly like [`tokenize`]: TermDict's
/// completion terms come from it, and prefix-expanded query terms only match
/// postings produced the same way. fold's default tokenizer is ASCII-only
/// and keeps 1-char tokens, so it would silently disagree.
pub fn lex_tokenize(text: &str, tokens: &mut Vec<u8>) {
    tokens.clear();
    for t in tokenize(text) {
        tokens.extend_from_slice(t.as_bytes());
        tokens.push(0);
    }
}

// ---------------------------------------------------------------------------
// TermDict: a fold terminal holding every live term under raw UTF-8 keys, so
// the trailing (partial) token of a query can be expanded with a prefix scan.
// Bm25 can't do this: postcard string keys are length-prefixed.
// ---------------------------------------------------------------------------

pub struct TermDict {
    name: String,
    ks: Option<fjall::SingleWriterTxKeyspace>,
    pending: FxHashMap<Vec<u8>, i64>,
}

impl TermDict {
    pub fn new(name: impl Into<String>) -> Self {
        TermDict {
            name: name.into(),
            ks: None,
            pending: FxHashMap::default(),
        }
    }
}

pub struct TermDictReader<'tx, R: Readable> {
    tx: &'tx R,
    ks: fjall::SingleWriterTxKeyspace,
}

impl<R: Readable> TermDictReader<'_, R> {
    /// Up to `k` live terms starting with `prefix`, lexicographic.
    pub fn complete(&self, prefix: &str, k: usize) -> Vec<String> {
        self.tx
            .prefix(&self.ks, prefix.as_bytes())
            .take(k)
            .map(|kv| String::from_utf8(kv.key().unwrap().to_vec()).unwrap())
            .collect()
    }
}

impl Push<String> for TermDict {
    type Reader<'tx, R: Readable + 'tx> = TermDictReader<'tx, R>;

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.ks = Some(init.keyspace(&self.name));
    }

    fn push(&mut self, _tx: &mut WriteTx<'_>, data: &String, delta: isize) {
        *self.pending.entry(data.as_bytes().to_vec()).or_insert(0) += delta as i64;
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        let ks = self.ks.clone().unwrap();
        for (key, delta) in self.pending.drain() {
            if delta == 0 {
                continue;
            }
            let cur = tx
                .get(&ks, &key)
                .map(|v| i64::from_be_bytes(v.as_ref().try_into().unwrap()))
                .unwrap_or(0);
            let new = cur + delta;
            if new > 0 {
                tx.insert(&ks, &key, new.to_be_bytes());
            } else {
                tx.remove(&ks, &key);
            }
        }
    }

    fn abort(&mut self) {
        self.pending.clear();
    }

    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        TermDictReader {
            tx,
            ks: self.ks.clone().unwrap(),
        }
    }
}

// ---------------------------------------------------------------------------
// The graph. fn pointers (not closures) keep every node's type nameable, so
// the whole graph and its reader tuple get type aliases.
// ---------------------------------------------------------------------------

// M_0=32, K=40 results, EF_SEARCH=80, EF_BUILD=80, MAX_LEVEL=16.
// K/EF_SEARCH are search-time only: bumping them doesn't invalidate stored
// graphs (the persisted blob validates DIM/M_0/MAX_LEVEL).
pub type VecIndex = Hnsw<ChunkKey, f32, Cosine, EMB_DIM, 32, 40, 80, 80, 16>;

/// The Bm25 sink's tokenizer type: a plain fn pointer keeps it nameable.
pub type LexTok = fn(&str, &mut Vec<u8>);

/// What the [`KeyedStream`] table feeds the graph: the primary key plus the
/// stored record.
pub type ChunkIn = Keyed<ChunkKey, ChunkRec>;

pub type LexSink = Map<
    fn(&ChunkIn) -> Keyed<ChunkKey, String>,
    Bm25<ChunkKey, String>,
    ChunkIn,
    Keyed<ChunkKey, String>,
>;
pub type VecSink =
    Map<fn(&ChunkIn) -> Keyed<ChunkKey, Emb>, VecIndex, ChunkIn, Keyed<ChunkKey, Emb>>;
pub type ManifestSink = Map<
    fn(&ChunkIn) -> Keyed<ChunkKey, String>,
    InvertedIndex<ChunkKey, String>,
    ChunkIn,
    Keyed<ChunkKey, String>,
>;
pub type TermSink = FlatMap<fn(&ChunkIn) -> Vec<String>, TermDict, ChunkIn, String>;

pub type Graph = ((LexSink, VecSink), (ManifestSink, TermSink));
pub type Library = KeyedStream<ChunkKey, ChunkRec, Graph>;

pub type Readers<'tx, R> = (
    (
        Bm25Reader<'tx, R, ChunkKey, LexTok>,
        HnswReader<'tx, R, ChunkKey, f32, Cosine, EMB_DIM, 32, 40, 80, 80, 16>,
    ),
    (InvertedIndexReader<'tx, R, ChunkKey, String>, TermDictReader<'tx, R>),
);

pub fn graph() -> Graph {
    fn to_lex(c: &ChunkIn) -> Keyed<ChunkKey, String> {
        Keyed::new(c.key.clone(), c.val.text())
    }
    fn to_vec(c: &ChunkIn) -> Keyed<ChunkKey, Emb> {
        Keyed::new(c.key.clone(), c.val.emb)
    }
    fn to_manifest(c: &ChunkIn) -> Keyed<ChunkKey, String> {
        Keyed::new(c.key.clone(), c.key.doc.clone())
    }
    fn to_terms(c: &ChunkIn) -> Vec<String> {
        tokenize(&c.val.text())
    }

    (
        (
            Map::new(
                to_lex as fn(&ChunkIn) -> Keyed<ChunkKey, String>,
                Bm25::with_tokenizer("lex", lex_tokenize as LexTok),
            ),
            Map::new(
                to_vec as fn(&ChunkIn) -> Keyed<ChunkKey, Emb>,
                VecIndex::new("vec", Cosine, 42),
            ),
        ),
        (
            Map::new(
                to_manifest as fn(&ChunkIn) -> Keyed<ChunkKey, String>,
                InvertedIndex::new("manifest"),
            ),
            FlatMap::new(to_terms as fn(&ChunkIn) -> Vec<String>, TermDict::new("terms")),
        ),
    )
}

pub fn open(path: impl AsRef<std::path::Path>) -> Library {
    KeyedStream::new(path, graph())
}

/// Fallible [`open`]: `Err(fjall::Error::Locked)` means another process
/// holds the store.
pub fn try_open(path: impl AsRef<std::path::Path>) -> Result<Library, fjall::Error> {
    KeyedStream::try_new(path, graph())
}

// ---------------------------------------------------------------------------
// Hybrid search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub score: f32,
    pub key: ChunkKey,
    pub words: Vec<Word>,
}

/// Reciprocal rank fusion: score(k) = sum over lists of 1/(60 + rank).
fn rrf(lists: &[Vec<ChunkKey>]) -> Vec<(f32, ChunkKey)> {
    let mut scores: FxHashMap<&ChunkKey, f32> = FxHashMap::default();
    for list in lists {
        for (rank, key) in list.iter().enumerate() {
            *scores.entry(key).or_insert(0.0) += 1.0 / (60.0 + rank as f32);
        }
    }
    let mut out: Vec<(f32, ChunkKey)> =
        scores.into_iter().map(|(k, s)| (s, k.clone())).collect();
    out.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out
}

/// Lexical (with trailing-prefix expansion) + optional semantic, RRF-fused,
/// metadata resolved via `resolve` (see below) — all under the one snapshot
/// `r` was taken from. `filter`, when set, restricts every ranker to the
/// given doc ids *inside* the search (filtering after truncation would
/// starve results).
///
/// `resolve` fetches a chunk's words given its key; callers should back it
/// with [`Library::get`] (a cheap primary-table point-read) rather than the
/// `meta` sink's reverse index — `meta` stores each chunk's full `Vec<Word>`
/// as part of its fjall *key* (needed to answer "what key maps to this
/// value"), so looking words up through it means every hit pays for
/// comparing against huge keys. `Library::get` reads the same words back out
/// of a value instead, which is what point-reads are fast at.
pub fn search<R: Readable>(
    r: &Readers<'_, R>,
    query: &str,
    qemb: Option<&Emb>,
    k: usize,
    filter: Option<&FxHashSet<String>>,
    resolve: impl Fn(&ChunkKey) -> Option<Vec<Word>>,
) -> Vec<Hit> {
    let ((lex, vec), (_, terms)) = r;

    let t = std::time::Instant::now();
    let mut toks = tokenize(query);
    if let Some(last) = toks.last().cloned() {
        for t in terms.complete(&last, 5) {
            if !toks.contains(&t) {
                toks.push(t);
            }
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("[perf] term_complete elapsed={}us", t.elapsed().as_micros());
    }
    if toks.is_empty() {
        return Vec::new();
    }

    // the expanded tokens are already normalized, so re-tokenizing the
    // joined query inside Bm25 is a no-op
    let expanded = toks.join(" ");

    // give RRF headroom beyond the final k
    let fetch = k.max(64);
    let t = std::time::Instant::now();
    let lexical: Vec<ChunkKey> = match filter {
        Some(f) => lex
            .search_filtered(&expanded, fetch, |key: &ChunkKey| f.contains(&key.doc))
            .into_iter()
            .map(|h| h.val)
            .collect(),
        None => lex.search(&expanded, fetch).into_iter().map(|h| h.val).collect(),
    };
    if cfg!(debug_assertions) {
        eprintln!("[perf] lex_search elapsed={}us hits={}", t.elapsed().as_micros(), lexical.len());
    }
    let t = std::time::Instant::now();
    let semantic: Vec<ChunkKey> = match (qemb, filter) {
        (Some(e), Some(f)) => vec
            .search_filtered(e, |key: &ChunkKey| f.contains(&key.doc))
            .into_iter()
            .map(|h| h.val)
            .collect(),
        (Some(e), None) => vec.search(e).into_iter().map(|h| h.val).collect(),
        (None, _) => Vec::new(),
    };
    if cfg!(debug_assertions) {
        eprintln!("[perf] vec_search elapsed={}us hits={}", t.elapsed().as_micros(), semantic.len());
    }

    let t = std::time::Instant::now();
    let hits: Vec<Hit> = rrf(&[lexical, semantic])
        .into_iter()
        .take(k)
        .filter_map(|(score, key)| {
            let words = resolve(&key)?;
            Some(Hit { score, key, words })
        })
        .collect();
    if cfg!(debug_assertions) {
        eprintln!("[perf] rrf_and_resolve elapsed={}us hits={}", t.elapsed().as_micros(), hits.len());
    }
    hits
}

// ---------------------------------------------------------------------------
// Image store: a second stream (images.db) over CLIP embeddings of figure
// regions detected on the page scans. Same shape as the text graph, minus
// the lexical sinks: HNSW for search, meta for key->bbox, manifest for
// doc->keys (diff-based re-ingest).
// ---------------------------------------------------------------------------

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

pub type ImgVecIndex = Hnsw<ImageKey, f32, Cosine, CLIP_DIM, 32, 40, 80, 80, 16>;

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
    HnswReader<'tx, R, ImageKey, f32, Cosine, CLIP_DIM, 32, 40, 80, 80, 16>,
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
    let found = match filter {
        Some(f) => vec.search_filtered(qemb, |key: &ImageKey| f.contains(&key.doc)),
        None => vec.search(qemb),
    };
    found
        .into_iter()
        .take(k)
        .filter_map(|hit| {
            let bbox = meta.search(&hit.val).into_iter().next()?;
            // cosine distance -> similarity, so higher is better like text
            Some(ImageHit { score: 1.0 - hit.score, key: hit.val, bbox })
        })
        .collect()
}

