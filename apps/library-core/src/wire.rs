//! Wire types + hit shaping shared by every host (server, desktop app).
//!
//! A `WireHit` is what a client renders: the page image to show, the snippet,
//! the matched-word boxes, and a `crop` rect that lets the client zoom past
//! the scan's baked-in margins.

use serde::Serialize;

use crate::{Bbox, Hit, ImageHit, Word, tokenize};

pub type Collections = std::collections::BTreeMap<String, Vec<String>>;

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
