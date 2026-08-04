//! Shared types, the fold graph, and hybrid search for The Library.

pub use fxhash::FxHashSet;

pub mod annots;
pub mod atlas;
pub mod images;
pub mod legibility;
pub mod meta;
pub mod naming;
pub mod notes;
pub mod perf;
pub mod rank;
pub mod records;
pub mod roots;
pub mod search_api;
pub mod sidecar;
pub mod store;
pub mod termdict;
pub mod text;
pub mod tools;
pub mod wire;

pub use search_api::{K, K_DOC, Query, answer};

pub use images::{
    Bbox, IMG_FETCH, IMG_MIN_REL, ImageHit, ImageIn, ImageKey, ImageRec, Images, ImgGraph,
    ImgManifestSink, ImgMetaSink, ImgReaders, ImgVecIndex, ImgVecSink, image_search, img_graph,
    open_images, try_open_images,
};
pub use rank::{Hit, LEX_FETCH, MIN_REL, RankerStats, search};
pub(crate) use rank::{MMR_LAMBDA, MMR_POOL};
pub use records::{ChunkKey, ChunkRec, Word};
pub use store::{
    ChunkIn, Graph, LexSink, LexTok, Library, ManifestSink, Readers, TermSink, VecIndex, VecSink,
    commit_chunks, graph, open, try_open,
};
pub use termdict::{TermDict, TermDictReader};
pub use text::{lex_tokenize, tokenize};

/// Text embeddings come from ese's compile-time static model; the dimension
/// follows its `dim-*` cargo feature. NOTE: with `dim-512` this equals
/// CLIP_DIM, so the type system no longer catches a text/CLIP embedding
/// mix-up — keep the two paths visibly separate.
pub const EMB_DIM: usize = ese::DIMENSIONS;
pub type Emb = [f32; EMB_DIM];

/// Dot product over eight independent accumulators.
///
/// The accumulator array is load-bearing, not a micro-optimization. f32
/// addition is not associative, so LLVM may not reassociate a single-`acc`
/// reduction — `a.iter().zip(b).map(|(x, y)| x * y).sum()` compiles to a
/// serial chain of scalar FMAs at every opt-level. At EMB_DIM=512 that
/// measured 300ns against 35ns for this shape on an M3 (~8.6x). Eight
/// partial sums give the vectorizer eight independent chains to work with.
/// Same trick, and the same reason, as `fold8` in `anny::metric`.
///
/// Reassociating changes the summation order, so results differ from a
/// serial sum in the last ulp or two — fine for ranking, not for anything
/// asserting exact float equality.
#[inline]
pub(crate) fn dot(a: &Emb, b: &Emb) -> f32 {
    let mut acc = [0.0f32; 8];
    let mut i = 0;
    while i + 8 <= EMB_DIM {
        let (xa, xb) = (&a[i..i + 8], &b[i..i + 8]); // proves indices in-bounds
        for l in 0..8 {
            acc[l] += xa[l] * xb[l];
        }
        i += 8;
    }
    let mut s = 0.0f32;
    for &lane in &acc {
        s += lane;
    }
    while i < EMB_DIM {
        s += a[i] * b[i];
        i += 1;
    }
    s
}

/// CLIP ViT-B/32 shared text/image space.
pub const CLIP_DIM: usize = 512;
pub type ClipEmb = [f32; CLIP_DIM];
