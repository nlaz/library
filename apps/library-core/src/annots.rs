//! Annotations: user marks on document pages.
//!
//! Two shapes, one record. A *text* mark anchors to a word-index range of
//! the page's OCR words and snapshots both the text and the merged line
//! boxes at creation time — the snapshot is what renders and searches, so
//! a later re-OCR can never silently move or reword a mark. A *region*
//! mark is a normalized bbox in the same 0..1 top-left space the OCR
//! words and figure detections use.
//!
//! Source of truth is one sidecar per document, `data/annotations/
//! <doc>.json`, mirroring the `status/<doc>.json` pattern (`doc` is
//! always a sanitized real doc id, so it is filesystem-safe by
//! construction). Marks with a note also mint a synthetic search chunk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ChunkKey, ChunkRec, Emb, Library, Word, sidecar};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnnotKind {
    /// Word-range highlight: `w0..w1` (exclusive) into the page's OCR
    /// words, plus text and per-line box snapshots.
    Text {
        w0: u32,
        w1: u32,
        text: String,
        boxes: Vec<[f32; 4]>,
    },
    /// Dragged rectangle, normalized `[x, y, w, h]`.
    Region { bbox: [f32; 4] },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotRec {
    /// Opaque stable id (`a` + 12 hex).
    pub id: String,
    pub doc: String,
    pub page: u32,
    #[serde(flatten)]
    pub kind: AnnotKind,
    /// Margin note; empty = plain highlight.
    #[serde(default)]
    pub note: String,
    /// Unix seconds.
    pub created: u64,
}

impl AnnotRec {
    /// Vertical anchor for page-order sorting.
    fn y(&self) -> f32 {
        match &self.kind {
            AnnotKind::Text { boxes, .. } => boxes.first().map_or(0.0, |b| b[1]),
            AnnotKind::Region { bbox } => bbox[1],
        }
    }
}

fn dir(data: &Path) -> PathBuf {
    data.join("annotations")
}

fn path(data: &Path, doc: &str) -> PathBuf {
    dir(data).join(format!("{doc}.json"))
}

