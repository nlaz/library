//! The graph. fn pointers (not closures) keep every node's type nameable, so
//! the whole graph and its reader tuple get type aliases.

use anny::metric::Cosine;
use fold::pipeline::terminal::search::{Bm25, Bm25Reader, Hnsw, HnswReader};
use fold::pipeline::terminal::{InvertedIndex, InvertedIndexReader};
use fold::pipeline::{FlatMap, Keyed, Map};
use fold::stream::KeyedStream;

use crate::termdict::{TermDict, TermDictReader};
use crate::text::{lex_tokenize, tokenize};
use crate::{ChunkKey, ChunkRec, EMB_DIM, Emb};

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
    (
        InvertedIndexReader<'tx, R, ChunkKey, String>,
        TermDictReader<'tx, R>,
    ),
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
            FlatMap::new(
                to_terms as fn(&ChunkIn) -> Vec<String>,
                TermDict::new("terms"),
            ),
        ),
    )
}

/// Atomic swap of one doc's chunks: upsert the new records, remove keys
/// that vanished, checkpoint. The table retracts replaced records itself
/// and byte-equal upserts skip the graph, so an unchanged chunk costs one
/// point read. Passing `&[]` retracts the doc entirely. Returns
/// (removed, added) — removed counts keys actually deleted.
pub fn commit_chunks(st: &mut Library, doc: &str, recs: &[ChunkRec]) -> (usize, usize) {
    let counts = st.wtx(|tx| {
        let old: Vec<ChunkKey> = tx.rtx(|(_, (manifest, _))| manifest.search(&doc.to_string()));
        let new: crate::FxHashSet<&ChunkKey> = recs.iter().map(|r| &r.key).collect();
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

pub fn open(path: impl AsRef<std::path::Path>) -> Library {
    KeyedStream::new(path, graph())
}

/// Fallible [`open`]: `Err(fjall::Error::Locked)` means another process
/// holds the store.
pub fn try_open(path: impl AsRef<std::path::Path>) -> Result<Library, fjall::Error> {
    KeyedStream::try_new(path, graph())
}
