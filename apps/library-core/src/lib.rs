//! Shared types, the fold graph, and hybrid search for The Library.

use anny::metric::Cosine;
use fold::pipeline::terminal::search::{Bm25, Bm25Reader, Hnsw, HnswReader};
use fold::pipeline::terminal::{InvertedIndex, InvertedIndexReader};
use fold::pipeline::{FlatMap, Keyed, Map, Push, Scored};
use fold::stream::{KeyedStream, PipelineInitCtx, Readable, WriteTx};
use fxhash::FxHashMap;
pub use fxhash::FxHashSet;
use serde::{Deserialize, Serialize};

pub mod legibility;
pub mod perf;
pub mod search_api;
pub mod tools;
pub mod wire;

pub use search_api::{K, K_DOC, Query, answer};

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

    /// Up to `k` live terms starting with `prefix`, ranked by corpus
    /// frequency (descending), ties broken lexicographically for
    /// determinism. Unlike [`complete`](Self::complete) — which takes the
    /// first `k` lexicographically and is right for query-time term
    /// expansion — this is for user-facing type-ahead, where the most common
    /// completions are what a human wants to see first. Scans at most
    /// `SCAN_CAP` matching terms so a 1-char prefix can't walk the whole
    /// keyspace.
    pub fn complete_ranked(&self, prefix: &str, k: usize) -> Vec<String> {
        const SCAN_CAP: usize = 2000;
        let mut cands: Vec<(i64, String)> = self
            .tx
            .prefix(&self.ks, prefix.as_bytes())
            .take(SCAN_CAP)
            .map(|kv| {
                let (key, val) = kv.into_inner().unwrap();
                let freq = i64::from_be_bytes(val.as_ref().try_into().unwrap());
                (freq, String::from_utf8(key.to_vec()).unwrap())
            })
            .collect();
        cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        cands.into_iter().take(k).map(|(_, t)| t).collect()
    }

    /// Whether `term` is a live vocabulary term (exact match). Used to decide
    /// if a query token needs fuzzy correction.
    pub fn contains(&self, term: &str) -> bool {
        self.tx.get(&self.ks, term.as_bytes()).unwrap().is_some()
    }

    /// Up to `k` live terms within edit distance [`MAX_FUZZ_DIST`] of `token`,
    /// nearest first (ties broken by higher corpus frequency). Scans only the
    /// term-dict bucket sharing `token`'s first two characters — typos and OCR
    /// errors overwhelmingly preserve leading characters — so it never walks
    /// the whole vocabulary. This is the fuzzy-correction primitive: an unknown
    /// query word is replaced by its nearest real words, which then feed the
    /// exact lexical index (no document-scale fuzzy index needed). Limitation:
    /// a corruption in the first two characters is not recovered.
    pub fn correct(&self, token: &str, k: usize) -> Vec<String> {
        const SCAN_CAP: usize = 4000;
        let prefix: String = token.chars().take(2).collect();
        let mut cands: Vec<(usize, i64, String)> = self
            .tx
            .prefix(&self.ks, prefix.as_bytes())
            .take(SCAN_CAP)
            .filter_map(|kv| {
                let (key, val) = kv.into_inner().unwrap();
                let term = String::from_utf8(key.to_vec()).ok()?;
                if term == token {
                    return None; // an exact match isn't a correction
                }
                let d = levenshtein(token, &term, MAX_FUZZ_DIST);
                (d <= MAX_FUZZ_DIST).then(|| {
                    let freq = i64::from_be_bytes(val.as_ref().try_into().unwrap());
                    (d, freq, term)
                })
            })
            .collect();
        cands.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        cands.into_iter().take(k).map(|(_, _, t)| t).collect()
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
    /// BM25 score relative to the query's best lexical hit (1.0 = top).
    /// Keys with no lexical evidence (semantic-only) get 1.0 — the semantic
    /// list is count-bounded upstream, so it never carries a noise tail.
    #[serde(default)]
    pub rel: f32,
    /// Raw BM25 score (0.0 for semantic-only hits). Unlike `rel`, this is
    /// an absolute signal: a weak query's top hit has a *low* raw score but
    /// still rel = 1.0, so agents gating on "did we really find anything?"
    /// must look here.
    #[serde(default)]
    pub bm25: f32,
    /// 0-based rank in the lexical (BM25) list; `None` = semantic-only —
    /// exactly the hits that take the `rel = 1.0` default above and so
    /// bypass the [`MIN_REL`] cutoff.
    #[serde(default)]
    pub lex_rank: Option<u32>,
    /// 0-based rank in the semantic (HNSW) list; `None` = lexical-only.
    #[serde(default)]
    pub sem_rank: Option<u32>,
    /// Cosine distance from the vector index (lower = closer).
    #[serde(default)]
    pub sem_dist: Option<f32>,
    pub key: ChunkKey,
    pub words: Vec<Word>,
}

