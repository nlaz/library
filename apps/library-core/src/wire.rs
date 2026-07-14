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

#[derive(Serialize)]
pub struct WireHit {
    /// "text" | "image"
    pub kind: &'static str,
    pub score: f32,
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
            }
        })
        .collect()
}

/// Text hits (RRF over lexical/semantic) and image hits (CLIP cosine) live
/// on unrelated score scales, so blending by raw `score` would just let
/// whichever scale happens to run bigger dominate the order. Both lists
/// arrive best-first, so blend by each hit's rank within its own list
/// instead — a reciprocal-rank curve, like RRF but per-modality — and give
/// images a slight edge so a handful of strong figures land among the top
/// text hits rather than trailing after all of them.
const IMAGE_PREFERENCE: f32 = 1.3;

pub fn blend(text: Vec<WireHit>, images: Vec<WireHit>) -> Vec<WireHit> {
    let mut ranked: Vec<(f32, WireHit)> = text
        .into_iter()
        .enumerate()
        .map(|(rank, h)| (1.0 / (1.0 + rank as f32), h))
        .chain(
            images
                .into_iter()
                .enumerate()
                .map(|(rank, h)| (IMAGE_PREFERENCE / (1.0 + rank as f32), h)),
        )
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().map(|(_, h)| h).collect()
}

pub fn wire_hit(hit: &Hit, qtoks: &[String]) -> WireHit {
    let matched = |w: &Word| {
        tokenize(&w.t)
            .iter()
            .any(|t| qtoks.iter().any(|q| t.starts_with(q.as_str())))
    };

    let first = hit.words.iter().position(|w| matched(w)).unwrap_or(0);
    let lo = first.saturating_sub(12);
    let hi = (first + 18).min(hit.words.len());
    let snippet = hit.words[lo..hi]
        .iter()
        .map(|w| SnippetWord { t: w.t.clone(), m: matched(w) })
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
        doc: hit.key.doc.clone(),
        page: hit.key.page,
        idx: hit.key.idx,
        img: format!("/pages/{}/page-{:04}.jpg", hit.key.doc, hit.key.page),
        snippet,
        boxes,
        crop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: &'static str, n: u32) -> WireHit {
        WireHit {
            kind,
            score: 0.0,
            doc: "d".into(),
            page: n,
            idx: 0,
            img: String::new(),
            snippet: Vec::new(),
            boxes: Vec::new(),
            crop: [0.0, 0.0, 1.0, 1.0],
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
        assert!(last_image_pos < merged.len() - 1, "an image should not be pinned last");
        let first_image_pos = merged.iter().position(|h| h.kind == "image").unwrap();
        assert!(first_image_pos < 5, "the image preference should surface one early");
        // still interleaved, not images-then-text or text-then-images
        assert!(merged[..10].iter().any(|h| h.kind == "text"));
        assert!(merged[..10].iter().any(|h| h.kind == "image"));
    }

    #[test]
    fn blend_handles_one_empty_side() {
        let text: Vec<WireHit> = (0..5).map(|n| hit("text", n)).collect();
        assert_eq!(blend(text.into_iter().collect(), Vec::new()).len(), 5);
        let images: Vec<WireHit> = (0..3).map(|n| hit("image", n)).collect();
        assert_eq!(blend(Vec::new(), images).len(), 3);
    }
}
