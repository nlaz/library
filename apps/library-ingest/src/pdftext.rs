//! Embedded PDF text layers, read via PDFKit. Born-digital PDFs (and scans
//! whose producer already ran OCR) carry per-glyph text; extracting it is
//! orders of magnitude faster than re-OCR'ing a render, and more accurate.
//! Output matches the OCR contract exactly — normalized top-left-origin
//! word boxes — so downstream never knows which path produced a page.
//! Pages that fail the gate here fall back to Vision OCR in `ocr.rs`.

use std::path::Path;

use library_core::Word;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSRange, NSString, NSURL};
use objc2_pdf_kit::{PDFDisplayBox, PDFDocument};

use crate::ocr::{round5, split_tokens, utf16_len};

/// A page counts as having a text layer when its string holds at least this
/// many non-whitespace chars. Below that it's an image-only page (or noise
/// like a printed page number) and Vision does a better job.
pub const TEXT_LAYER_MIN_CHARS: usize = 50;

pub struct TextLayer {
    doc: Retained<PDFDocument>,
}

impl TextLayer {
    /// `None` when PDFKit cannot open the file — never fatal, the caller
    /// just OCRs every page.
    pub fn open(pdf: &Path) -> Option<TextLayer> {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&pdf.to_string_lossy()));
        let doc = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }?;
        Some(TextLayer { doc })
    }

    /// Words with boxes for 1-based page `i`, or `None` when the page has
    /// no usable text layer and should be OCR'd instead.
    pub fn words(&self, i: usize) -> Option<Vec<Word>> {
        let page = unsafe { self.doc.pageAtIndex(i - 1) }?;
        let text = unsafe { page.string() }?.to_string();
        if text.chars().filter(|c| !c.is_whitespace()).count() < TEXT_LAYER_MIN_CHARS {
            return None;
        }
        let media = unsafe { page.boundsForBox(PDFDisplayBox::MediaBox) };
        if media.size.width <= 0.0 || media.size.height <= 0.0 {
            return None;
        }
        let rot = unsafe { page.rotation() }.rem_euclid(360);

        let mut words = Vec::new();
        let mut tokens = 0usize;
        for (loc, tok) in split_tokens(&text) {
            tokens += 1;
            let range = NSRange::new(utf16_len(&text[..loc]), utf16_len(tok));
            let Some(sel) = (unsafe { page.selectionForRange(range) }) else {
                continue;
            };
            let b = unsafe { sel.boundsForPage(&page) };
            if b.size.width <= 0.0 || b.size.height <= 0.0 {
                continue;
            }
            // normalize in unrotated media-box space (bottom-left origin,
            // y-up), then map into the rendered page's top-left space
            let u0 = ((b.origin.x - media.origin.x) / media.size.width) as f32;
            let v0 = ((b.origin.y - media.origin.y) / media.size.height) as f32;
            let u1 = ((b.origin.x + b.size.width - media.origin.x) / media.size.width) as f32;
            let v1 = ((b.origin.y + b.size.height - media.origin.y) / media.size.height) as f32;
            let (x0, y0) = display(u0, v0, rot);
            let (x1, y1) = display(u1, v1, rot);
            words.push(Word {
                t: tok.to_string(),
                x: round5(x0.min(x1) as f64),
                y: round5(y0.min(y1) as f64),
                w: round5((x0 - x1).abs() as f64),
                h: round5((y0 - y1).abs() as f64),
            });
        }
        // selections failing wholesale means the layer is broken — OCR it
        (!words.is_empty() && words.len() * 2 >= tokens).then_some(words)
    }
}

/// Map a normalized point from unrotated PDF space (origin bottom-left,
/// y-up) into display space (origin top-left, y-down) for a page whose
/// /Rotate is `rot` — the same orientation `render_page`'s drawing
/// transform produces.
fn display(u: f32, v: f32, rot: isize) -> (f32, f32) {
    match rot {
        90 => (v, u),
        180 => (1.0 - u, v),
        270 => (1.0 - v, 1.0 - u),
        _ => (u, 1.0 - v),
    }
}