/// Pre-fusion ranker list sizes, reported through the `stats` out-param of
/// [`search`] — the fused hit list alone can't reconstruct them.
#[derive(Debug, Clone, Copy, Default)]
pub struct RankerStats {
    pub lex_n: usize,
    pub sem_n: usize,
}

/// Hits scoring below this fraction of the query's top BM25 hit are noise;
/// the paginated result stream ends here. Tuning knob — the dev `[perf]`
/// logging in `search` is the place to eyeball rel distributions.
pub const MIN_REL: f32 = 0.25;

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

/// How deep to fetch from the lexical ranker regardless of `k`. Pinning the
/// depth pins the lexical list — and therefore the RRF input and the final
/// order — so paginated slices of the same query tile without drift (a
/// growing fetch would add RRF terms to dual-membership keys and shift
/// ranks between page requests). Also caps stable pagination depth at
/// ~LEX_FETCH lexical + TOP_K semantic hits. BM25 cost is limit-independent
/// (full postings scan, truncate at end), so the extra depth is nearly free.
pub const LEX_FETCH: usize = 512;

/// Max edit distance for fuzzy term correction ([`TermDictReader::correct`]).
const MAX_FUZZ_DIST: usize = 2;
/// Nearest real terms substituted per unknown query word.
const FUZZ_CANDIDATES: usize = 3;

