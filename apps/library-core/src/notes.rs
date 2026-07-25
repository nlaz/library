//! Notes: atomic cards on a flat timeline.
//!
//! A card is one claim — a title, a short body, evidence quotes anchored
//! to document pages, and typed links to other cards. There is no
//! hierarchy: the notes view is a reverse-chronological journal, and any
//! structure is carried by explicit links. Identity is the opaque `id`,
//! so links and the search namespace survive anything the display layer
//! does.
//!
//! Source of truth is `data/notes/cards.json` (one atomic sidecar, see
//! [`crate::sidecar`]); the search index holds derived synthetic chunks.
//! Cards written by the old threaded schema carry extra `thread`/`addr`
//! keys — serde ignores them on load and the next save drops them.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::store::commit_chunks;
use crate::{ChunkKey, ChunkRec, Emb, Library, Word, sidecar};

/// The shape of an evidence anchor on its page. Mirrors the mark
/// geometry the reader draws: snapshots are taken at mark time, so a
/// later re-OCR can never silently move or reword a mark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnchorKind {
    /// Word-range mark: `w0..w1` (exclusive) into the page's OCR words,
    /// plus text and per-line box snapshots.
    Text {
        w0: u32,
        w1: u32,
        text: String,
        boxes: Vec<[f32; 4]>,
    },
    /// Dragged rectangle, normalized `[x, y, w, h]`. Carries no text.
    Region { bbox: [f32; 4] },
}

/// A mark on a document page, kept as a card's evidence: where it lives
/// and what it looks like. What renders on the page and what searches
/// both come from the snapshot inside [`AnchorKind`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteAnchor {
    pub doc: String,
    pub page: u32,
    #[serde(flatten)]
    pub kind: AnchorKind,
}

