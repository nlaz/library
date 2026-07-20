//! Shared types, the fold graph, and hybrid search for The Library.

pub use fxhash::FxHashSet;

pub mod images;
pub mod legibility;
pub mod perf;
pub mod rank;
pub mod records;
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
    graph, open, try_open,
};
pub use termdict::{TermDict, TermDictReader};
pub use text::{lex_tokenize, tokenize};

/// Text embeddings come from ese's compile-time static model; the dimension
/// follows its `dim-*` cargo feature. NOTE: with `dim-512` this equals
/// CLIP_DIM, so the type system no longer catches a text/CLIP embedding
/// mix-up — keep the two paths visibly separate.
pub const EMB_DIM: usize = ese::DIMENSIONS;
pub type Emb = [f32; EMB_DIM];

/// CLIP ViT-B/32 shared text/image space.
pub const CLIP_DIM: usize = 512;
pub type ClipEmb = [f32; CLIP_DIM];