/// Levenshtein edit distance, capped at `max`: returns `max + 1` as soon as it
/// is certain the true distance exceeds `max` (callers only care about the
/// `<= max` band), keeping each comparison cheap.
fn levenshtein(a: &str, b: &str, max: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > max {
        return max + 1;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut cur = vec![0usize; lb + 1];
    for i in 1..=la {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=lb {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return max + 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[lb]
}

/// Cosine similarity of two embeddings (0 if either is degenerate).
fn cosine(a: &Emb, b: &Emb) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..EMB_DIM {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// How many top fused hits the MMR diversity re-rank considers. Fixed
/// (independent of `k`/`offset`) so greedy selection is deterministic and
/// pagination stays prefix-stable; hits past the pool keep their fused order.
const MMR_POOL: usize = 100;
/// MMR relevance/diversity mix: `score = λ·relevance − (1−λ)·max_similarity`.
/// 1.0 = pure relevance (today); lower demotes near-duplicates harder.
const MMR_LAMBDA: f32 = 0.7;

/// Update each unpicked pool item's running max similarity against one newly
/// selected item — the trick that keeps [`mmr_rerank`] O(pool²) overall.
fn bump_sim(max_sim: &mut [f32], picked: &[bool], embs: &[Option<Emb>], sel: usize) {
    let Some(se) = &embs[sel] else { return };
    for (i, mi) in max_sim.iter_mut().enumerate() {
        if !picked[i] {
            if let Some(e) = &embs[i] {
                *mi = mi.max(cosine(e, se));
            }
        }
    }
}

/// Maximal Marginal Relevance re-rank of the fused list: greedily reorders the
/// top [`MMR_POOL`] hits to demote near-duplicates (same book/edition — common
/// in scanned corpora), then appends the remainder in fused order. Similarity
/// is cosine between chunk embeddings fetched via `resolve`. Deterministic over
/// the (pinned) fused list, so paginated slices still tile.
fn mmr_rerank(
    fused: Vec<(f32, ChunkKey)>,
    resolve: &impl Fn(&ChunkKey) -> Option<ChunkRec>,
) -> Vec<(f32, ChunkKey)> {
    let pool_n = fused.len().min(MMR_POOL);
    if pool_n <= 1 {
        return fused;
    }
    let embs: Vec<Option<Emb>> =
        fused[..pool_n].iter().map(|(_, k)| resolve(k).map(|r| r.emb)).collect();
    let top = fused[0].0;
    let norm = |s: f32| if top > 0.0 { s / top } else { 1.0 };

    let mut max_sim = vec![0.0f32; pool_n];
    let mut picked = vec![false; pool_n];
    let mut order = Vec::with_capacity(pool_n);
    // seed with the most relevant (fused is already best-first)
    picked[0] = true;
    order.push(0usize);
    bump_sim(&mut max_sim, &picked, &embs, 0);
    for _ in 1..pool_n {
        let mut best = usize::MAX;
        let mut best_score = f32::NEG_INFINITY;
        for i in 0..pool_n {
            if picked[i] {
                continue;
            }
            let score = MMR_LAMBDA * norm(fused[i].0) - (1.0 - MMR_LAMBDA) * max_sim[i];
            if score > best_score {
                best_score = score;
                best = i;
            }
        }
        picked[best] = true;
        order.push(best);
        bump_sim(&mut max_sim, &picked, &embs, best);
    }

    let mut out: Vec<(f32, ChunkKey)> = order.into_iter().map(|i| fused[i].clone()).collect();
    out.extend_from_slice(&fused[pool_n..]);
    out
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

/// Lexical + optional semantic, RRF-fused, metadata resolved via `resolve`
/// (see below) — all under the one snapshot `r` was taken from. `filter`,
/// when set, restricts every ranker to the given doc ids *inside* the
/// search (filtering after truncation would starve results).
///
/// `complete` expands the trailing token via the term dictionary — right
/// for type-ahead (a human mid-word), wrong for programmatic callers whose
/// queries are complete words ("micro" must not match "microscope").
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
    complete: bool,
    fuzzy: bool,
    diversify: bool,
    resolve: impl Fn(&ChunkKey) -> Option<ChunkRec>,
    mut stats: Option<&mut RankerStats>,
) -> Vec<Hit> {
    let ((lex, vec), (_, terms)) = r;

    let t = std::time::Instant::now();
    let orig = tokenize(query);
    let mut toks = orig.clone();
    if complete && let Some(last) = toks.last().cloned() {
        for t in terms.complete(&last, 5) {
            if !toks.contains(&t) {
                toks.push(t);
            }
        }
    }
    // fuzzy correction (full queries only): replace each unknown query word
    // with its nearest real vocabulary words, which then feed the exact
    // lexical index. Exact words expand nothing, so clean queries are
    // unchanged. Bounded to FUZZ_CANDIDATES per token.
    if fuzzy {
        for tok in &orig {
            if terms.contains(tok) {
                continue;
            }
            for t in terms.correct(tok, FUZZ_CANDIDATES) {
                if !toks.contains(&t) {
                    toks.push(t);
                }
            }
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("[perf] term_expand elapsed={}us", t.elapsed().as_micros());
    }
    if toks.is_empty() {
        return Vec::new();
    }

    // the expanded tokens are already normalized, so re-tokenizing the
    // joined query inside Bm25 is a no-op
    let expanded = toks.join(" ");

    // give RRF headroom beyond the final k (and keep the list pinned — see LEX_FETCH)
    let fetch = k.max(LEX_FETCH);
    let t = std::time::Instant::now();
    let scored = match filter {
        Some(f) => {
            lex.search_filtered(&expanded, fetch, |key: &ChunkKey| f.contains(&key.doc))
        }
        None => lex.search(&expanded, fetch),
    };
    let top = scored.first().map(|h| h.score).unwrap_or(0.0);
    let rel: FxHashMap<ChunkKey, (f32, f32)> = scored
        .iter()
        .map(|h| {
            let r = if top > 0.0 { (h.score / top) as f32 } else { 1.0 };
            (h.val.clone(), (r, h.score as f32))
        })
        .collect();
    let lexical: Vec<ChunkKey> = scored.into_iter().map(|h| h.val).collect();
    let lex_rank: FxHashMap<ChunkKey, u32> =
        lexical.iter().enumerate().map(|(i, k)| (k.clone(), i as u32)).collect();
    if cfg!(debug_assertions) {
        eprintln!("[perf] lex_search elapsed={}us hits={}", t.elapsed().as_micros(), lexical.len());
    }
    let t = std::time::Instant::now();
    let sem_scored: Vec<Scored<f32, ChunkKey>> = match (qemb, filter) {
        (Some(e), Some(f)) => vec.search_filtered(e, |key: &ChunkKey| f.contains(&key.doc)),
        (Some(e), None) => vec.search(e),
        (None, _) => Vec::new(),
    };
    let sem_rank: FxHashMap<ChunkKey, (u32, f32)> = sem_scored
        .iter()
        .enumerate()
        .map(|(i, h)| (h.val.clone(), (i as u32, h.score)))
        .collect();
    let semantic: Vec<ChunkKey> = sem_scored.into_iter().map(|h| h.val).collect();
    if cfg!(debug_assertions) {
        eprintln!("[perf] vec_search elapsed={}us hits={}", t.elapsed().as_micros(), semantic.len());
    }
    if let Some(s) = stats.as_deref_mut() {
        s.lex_n = lexical.len();
        s.sem_n = semantic.len();
    }

    let t = std::time::Instant::now();
    let fused = rrf(&[lexical, semantic]);
    // diversity: demote near-duplicates (same book/edition) among the top
    // hits. Full queries only — the per-keystroke path can't afford the
    // embedding reads, and doc-scoped browser-find must keep full coverage.
    let ordered = if diversify { mmr_rerank(fused, &resolve) } else { fused };
    let hits: Vec<Hit> = ordered
        .into_iter()
        .take(k)
        .filter_map(|(score, key)| {
            let rec = resolve(&key)?;
            let (r, bm25) = rel.get(&key).copied().unwrap_or((1.0, 0.0));
            let (sem_rank, sem_dist) = match sem_rank.get(&key) {
                Some(&(rank, dist)) => (Some(rank), Some(dist)),
                None => (None, None),
            };
            Some(Hit {
                score,
                rel: r,
                bm25,
                lex_rank: lex_rank.get(&key).copied(),
                sem_rank,
                sem_dist,
                key,
                words: rec.words,
            })
        })
        .collect();
    if cfg!(debug_assertions) {
        eprintln!("[perf] fuse_rerank_resolve elapsed={}us hits={}", t.elapsed().as_micros(), hits.len());
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
            Some(ImageHit { score: 1.0 - hit.score, key: hit.val, bbox })
        })
        .collect();
    if cfg!(debug_assertions) {
        let (top, last) = (
            hits.first().map(|h| h.score).unwrap_or(0.0),
            hits.last().map(|h| h.score).unwrap_or(0.0),
        );
        // how many figures survive the spread cutoff at various strengths —
        // the data behind the IMG_MIN_REL choice
        let above = |f: f32| hits.iter().filter(|h| h.score >= last + f * (top - last)).count();
        eprintln!(
            "[perf] image_search elapsed={}us n={} sims top={top:.3} floor={last:.3} above .2/.35/.5/.65={}/{}/{}/{}",
            t.elapsed().as_micros(),
            hits.len(),
            above(0.2), above(0.35), above(0.5), above(0.65),
        );
    }
    hits
}

#[cfg(test)]
mod fuzzy_mmr_tests {
    use super::*;

    fn key(doc: &str) -> ChunkKey {
        ChunkKey { doc: doc.to_string(), page: 1, idx: 0 }
    }

    fn rec(doc: &str, hot: usize) -> ChunkRec {
        let mut emb = [0.0f32; EMB_DIM];
        emb[hot] = 1.0;
        ChunkRec { key: key(doc), words: vec![], emb }
    }

    #[test]
    fn levenshtein_is_bounded() {
        assert_eq!(levenshtein("escapement", "escapement", 2), 0);
        assert_eq!(levenshtein("escapment", "escapement", 2), 1); // one deletion
        assert_eq!(levenshtein("escaprnent", "escapement", 2), 2); // OCR rn->m
        assert_eq!(levenshtein("abc", "abd", 2), 1);
        // beyond the cap: reports max+1, not the true distance (3)
        assert_eq!(levenshtein("kitten", "sitting", 2), 3);
    }

    #[test]
    fn mmr_demotes_a_near_duplicate() {
        // a and b share an embedding direction (near-duplicate); c is novel.
        // fused order by relevance is a, b, c — MMR should promote c over b.
        let fused = vec![(0.9f32, key("a")), (0.85, key("b")), (0.5, key("c"))];
        let resolve = |k: &ChunkKey| match k.doc.as_str() {
            "a" => Some(rec("a", 0)),
            "b" => Some(rec("b", 0)),
            "c" => Some(rec("c", 1)),
            _ => None,
        };
        let out: Vec<String> =
            mmr_rerank(fused, &resolve).into_iter().map(|(_, k)| k.doc).collect();
        assert_eq!(out, vec!["a", "c", "b"]);
    }

    #[test]
    fn mmr_preserves_order_without_duplicates() {
        // all-distinct directions: MMR must not reshuffle a diverse list
        let fused = vec![(0.9f32, key("a")), (0.8, key("b")), (0.7, key("c"))];
        let resolve = |k: &ChunkKey| match k.doc.as_str() {
            "a" => Some(rec("a", 0)),
            "b" => Some(rec("b", 1)),
            "c" => Some(rec("c", 2)),
            _ => None,
        };
        let out: Vec<String> =
            mmr_rerank(fused, &resolve).into_iter().map(|(_, k)| k.doc).collect();
        assert_eq!(out, vec!["a", "b", "c"]);
    }
}