impl QuoteAnchor {
    /// The quoted snapshot, when the anchor has one.
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            AnchorKind::Text { text, .. } => Some(text),
            AnchorKind::Region { .. } => None,
        }
    }

    /// The page boxes the mark draws.
    pub fn boxes(&self) -> Vec<[f32; 4]> {
        match &self.kind {
            AnchorKind::Text { boxes, .. } => boxes.clone(),
            AnchorKind::Region { bbox } => vec![*bbox],
        }
    }

    /// Vertical anchor for page-order sorting.
    pub fn y(&self) -> f32 {
        match &self.kind {
            AnchorKind::Text { boxes, .. } => boxes.first().map_or(0.0, |b| b[1]),
            AnchorKind::Region { bbox } => bbox[1],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// This card continues the thought of the target.
    Continues,
    /// Cross-thread association.
    Relates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardLink {
    pub to: String,
    pub kind: LinkKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardRec {
    /// Opaque stable id (`c` + 12 hex), minted once.
    pub id: String,
    /// The claim, stated as a sentence.
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub evidence: Vec<QuoteAnchor>,
    #[serde(default)]
    pub links: Vec<CardLink>,
    /// Unix seconds.
    pub created: u64,
    pub modified: u64,
    /// Filed away: out of the box's working set, retracted from search.
    #[serde(default)]
    pub filed: bool,
    /// The "split?" whisper has been shown for this card — never nag twice.
    #[serde(default)]
    pub split_hinted: bool,
}

// --- ids -------------------------------------------------------------------

static MINTED: AtomicU64 = AtomicU64::new(0);

/// Mint an opaque id: prefix + 12 hex chars of wall-clock nanos mixed with
/// a process counter. Uniqueness needs only "one library, occasional
/// mints" — not cryptography.
pub fn mint_id(prefix: char) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = MINTED.fetch_add(1, Ordering::Relaxed);
    // odd multiplier is a bijection mod 2^48, so equal-nanos mints in one
    // process still get distinct low bits
    let mix = nanos ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (u64::from(std::process::id()) << 40);
    format!("{prefix}{:012x}", mix & 0xffff_ffff_ffff)
}

// --- sidecar ---------------------------------------------------------------

fn cards_path(data: &Path) -> PathBuf {
    data.join("notes").join("cards.json")
}

/// Every card in the box. Missing or corrupt sidecar reads as empty.
pub fn load_cards(data: &Path) -> Vec<CardRec> {
    sidecar::read_json(&cards_path(data)).unwrap_or_default()
}

pub fn store_cards(data: &Path, cards: &[CardRec]) -> std::io::Result<()> {
    std::fs::create_dir_all(data.join("notes"))?;
    sidecar::write_json_atomic(&cards_path(data), &cards)
}

// --- search integration ----------------------------------------------------

/// Reserved search-namespace doc id for a card. Never filesystem-safe.
pub fn card_doc(id: &str) -> String {
    format!("~card/{id}")
}

fn card_key(id: &str) -> ChunkKey {
    ChunkKey {
        doc: card_doc(id),
        page: 0,
        idx: 0,
    }
}

/// A card's searchable text: claim, body, and the quoted evidence — so
/// quoting a passage makes the card findable by that passage's words.
fn card_text(card: &CardRec) -> String {
    let mut text = card.title.clone();
    if !card.body.is_empty() {
        text.push('\n');
        text.push_str(&card.body);
    }
    for q in &card.evidence {
        if let Some(snap) = q.text() {
            text.push('\n');
            text.push_str(snap);
        }
    }
    text
}

/// One synthetic chunk per card (cards stay card-sized — far under the
/// ingest window — and one chunk means one hit, never a double surface).
/// Words carry zero geometry; wire shaping strips the boxes downstream.
pub fn card_chunk(card: &CardRec, embed: &dyn Fn(&str) -> Emb) -> ChunkRec {
    let text = card_text(card);
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
    ChunkRec {
        key: card_key(&card.id),
        words,
        emb: embed(&text),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Re-mint a card's search presence after a save: filed cards retract,
/// live cards commit their one chunk (the manifest diff replaces any
/// prior version).
fn reindex_card(lib: &mut Library, card: &CardRec, embed: &dyn Fn(&str) -> Emb) {
    let doc = card_doc(&card.id);
    if card.filed {
        commit_chunks(lib, &doc, &[]);
    } else {
        commit_chunks(lib, &doc, &[card_chunk(card, embed)]);
    }
}

/// Input for a card birth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewCard {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub evidence: Vec<QuoteAnchor>,
    #[serde(default)]
    pub links: Vec<CardLink>,
}

/// Mint and persist a new card: sidecar write, then the synthetic chunk.
pub fn create_card(
    lib: &mut Library,
    data: &Path,
    input: NewCard,
    embed: &dyn Fn(&str) -> Emb,
) -> io::Result<CardRec> {
    let mut cards = load_cards(data);
    let ts = now();
    let card = CardRec {
        id: mint_id('c'),
        title: input.title,
        body: input.body,
        evidence: input.evidence,
        links: input.links,
        created: ts,
        modified: ts,
        filed: false,
        split_hinted: false,
    };
    cards.push(card.clone());
    store_cards(data, &cards)?;
    reindex_card(lib, &card, embed);
    Ok(card)
}

/// Save an edit. Identity is immutable — the stored id and created stamp
/// win over whatever the client sent.
pub fn update_card(
    lib: &mut Library,
    data: &Path,
    card: CardRec,
    embed: &dyn Fn(&str) -> Emb,
) -> io::Result<CardRec> {
    let mut cards = load_cards(data);
    let slot = cards
        .iter_mut()
        .find(|c| c.id == card.id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown card"))?;
    let saved = CardRec {
        id: slot.id.clone(),
        created: slot.created,
        modified: now(),
        ..card
    };
    *slot = saved.clone();
    store_cards(data, &cards)?;
    reindex_card(lib, &saved, embed);
    Ok(saved)
}

/// A near-but-unlinked card for the related rail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborCard {
    pub id: String,
    pub title: String,
    /// Cosine distance (smaller = nearer).
    pub score: f32,
}

/// Embedding neighbors of `id` among live cards, excluding itself and
/// anything already linked in either direction.
pub fn card_neighbors(lib: &Library, data: &Path, id: &str, k: usize) -> Vec<NeighborCard> {
    let Some(rec) = lib.get(&card_key(id)) else {
        return Vec::new(); // filed or unknown: no chunk, no neighborhood
    };
    let cards = load_cards(data);
    let Some(me) = cards.iter().find(|c| c.id == id) else {
        return Vec::new();
    };
    let linked: crate::FxHashSet<&str> = me
        .links
        .iter()
        .map(|l| l.to.as_str())
        .chain(
            cards
                .iter()
                .filter(|c| c.links.iter().any(|l| l.to == id))
                .map(|c| c.id.as_str()),
        )
        .collect();
    let scored = lib.rtx(|((_, vec), _)| {
        vec.search_filtered(&rec.emb, |key: &ChunkKey| key.doc.starts_with("~card/"))
    });
    scored
        .into_iter()
        .filter_map(|s| {
            let nid = s.val.doc.strip_prefix("~card/")?;
            if nid == id || linked.contains(nid) {
                return None;
            }
            let c = cards.iter().find(|c| c.id == nid && !c.filed)?;
            Some(NeighborCard {
                id: c.id.clone(),
                title: c.title.clone(),
                score: s.score,
            })
        })
        .take(k)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str) -> CardRec {
        CardRec {
            id: id.to_string(),
            title: format!("card {id}"),
            body: String::new(),
            evidence: vec![],
            links: vec![],
            created: 0,
            modified: 0,
            filed: false,
            split_hinted: false,
        }
    }

    #[test]
    fn legacy_threaded_cards_still_load() {
        // sidecars written by the threaded schema carry thread/addr keys;
        // they must deserialize (dropped), not fail the whole file
        let legacy = serde_json::json!({
            "id": "c000000000001",
            "thread": 3,
            "addr": [2, 1],
            "title": "a legacy card",
            "body": "",
            "evidence": [],
            "links": [],
            "created": 100,
            "modified": 100,
            "filed": false,
            "split_hinted": false
        });
        let c: CardRec = serde_json::from_value(legacy).unwrap();
        assert_eq!(c.id, "c000000000001");
        assert_eq!(c.title, "a legacy card");
        let back = serde_json::to_value(&c).unwrap();
        assert!(back.get("thread").is_none());
        assert!(back.get("addr").is_none());
    }

    #[test]
    fn ids_are_unique_and_prefixed() {
        let ids: Vec<String> = (0..64).map(|_| mint_id('c')).collect();
        assert!(ids.iter().all(|id| id.starts_with('c') && id.len() == 13));
        let set: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(set.len(), ids.len());
    }

    #[test]
    fn sidecar_round_trip() {
        let dir = std::env::temp_dir().join(format!("notes-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(load_cards(&dir).is_empty());
        let mut c = card("c1");
        c.evidence.push(QuoteAnchor {
            doc: "moxon".into(),
            page: 215,
            kind: AnchorKind::Text {
                w0: 10,
                w1: 24,
                text: "an hundred and twenty in the hour".into(),
                boxes: vec![[0.1, 0.2, 0.5, 0.02]],
            },
        });
        c.evidence.push(QuoteAnchor {
            doc: "moxon".into(),
            page: 216,
            kind: AnchorKind::Region {
                bbox: [0.25, 0.1, 0.5, 0.25],
            },
        });
        c.links.push(CardLink {
            to: "c2".into(),
            kind: LinkKind::Relates,
        });
        store_cards(&dir, &[c.clone()]).unwrap();
        assert_eq!(load_cards(&dir), vec![c]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn anchor_wire_shape_is_pinned() {
        // the TS side builds and matches these exact shapes
        let t = serde_json::to_value(QuoteAnchor {
            doc: "moxon".into(),
            page: 2,
            kind: AnchorKind::Text {
                w0: 10,
                w1: 14,
                text: "an hundred and twenty".into(),
                boxes: vec![[0.1, 0.5, 0.5, 0.02]],
            },
        })
        .unwrap();
        assert_eq!(t["kind"], "text");
        assert_eq!(t["w0"], 10);
        assert_eq!(t["boxes"][0][1], 0.5);

        let r = serde_json::to_value(QuoteAnchor {
            doc: "moxon".into(),
            page: 3,
            kind: AnchorKind::Region {
                bbox: [0.25, 0.125, 0.5, 0.25],
            },
        })
        .unwrap();
        assert_eq!(r["kind"], "region");
        assert_eq!(r["bbox"][2], 0.5);
        assert!(r.get("text").is_none());

        let back: QuoteAnchor = serde_json::from_value(r).unwrap();
        assert_eq!(back.text(), None);
        assert_eq!(back.boxes(), vec![[0.25, 0.125, 0.5, 0.25]]);
        assert_eq!(back.y(), 0.125);
    }

    #[test]
    fn card_text_skips_region_anchors() {
        let mut c = card("c1");
        c.title = "presses ran fast".into();
        c.evidence.push(QuoteAnchor {
            doc: "moxon".into(),
            page: 216,
            kind: AnchorKind::Region {
                bbox: [0.25, 0.1, 0.5, 0.25],
            },
        });
        c.evidence.push(QuoteAnchor {
            doc: "moxon".into(),
            page: 215,
            kind: AnchorKind::Text {
                w0: 10,
                w1: 24,
                text: "an hundred and twenty in the hour".into(),
                boxes: vec![],
            },
        });
        assert_eq!(
            card_text(&c),
            "presses ran fast\nan hundred and twenty in the hour"
        );
    }

    #[test]
    fn link_kind_serializes_snake_case() {
        // the TS side matches on these exact strings
        assert_eq!(
            serde_json::to_string(&LinkKind::Continues).unwrap(),
            "\"continues\""
        );
        assert_eq!(
            serde_json::to_string(&LinkKind::Relates).unwrap(),
            "\"relates\""
        );
    }
}