/// Every annotation on `doc`, in reading order (page, then top edge).
/// Missing or corrupt sidecar reads as empty.
pub fn load_annots(data: &Path, doc: &str) -> Vec<AnnotRec> {
    let mut annots: Vec<AnnotRec> = sidecar::read_json(&path(data, doc)).unwrap_or_default();
    annots.sort_by(|a, b| {
        (a.page, a.y())
            .partial_cmp(&(b.page, b.y()))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    annots
}

pub fn store_annots(data: &Path, doc: &str, annots: &[AnnotRec]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir(data))?;
    sidecar::write_json_atomic(&path(data, doc), &annots)
}

// --- search integration ----------------------------------------------------

/// Reserved search-namespace doc id for an annotation. Never
/// filesystem-safe.
pub fn annot_doc(id: &str) -> String {
    format!("~annot/{id}")
}

fn annot_key(id: &str) -> ChunkKey {
    ChunkKey {
        doc: annot_doc(id),
        page: 0,
        idx: 0,
    }
}

/// The searchable text of a mark: the margin note, plus the quoted
/// snapshot for context. Only the *note* makes a mark searchable — the
/// passage itself is already indexed under its real page, so a bare
/// highlight minting a chunk would just duplicate that hit.
fn annot_text(a: &AnnotRec) -> Option<String> {
    if a.note.trim().is_empty() {
        return None;
    }
    let mut text = a.note.clone();
    if let AnnotKind::Text { text: snap, .. } = &a.kind
        && !snap.is_empty()
    {
        text.push('\n');
        text.push_str(snap);
    }
    Some(text)
}

/// One synthetic chunk per noted mark; zero-geometry words like cards.
pub fn annot_chunk(a: &AnnotRec, embed: &dyn Fn(&str) -> Emb) -> Option<ChunkRec> {
    let text = annot_text(a)?;
    let words = text
        .split_whitespace()
        .map(|t| Word {
            t: t.to_string(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        })
        .collect();
    Some(ChunkRec {
        key: annot_key(&a.id),
        words,
        emb: embed(&text),
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Create or update a mark: sidecar write, then its search presence.
/// An empty id mints one; a note-less mark retracts any prior chunk.
pub fn save_annot(
    lib: &mut Library,
    data: &Path,
    mut a: AnnotRec,
    embed: &dyn Fn(&str) -> Emb,
) -> std::io::Result<AnnotRec> {
    if a.id.is_empty() {
        a.id = crate::notes::mint_id('a');
    }
    if a.created == 0 {
        a.created = now();
    }
    let mut annots = load_annots(data, &a.doc);
    match annots.iter_mut().find(|x| x.id == a.id) {
        Some(slot) => *slot = a.clone(),
        None => annots.push(a.clone()),
    }
    store_annots(data, &a.doc, &annots)?;
    let doc = annot_doc(&a.id);
    match annot_chunk(&a, embed) {
        Some(chunk) => {
            crate::store::commit_chunks(lib, &doc, &[chunk]);
        }
        None => {
            crate::store::commit_chunks(lib, &doc, &[]);
        }
    }
    Ok(a)
}

/// Every annotation across every doc — the id → record view wire shaping
/// needs (annotation ids don't encode their doc). One small JSON per doc;
/// a personal library has dozens, not thousands.
pub fn load_all(data: &Path) -> Vec<AnnotRec> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(data)) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json")
            && let Some(annots) = sidecar::read_json::<Vec<AnnotRec>>(&p)
        {
            out.extend(annots);
        }
    }
    out
}

pub fn delete_annot(lib: &mut Library, data: &Path, doc: &str, id: &str) -> std::io::Result<()> {
    let mut annots = load_annots(data, doc);
    annots.retain(|a| a.id != id);
    store_annots(data, doc, &annots)?;
    crate::store::commit_chunks(lib, &annot_doc(id), &[]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_mark(id: &str, page: u32, y: f32) -> AnnotRec {
        AnnotRec {
            id: id.to_string(),
            doc: "moxon".to_string(),
            page,
            kind: AnnotKind::Text {
                w0: 10,
                w1: 14,
                text: "an hundred and twenty".to_string(),
                boxes: vec![[0.1, y, 0.5, 0.02]],
            },
            note: "check the day-book".to_string(),
            created: 7,
        }
    }

    fn region_mark(id: &str, page: u32, y: f32) -> AnnotRec {
        AnnotRec {
            id: id.to_string(),
            doc: "moxon".to_string(),
            page,
            kind: AnnotKind::Region {
                bbox: [0.25, y, 0.5, 0.25],
            },
            note: String::new(),
            created: 8,
        }
    }

    #[test]
    fn round_trip_in_reading_order() {
        let data = std::env::temp_dir().join(format!("annots-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();

        assert!(load_annots(&data, "moxon").is_empty());
        // stored shuffled; loads sorted by (page, y)
        store_annots(
            &data,
            "moxon",
            &[
                region_mark("a3", 4, 0.1),
                text_mark("a1", 2, 0.8),
                text_mark("a2", 2, 0.3),
            ],
        )
        .unwrap();
        let ids: Vec<String> = load_annots(&data, "moxon")
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["a2", "a1", "a3"]);

        // other docs unaffected
        assert!(load_annots(&data, "fournier").is_empty());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn wire_shape_is_pinned() {
        // the TS side builds and matches these exact shapes
        let t = serde_json::to_value(text_mark("a1", 2, 0.5)).unwrap();
        assert_eq!(t["kind"], "text");
        assert_eq!(t["w0"], 10);
        assert_eq!(t["boxes"][0][1], 0.5);
        assert_eq!(t["note"], "check the day-book");

        let r = serde_json::to_value(region_mark("a2", 3, 0.125)).unwrap();
        assert_eq!(r["kind"], "region");
        assert_eq!(r["bbox"][2], 0.5);

        // note defaults empty on the way back in
        let json = r#"{"id":"a9","doc":"d","page":1,"kind":"region","bbox":[0,0,1,1],"created":0}"#;
        let back: AnnotRec = serde_json::from_str(json).unwrap();
        assert_eq!(back.note, "");
    }
}
