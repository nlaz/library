//! The note box: atomic cards with permanent branching addresses.
//!
//! A card is one claim — a title, a short body, evidence quotes anchored
//! to document pages, and typed links to other cards. Cards live in
//! threads; a card's *address* records where in the thread's conversation
//! it was born (`21/3a` = thread 21, card 3, first branch). Addresses are
//! minted once and never renumbered — they are display furniture, not
//! foreign keys. Identity is the opaque `id`, so links and the search
//! namespace survive anything the display layer does.
//!
//! Source of truth is `data/notes/cards.json` (one atomic sidecar, see
//! [`crate::sidecar`]); the search index holds derived synthetic chunks.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::store::commit_chunks;
use crate::{ChunkKey, ChunkRec, Emb, Library, Word, sidecar};

/// A quoted passage: `w0..w1` (exclusive) word-index range into the page's
/// OCR words, plus the text snapshot taken at quote time. The snapshot is
/// what renders and searches — re-OCR can't silently move a quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteAnchor {
    pub doc: String,
    pub page: u32,
    pub w0: u32,
    pub w1: u32,
    pub text: String,
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
    /// 1-based thread number.
    pub thread: u32,
    /// 1-based address segments within the thread. Even indices are
    /// numeric positions, odd indices are branch letters in display form:
    /// `[3]` → `3`, `[3,1]` → `3a`, `[3,1,2]` → `3a2`. Lexicographic
    /// order on `(thread, addr)` is exactly thread-view reading order.
    pub addr: Vec<u32>,
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

// --- addresses -------------------------------------------------------------

/// Next unused thread number.
pub fn next_thread(cards: &[CardRec]) -> u32 {
    cards.iter().map(|c| c.thread).max().unwrap_or(0) + 1
}

/// Next trunk position in `thread`: `[max + 1]` over the thread's
/// top-level cards. Filed cards keep their numbers — addresses are
/// append-only.
pub fn mint_trunk(cards: &[CardRec], thread: u32) -> Vec<u32> {
    let max = cards
        .iter()
        .filter(|c| c.thread == thread && c.addr.len() == 1)
        .map(|c| c.addr[0])
        .max()
        .unwrap_or(0);
    vec![max + 1]
}

/// Next branch under `parent`: parent's address plus one more segment,
/// numbered after the last existing direct child.
pub fn mint_branch(cards: &[CardRec], parent: &CardRec) -> Vec<u32> {
    let max = cards
        .iter()
        .filter(|c| {
            c.thread == parent.thread
                && c.addr.len() == parent.addr.len() + 1
                && c.addr[..parent.addr.len()] == parent.addr[..]
        })
        .map(|c| *c.addr.last().unwrap_or(&0))
        .max()
        .unwrap_or(0);
    let mut addr = parent.addr.clone();
    addr.push(max + 1);
    addr
}

/// Bijective base-26: 1 → `a`, 26 → `z`, 27 → `aa`.
fn letters(mut n: u32) -> String {
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn unletters(s: &str) -> Option<u32> {
    let mut n: u32 = 0;
    for c in s.bytes() {
        if !c.is_ascii_lowercase() {
            return None;
        }
        n = n.checked_mul(26)?.checked_add(u32::from(c - b'a') + 1)?;
    }
    (n > 0).then_some(n)
}

/// `21/3a2` for thread 21, addr `[3,1,2]`.
pub fn display_addr(thread: u32, addr: &[u32]) -> String {
    let mut out = format!("{thread}/");
    for (i, seg) in addr.iter().enumerate() {
        if i % 2 == 0 {
            out.push_str(&seg.to_string());
        } else {
            out.push_str(&letters(*seg));
        }
    }
    out
}

/// Inverse of [`display_addr`]; `None` on anything malformed.
pub fn parse_addr(s: &str) -> Option<(u32, Vec<u32>)> {
    let (thread, rest) = s.split_once('/')?;
    let thread: u32 = thread.parse().ok()?;
    let mut addr = Vec::new();
    let mut chunk = String::new();
    let mut digits = true;
    for c in rest.chars() {
        if c.is_ascii_digit() != digits {
            addr.push(take_seg(&mut chunk, digits)?);
            digits = !digits;
        }
        chunk.push(c);
    }
    addr.push(take_seg(&mut chunk, digits)?);
    // segments must alternate starting numeric
    Some((thread, addr))
}

fn take_seg(chunk: &mut String, digits: bool) -> Option<u32> {
    let seg = if digits {
        chunk.parse().ok()?
    } else {
        unletters(chunk)?
    };
    chunk.clear();
    Some(seg)
}

/// Sort key for thread-view order: branches read directly after their
/// parent, trunks in numeric order, threads apart.
pub fn addr_key(c: &CardRec) -> (u32, &[u32]) {
    (c.thread, &c.addr)
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
        text.push('\n');
        text.push_str(&q.text);
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

/// Input for a card birth. `parent` wins over `thread`; neither starts a
/// fresh thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewCard {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub evidence: Vec<QuoteAnchor>,
    #[serde(default)]
    pub links: Vec<CardLink>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub thread: Option<u32>,
}

/// Mint and persist a new card: address from its birth context, sidecar
/// write, then the synthetic chunk.
pub fn create_card(
    lib: &mut Library,
    data: &Path,
    input: NewCard,
    embed: &dyn Fn(&str) -> Emb,
) -> io::Result<CardRec> {
    let mut cards = load_cards(data);
    let (thread, addr) = match (&input.parent, input.thread) {
        (Some(pid), _) => {
            let parent = cards
                .iter()
                .find(|c| c.id == *pid)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown parent card"))?
                .clone();
            (parent.thread, mint_branch(&cards, &parent))
        }
        (None, Some(t)) => (t, mint_trunk(&cards, t)),
        (None, None) => {
            let t = next_thread(&cards);
            (t, vec![1])
        }
    };
    let ts = now();
    let card = CardRec {
        id: mint_id('c'),
        thread,
        addr,
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

/// Save an edit. Identity and address are immutable — the stored id,
/// thread, addr, and created stamp win over whatever the client sent.
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
        thread: slot.thread,
        addr: slot.addr.clone(),
        created: slot.created,
        modified: now(),
        ..card
    };
    *slot = saved.clone();
    store_cards(data, &cards)?;
    reindex_card(lib, &saved, embed);
    Ok(saved)
}

/// Where a fresh card with this text would file: after its nearest
/// existing card by embedding. `None` when the box is empty (of live,
/// indexed cards) — the composer falls back to "new thread".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadProposal {
    /// The proposed parent card.
    pub parent: String,
    pub parent_address: String,
    pub parent_title: String,
    pub thread: u32,
    /// The address the new card would receive.
    pub address: String,
}

