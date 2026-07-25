//! Legacy annotations: the pre-card marks, kept only for migration.
//!
//! Marks used to be their own record — per-document sidecars in
//! `data/annotations/<doc>.json`, with noted marks minting `~annot/`
//! search chunks. A mark is a notebox card now (its geometry rides in
//! the card's evidence anchor), so this module retains just enough to
//! read the old sidecars and migrate them: the wire types, the loaders,
//! and [`migrate_annots_to_cards`]. The sidecar files themselves are
//! never modified or deleted — they are the user's marginalia.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::notes::{self, CardRec};
use crate::{Emb, Library, sidecar};

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

/// Reserved search-namespace doc id an annotation used to index under;
/// the migration retracts these. Never filesystem-safe.
pub fn annot_doc(id: &str) -> String {
    format!("~annot/{id}")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --- migration to cards ----------------------------------------------------

/// The annotation's page geometry as a card evidence anchor.
fn anchor_of(a: &AnnotRec) -> notes::QuoteAnchor {
    notes::QuoteAnchor {
        doc: a.doc.clone(),
        page: a.page,
        kind: match &a.kind {
            AnnotKind::Text {
                w0,
                w1,
                text,
                boxes,
            } => notes::AnchorKind::Text {
                w0: *w0,
                w1: *w1,
                text: text.clone(),
                boxes: boxes.clone(),
            },
            AnnotKind::Region { bbox } => notes::AnchorKind::Region { bbox: *bbox },
        },
    }
}

/// One-time migration: every *noted* mark becomes a notebox card — one
/// fresh thread per source doc, trunk cards in reading order, birth
/// stamps preserved. Placement is deliberately not embedding-proposed:
/// against a near-empty card box a proposal would file old marks by
/// accident, and one "margin notes on this doc" thread per doc is
/// predictable and reviewable. Bare highlights are left behind (marks
/// without notes no longer exist as a concept) and the sidecar files
/// are never modified — only their `~annot/` search chunks retract.
/// Idempotent via a marker file written only on success, so a failed
/// run retries next launch.
pub fn migrate_annots_to_cards(
    lib: &mut Library,
    data: &Path,
    embed: &dyn Fn(&str) -> Emb,
) -> std::io::Result<usize> {
    let marker = dir(data).join(".migrated-to-cards");
    if marker.exists() {
        return Ok(0);
    }
    // per-doc sidecars, in stable (sorted) order so thread numbering is
    // deterministic
    let mut docs: Vec<String> = match std::fs::read_dir(dir(data)) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().is_some_and(|x| x == "json"))
                    .then(|| p.file_stem()?.to_str().map(String::from))
                    .flatten()
            })
            .collect(),
        Err(_) => Vec::new(), // fresh library: nothing to migrate
    };
    docs.sort();

    let mut cards = notes::load_cards(data);
    let mut minted: Vec<CardRec> = Vec::new();
    let mut retract: Vec<String> = Vec::new();
    for doc in &docs {
        let annots = load_annots(data, doc);
        retract.extend(annots.iter().map(|a| a.id.clone()));
        let noted: Vec<&AnnotRec> = annots
            .iter()
            .filter(|a| !a.note.trim().is_empty())
            .collect();
        if noted.is_empty() {
            continue;
        }
        let thread = notes::next_thread(&cards);
        for a in noted {
            let card = CardRec {
                id: notes::mint_id('c'),
                thread,
                addr: notes::mint_trunk(&cards, thread),
                title: a.note.clone(),
                body: String::new(),
                evidence: vec![anchor_of(a)],
                links: Vec::new(),
                created: a.created,
                modified: a.created,
                filed: false,
                split_hinted: false,
            };
            cards.push(card.clone());
            minted.push(card);
        }
    }

    if !minted.is_empty() {
        notes::store_cards(data, &cards)?;
    }
    for card in &minted {
        crate::store::commit_chunks(
            lib,
            &notes::card_doc(&card.id),
            &[notes::card_chunk(card, embed)],
        );
    }
    for id in &retract {
        crate::store::commit_chunks(lib, &annot_doc(id), &[]);
    }

    std::fs::create_dir_all(dir(data))?;
    sidecar::write_json_atomic(
        &marker,
        &serde_json::json!({ "at": now(), "cards": minted.len() }),
    )?;
    Ok(minted.len())
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
    fn sidecar_shape_is_pinned() {
        // the migration must keep reading what the old pen wrote
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
