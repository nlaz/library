//! Wire types + hit shaping shared by every host (server, desktop app).
//!
//! A `WireHit` is what a client renders: the page image to show, the snippet,
//! the matched-word boxes, and a `crop` rect that lets the client zoom past
//! the scan's baked-in margins.

use serde::Serialize;

use crate::{Bbox, Hit, ImageHit, Word, tokenize};

pub type Collections = std::collections::BTreeMap<String, Vec<String>>;

/// Number of rendered page images already in a doc's `data/pages/<doc>/`
/// directory — every host uses this to tell the reader how far it can
/// scroll (the reader has no other source of a doc's total page count).
pub fn count_pages(doc_pages_dir: &std::path::Path) -> u32 {
    std::fs::read_dir(doc_pages_dir)
        .map(|it| {
            it.flatten()
                .filter(|f| {
                    let n = f.file_name();
                    let n = n.to_string_lossy();
                    n.starts_with("page-") && n.ends_with(".jpg")
                })
                .count() as u32
        })
        .unwrap_or(0)
}

pub fn read_collections(data: &std::path::Path) -> Collections {
    std::fs::read(data.join("collections.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

#[derive(Serialize)]
pub struct SnippetWord {
    pub t: String,
    pub m: bool,
}

/// Note-box context on a `kind: "card"` hit.
#[derive(Serialize)]
pub struct CardMeta {
    pub id: String,
    /// Display address, e.g. `21/3a`.
    pub address: String,
    pub title: String,
    pub thread: u32,
    /// `21 · <thread name>` for the result card's locator line.
    pub breadcrumb: String,
}

/// Jump target on a `kind: "annotation"` hit — the *real* doc and page
/// the mark lives on (the hit's own doc is the reserved namespace id).
#[derive(Serialize)]
pub struct AnnotMeta {
    pub id: String,
    pub doc: String,
    pub page: u32,
}

#[derive(Serialize)]
pub struct WireHit {
    /// "text" | "image" | "card" | "annotation"
    pub kind: &'static str,
    pub score: f32,
    /// BM25 score as a fraction of the top lexical hit (the MIN_REL cutoff
    /// signal); 0.0 for image hits, whose CLIP sims live on another scale
    pub rel: f32,
    pub doc: String,
    pub page: u32,
    pub idx: u32,
    pub img: String,
    pub snippet: Vec<SnippetWord>,
    /// normalized [x, y, w, h] boxes of matched words on the page image
    pub boxes: Vec<[f32; 4]>,
    /// normalized [x, y, w, h] bounding box of the chunk's text on the page,
    /// so the client can zoom past the scan's white margins
    pub crop: [f32; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<CardMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annot: Option<AnnotMeta>,
}

#[derive(Serialize)]
pub struct Response {
    pub seq: u64,
    pub phase: &'static str,
    pub us: u128,
    pub hits: Vec<WireHit>,
}

/// One card per page: subdivision indexes whole figures AND their parts, so a
/// single grid page can dominate raw results. Group by (doc, page), keep the
/// best score, and surface every matched region as its own highlight box.
pub fn group_image_hits(found: &[ImageHit], k: usize) -> Vec<WireHit> {
    let mut order: Vec<(String, u32)> = Vec::new();
    let mut groups: std::collections::HashMap<(String, u32), Vec<&ImageHit>> =
        std::collections::HashMap::new();
    for h in found {
        let key = (h.key.doc.clone(), h.key.page);
        groups
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push(h);
    }
    order
        .into_iter()
        .take(k)
        .map(|key| {
            let hs = &groups[&key];
            let boxes: Vec<Bbox> = hs.iter().map(|h| h.bbox).collect();
            let (mut x0, mut y0, mut x1, mut y1) = (1f32, 1f32, 0f32, 0f32);
            for b in &boxes {
                x0 = x0.min(b[0]);
                y0 = y0.min(b[1]);
                x1 = x1.max(b[0] + b[2]);
                y1 = y1.max(b[1] + b[3]);
            }
            let pad = 0.01;
            let best = hs[0];
            WireHit {
                kind: "image",
                score: best.score,
                rel: 0.0,
                doc: best.key.doc.clone(),
                page: best.key.page,
                idx: best.key.idx,
                img: format!("/pages/{}/page-{:04}.jpg", best.key.doc, best.key.page),
                snippet: Vec::new(),
                boxes,
                crop: [
                    (x0 - pad).max(0.0),
                    (y0 - pad).max(0.0),
                    (x1 - x0 + 2.0 * pad).min(1.0),
                    (y1 - y0 + 2.0 * pad).min(1.0),
                ],
                card: None,
                annot: None,
            }
        })
        .collect()
}

/// Text hits (RRF over lexical/semantic) and image hits (CLIP cosine) live
/// on unrelated score scales, so blending by raw `score` would just let
/// whichever scale happens to run bigger dominate the order. Both lists
/// arrive best-first; deal them into the stream at one image per
/// `IMG_CADENCE` slots on average — so figures stay a steady presence
/// through the paginated stream instead of packing the first pages (a
/// rank-weight interleave puts image rank r at text depth ~0.77r, i.e.
/// everything up front). Each figure's exact slot is jittered so the
/// interleave doesn't read as a mechanical every-Nth pattern.
const IMG_CADENCE: usize = 4;

/// Slot for the i-th image: `i * IMG_CADENCE` plus a jitter hashed from the
/// figure's identity. Deterministic — real randomness would reorder between
/// continuation requests and break slice tiling — and dependent only on the
/// image list, so extending the text list never moves a figure (the
/// prefix-stability invariant pagination relies on). Jitter < IMG_CADENCE
/// keeps slots strictly increasing, so gaps vary 1..2*IMG_CADENCE-1 while
/// average density stays 1-per-IMG_CADENCE.
fn img_slot(i: usize, h: &WireHit) -> usize {
    let mut x: u32 = 2166136261; // FNV-1a
    for b in h.doc.bytes() {
        x = (x ^ b as u32).wrapping_mul(16777619);
    }
    x = (x ^ h.page).wrapping_mul(16777619);
    i * IMG_CADENCE + x as usize % IMG_CADENCE
}

pub fn blend(text: Vec<WireHit>, images: Vec<WireHit>) -> Vec<WireHit> {
    let mut out = Vec::with_capacity(text.len() + images.len());
    let mut text = text.into_iter();
    let mut images = images.into_iter().peekable();
    let mut img_idx = 0;
    loop {
        let img_due = images
            .peek()
            .is_some_and(|h| img_slot(img_idx, h) <= out.len());
        let next = if img_due {
            img_idx += 1;
            images.next()
        } else {
            // text first; once it runs dry the remaining images drain in order
            text.next().or_else(|| images.next())
        };
        match next {
            Some(h) => out.push(h),
            None => break,
        }
    }
    out
}

pub fn wire_hit(hit: &Hit, qtoks: &[String]) -> WireHit {
    let matched = |w: &Word| {
        tokenize(&w.t)
            .iter()
            .any(|t| qtoks.iter().any(|q| t.starts_with(q.as_str())))
    };

    let first = hit.words.iter().position(&matched).unwrap_or(0);
    let lo = first.saturating_sub(12);
    let hi = (first + 18).min(hit.words.len());
    let snippet = hit.words[lo..hi]
        .iter()
        .map(|w| SnippetWord {
            t: w.t.clone(),
            m: matched(w),
        })
        .collect();
    let boxes = hit
        .words
        .iter()
        .filter(|w| matched(w))
        .map(|w| [w.x, w.y, w.w, w.h])
        .collect();

    let (mut x0, mut y0, mut x1, mut y1) = (1f32, 1f32, 0f32, 0f32);
    for w in &hit.words {
        x0 = x0.min(w.x);
        y0 = y0.min(w.y);
        x1 = x1.max(w.x + w.w);
        y1 = y1.max(w.y + w.h);
    }
    let crop = if x1 > x0 {
        let pad = 0.015;
        [
            (x0 - pad).max(0.0),
            (y0 - pad).max(0.0),
            (x1 - x0 + 2.0 * pad).min(1.0),
            (y1 - y0 + 2.0 * pad).min(1.0),
        ]
    } else {
        [0.0, 0.0, 1.0, 1.0]
    };

    WireHit {
        kind: "text",
        score: hit.score,
        rel: hit.rel,
        doc: hit.key.doc.clone(),
        page: hit.key.page,
        idx: hit.key.idx,
        img: format!("/pages/{}/page-{:04}.jpg", hit.key.doc, hit.key.page),
        snippet,
        boxes,
        crop,
        card: None,
        annot: None,
    }
}

/// Rewrite reserved-namespace hits (`~card/…`, `~annot/…`) into their
/// user-facing kinds. Synthetic chunks rank through the normal text path
/// on merit; this is the one place their page-scan assumptions (the
/// `/pages/…` img URL, all-zero word boxes) get stripped and their
/// note-box context attached. No-op — and no sidecar reads — unless a
/// reserved hit is actually present.
pub fn decorate_reserved_hits(hits: &mut [WireHit], data: &std::path::Path) {
    use crate::records::is_reserved;

    if !hits.iter().any(|h| is_reserved(&h.doc)) {
        return;
    }
    let cards = crate::notes::load_cards(data);
    let annots = crate::annots::load_all(data);
    for h in hits.iter_mut() {
        if let Some(id) = h.doc.strip_prefix("~card/") {
            h.kind = "card";
            h.img = String::new();
            h.boxes.clear();
            h.crop = [0.0, 0.0, 1.0, 1.0];
            if let Some(c) = cards.iter().find(|c| c.id == id) {
                let name = cards
                    .iter()
                    .filter(|t| t.thread == c.thread && t.addr.len() == 1)
                    .min_by_key(|t| t.addr[0])
                    .map(|t| t.title.as_str())
                    .unwrap_or(c.title.as_str());
                h.card = Some(CardMeta {
                    id: c.id.clone(),
                    address: crate::notes::display_addr(c.thread, &c.addr),
                    title: c.title.clone(),
                    thread: c.thread,
                    breadcrumb: format!("{} · {}", c.thread, name),
                });
            }
        } else if let Some(id) = h.doc.strip_prefix("~annot/") {
            h.kind = "annotation";
            h.img = String::new();
            h.boxes.clear();
            h.crop = [0.0, 0.0, 1.0, 1.0];
            if let Some(a) = annots.iter().find(|a| a.id == id) {
                h.annot = Some(AnnotMeta {
                    id: a.id.clone(),
                    doc: a.doc.clone(),
                    page: a.page,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: &'static str, n: u32) -> WireHit {
        WireHit {
            kind,
            score: 0.0,
            rel: 0.0,
            doc: "d".into(),
            page: n,
            idx: 0,
            img: String::new(),
            snippet: Vec::new(),
            boxes: Vec::new(),
            crop: [0.0, 0.0, 1.0, 1.0],
            card: None,
            annot: None,
        }
    }

    #[test]
    fn blend_interleaves_instead_of_appending() {
        let text: Vec<WireHit> = (0..20).map(|n| hit("text", n)).collect();
        let images: Vec<WireHit> = (0..6).map(|n| hit("image", n)).collect();
        let merged = blend(text, images);

        assert_eq!(merged.len(), 26);
        // every image trailing all 20 text hits is exactly the bug being fixed
        let last_image_pos = merged.iter().rposition(|h| h.kind == "image").unwrap();
        assert!(
            last_image_pos < merged.len() - 1,
            "an image should not be pinned last"
        );
        let first_image_pos = merged.iter().position(|h| h.kind == "image").unwrap();
        assert!(
            first_image_pos < 5,
            "the image preference should surface one early"
        );
        // still interleaved, not images-then-text or text-then-images
        assert!(merged[..10].iter().any(|h| h.kind == "text"));
        assert!(merged[..10].iter().any(|h| h.kind == "image"));
    }

    #[test]
    fn blend_prefix_is_stable_under_longer_text_list() {
        // infinite scroll slices the blended order at growing offsets across
        // separate requests with growing k — that only tiles cleanly if
        // extending the text list never reorders the existing prefix
        let short = blend(
            (0..20).map(|n| hit("text", n)).collect(),
            (0..6).map(|n| hit("image", n)).collect(),
        );
        let long = blend(
            (0..40).map(|n| hit("text", n)).collect(),
            (0..6).map(|n| hit("image", n)).collect(),
        );
        for (i, (a, b)) in short.iter().zip(&long).enumerate() {
            assert_eq!(
                (a.kind, a.page),
                (b.kind, b.page),
                "prefix diverged at rank {i}"
            );
        }
    }

    #[test]
    fn blend_deals_images_at_jittered_cadence() {
        let merged = blend(
            (0..100).map(|n| hit("text", n)).collect(),
            (0..30).map(|n| hit("image", n)).collect(),
        );
        assert_eq!(merged.len(), 130);
        let slots: Vec<usize> = merged
            .iter()
            .enumerate()
            .filter(|(_, h)| h.kind == "image")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(slots.len(), 30);
        // every image lands within jitter range of its cadence position,
        // in list order (kept by the strictly-increasing slot function)
        for (i, (slot, h)) in slots
            .iter()
            .zip(merged.iter().filter(|h| h.kind == "image"))
            .enumerate()
        {
            assert_eq!(*slot, img_slot(i, h), "image {i}");
            assert!(
                *slot >= i * IMG_CADENCE && *slot < (i + 1) * IMG_CADENCE,
                "image {i} at {slot}"
            );
        }
        // the jitter actually varies — a fixed every-Nth pattern is the bug
        let gaps: std::collections::HashSet<usize> =
            slots.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.len() > 1, "cadence gaps all identical: {gaps:?}");
        // figures reach deep into the stream instead of packing page 1
        assert!(*slots.last().unwrap() >= 29 * IMG_CADENCE);
    }

    #[test]
    fn blend_handles_one_empty_side() {
        let text: Vec<WireHit> = (0..5).map(|n| hit("text", n)).collect();
        assert_eq!(blend(text.into_iter().collect(), Vec::new()).len(), 5);
        let images: Vec<WireHit> = (0..3).map(|n| hit("image", n)).collect();
        assert_eq!(blend(Vec::new(), images).len(), 3);
    }
}