pub fn propose_thread(lib: &Library, data: &Path, emb: &Emb) -> Option<ThreadProposal> {
    let nearest = lib.rtx(|((_, vec), _)| {
        vec.search_filtered(emb, |key: &ChunkKey| key.doc.starts_with("~card/"))
            .into_iter()
            .next()
            .map(|s| s.val)
    })?;
    let id = nearest.doc.strip_prefix("~card/")?.to_string();
    let cards = load_cards(data);
    let parent = cards.iter().find(|c| c.id == id)?;
    let addr = mint_branch(&cards, parent);
    Some(ThreadProposal {
        parent: parent.id.clone(),
        parent_address: display_addr(parent.thread, &parent.addr),
        parent_title: parent.title.clone(),
        thread: parent.thread,
        address: display_addr(parent.thread, &addr),
    })
}

/// A near-but-unlinked card for the thread rail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborCard {
    pub id: String,
    pub address: String,
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
                address: display_addr(c.thread, &c.addr),
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

    fn card(id: &str, thread: u32, addr: &[u32]) -> CardRec {
        CardRec {
            id: id.to_string(),
            thread,
            addr: addr.to_vec(),
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
    fn minting_is_append_only_under_interleaving() {
        let mut cards: Vec<CardRec> = vec![];
        assert_eq!(next_thread(&cards), 1);

        // first trunk card of thread 1
        assert_eq!(mint_trunk(&cards, 1), vec![1]);
        cards.push(card("a", 1, &[1]));
        // second trunk
        assert_eq!(mint_trunk(&cards, 1), vec![2]);
        cards.push(card("b", 1, &[2]));
        // branch off 1/2
        let parent = cards[1].clone();
        assert_eq!(mint_branch(&cards, &parent), vec![2, 1]);
        cards.push(card("c", 1, &[2, 1]));
        // another trunk after branching — trunk numbering unaffected
        assert_eq!(mint_trunk(&cards, 1), vec![3]);
        cards.push(card("d", 1, &[3]));
        // second branch off 1/2 lands after the first
        assert_eq!(mint_branch(&cards, &parent), vec![2, 2]);
        cards.push(card("e", 1, &[2, 2]));
        // branch off the branch: numeric segment again
        let sub = cards[2].clone();
        assert_eq!(mint_branch(&cards, &sub), vec![2, 1, 1]);

        // filing a card frees nothing
        cards[3].filed = true;
        assert_eq!(mint_trunk(&cards, 1), vec![4]);
        // fresh thread numbering
        assert_eq!(next_thread(&cards), 2);
        cards.push(card("f", 2, &[1]));
        assert_eq!(next_thread(&cards), 3);
        assert_eq!(mint_trunk(&cards, 2), vec![2]);
    }

    #[test]
    fn display_and_parse_round_trip() {
        for (thread, addr, want) in [
            (21, vec![3], "21/3"),
            (21, vec![3, 1], "21/3a"),
            (21, vec![3, 1, 2], "21/3a2"),
            (21, vec![3, 1, 2, 1], "21/3a2a"),
            (1, vec![12, 26], "1/12z"),
            (1, vec![12, 27], "1/12aa"),
            (1, vec![12, 52], "1/12az"),
            (1, vec![12, 53], "1/12ba"),
        ] {
            assert_eq!(display_addr(thread, &addr), want);
            assert_eq!(parse_addr(want), Some((thread, addr)));
        }
        assert_eq!(parse_addr("21"), None);
        assert_eq!(parse_addr("x/3"), None);
        assert_eq!(parse_addr("21/"), None);
    }

    #[test]
    fn sort_order_reads_as_a_thread() {
        let mut cards = [
            card("e", 2, &[1]),
            card("b", 1, &[3, 1]),
            card("d", 1, &[4]),
            card("a", 1, &[3]),
            card("c", 1, &[3, 1, 1]),
            card("f", 1, &[3, 2]),
        ];
        cards.sort_by(|x, y| addr_key(x).cmp(&addr_key(y)));
        let order: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
        // 1/3 → 1/3a → 1/3a1 → 1/3b → 1/4 → thread 2
        assert_eq!(order, vec!["a", "b", "c", "f", "d", "e"]);
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
        let mut c = card("c1", 1, &[1]);
        c.evidence.push(QuoteAnchor {
            doc: "moxon".into(),
            page: 215,
            w0: 10,
            w1: 24,
            text: "an hundred and twenty in the hour".into(),
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
