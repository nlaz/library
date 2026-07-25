//! Shared chunk records: keys, words, and the stored record type.

use serde::{Deserialize, Serialize};

use crate::Emb;

/// Stable identity of one chunk: a contiguous run of words on one page.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChunkKey {
    pub doc: String,
    pub page: u32,
    pub idx: u32,
}

/// Synthetic docs (note-box cards, annotation notes) live under reserved
/// `~`-prefixed ids. `doc_id` sanitizes `~` out of every real id, so the
/// namespaces can never collide — but reserved ids contain `/` and must
/// never reach a filesystem path join.
pub fn is_reserved(doc: &str) -> bool {
    doc.starts_with('~')
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
pub(crate) mod f32_array {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[f32; N], s: S) -> Result<S::Ok, S::Error> {
        v[..].serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[f32; N], D::Error> {
        let v = Vec::<f32>::deserialize(d)?;
        v.try_into().map_err(|v: Vec<f32>| {
            serde::de::Error::custom(format!("expected {N} floats, got {}", v.len()))
        })
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
