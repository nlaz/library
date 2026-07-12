//! Figure subdivision: composite figures (catalog grids, multi-figure plates)
//! embed as a blur of their parts under CLIP, so component queries miss. We
//! index the whole figure AND its parts:
//!
//! - tier 1: recursive XY-cut at whitespace valleys in the darkness profile
//! - tier 2: overlapping tiles, only for large figures with no valleys
//!   (full-bleed photos, collages)
//!
//! The server groups hits per page afterwards, so parts never spam results.

use image::DynamicImage;
use library_core::Bbox;

/// Figures below this fraction of the page stay whole (single diagrams).
pub const SPLIT_MIN_AREA: f32 = 0.25;
/// Tiles only for unsplittable figures at least this large.
pub const TILE_MIN_AREA: f32 = 0.30;
/// Parts smaller than this fraction of the page are noise for CLIP.
pub const PART_MIN_AREA: f32 = 0.04;
/// ...as are parts thinner than this many source pixels.
pub const PART_MIN_PX: f32 = 120.0;

/// Profile within this fraction of the (max-min) range above the baseline
/// counts as blank — relative to baseline, so a uniform gray background
/// (catalog spreads) doesn't lift gutters above the threshold.
const VALLEY_FRAC: f32 = 0.12;
/// A blank run must span this fraction of the axis to be a cut.
const VALLEY_MIN_RUN: f32 = 0.015;
const MAX_DEPTH: u32 = 2;

/// Component parts of a figure (page-normalized bboxes). Does not include
/// `bbox` itself; may be empty for simple figures.
pub fn subdivide(page: &DynamicImage, bbox: Bbox) -> Vec<Bbox> {
    if bbox[2] * bbox[3] < SPLIT_MIN_AREA {
        return Vec::new();
    }
    let mut parts = Vec::new();
    split_rec(page, bbox, MAX_DEPTH, &mut parts);
    if parts.is_empty() && bbox[2] * bbox[3] >= TILE_MIN_AREA {
        // tier 2: 2x2 overlapping tiles (60% size, 40% stride)
        for oy in [0.0f32, 0.4] {
            for ox in [0.0f32, 0.4] {
                parts.push([
                    bbox[0] + ox * bbox[2],
                    bbox[1] + oy * bbox[3],
                    0.6 * bbox[2],
                    0.6 * bbox[3],
                ]);
            }
        }
    }
    let (iw, ih) = (page.width() as f32, page.height() as f32);
    parts.retain(|b| {
        b[2] * b[3] >= PART_MIN_AREA && b[2] * iw >= PART_MIN_PX && b[3] * ih >= PART_MIN_PX
    });
    parts
}

fn split_rec(page: &DynamicImage, bbox: Bbox, depth: u32, out: &mut Vec<Bbox>) {
    if depth == 0 {
        return;
    }
    for cell in split_once(page, bbox) {
        out.push(cell);
        split_rec(page, cell, depth - 1, out);
    }
}

/// One XY-cut: split `bbox` along the axis with the most whitespace valleys.
/// Empty when no interior valley exists.
fn split_once(page: &DynamicImage, bbox: Bbox) -> Vec<Bbox> {
    let (iw, ih) = (page.width() as f32, page.height() as f32);
    let crop = page.crop_imm(
        (bbox[0] * iw) as u32,
        (bbox[1] * ih) as u32,
        ((bbox[2] * iw) as u32).max(1),
        ((bbox[3] * ih) as u32).max(1),
    );
    let small = crop.thumbnail(512, 512).into_luma8();
    let (w, h) = (small.width() as usize, small.height() as usize);
    if w < 8 || h < 8 {
        return Vec::new();
    }

    let mut cols = vec![0f32; w];
    let mut rows = vec![0f32; h];
    for (x, y, p) in small.enumerate_pixels() {
        let d = (255 - p.0[0]) as f32;
        cols[x as usize] += d;
        rows[y as usize] += d;
    }
    for c in cols.iter_mut() {
        *c /= h as f32;
    }
    for r in rows.iter_mut() {
        *r /= w as f32;
    }

    let col_cuts = valley_cuts(&cols);
    let row_cuts = valley_cuts(&rows);

    if !col_cuts.is_empty() && col_cuts.len() >= row_cuts.len() {
        segments(&col_cuts, w)
            .into_iter()
            .map(|(a, b)| {
                [
                    bbox[0] + (a as f32 / w as f32) * bbox[2],
                    bbox[1],
                    ((b - a) as f32 / w as f32) * bbox[2],
                    bbox[3],
                ]
            })
            .collect()
    } else if !row_cuts.is_empty() {
        segments(&row_cuts, h)
            .into_iter()
            .map(|(a, b)| {
                [
                    bbox[0],
                    bbox[1] + (a as f32 / h as f32) * bbox[3],
                    bbox[2],
                    ((b - a) as f32 / h as f32) * bbox[3],
                ]
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Centers of interior blank runs in a darkness profile. Runs touching either
/// edge are margins, not cuts. The profile is smoothed (absorbs 1px cell
/// borders) and thresholded relative to its own baseline (immune to uniform
/// gray backgrounds).
fn valley_cuts(profile: &[f32]) -> Vec<usize> {
    let n = profile.len();
    // moving average, window ~1% of the axis
    let w = (n / 100).max(1);
    let mut smooth = Vec::with_capacity(n);
    for i in 0..n {
        let lo = i.saturating_sub(w);
        let hi = (i + w + 1).min(n);
        smooth.push(profile[lo..hi].iter().sum::<f32>() / (hi - lo) as f32);
    }

    let max = smooth.iter().cloned().fold(f32::MIN, f32::max);
    let min = smooth.iter().cloned().fold(f32::MAX, f32::min);
    if max - min <= 1.0 {
        return Vec::new(); // featureless (blank or uniformly dark)
    }
    let thr = min + VALLEY_FRAC * (max - min);
    let min_run = (VALLEY_MIN_RUN * n as f32).ceil() as usize;
    let mut cuts = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &v) in smooth.iter().enumerate() {
        if v < thr {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            if i - s >= min_run && s > 0 {
                cuts.push((s + i) / 2);
            }
        }
    }
    // an unclosed run reaches the trailing edge: margin, not a cut
    cuts
}

fn segments(cuts: &[usize], len: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(cuts.len() + 1);
    let mut a = 0usize;
    for &c in cuts {
        out.push((a, c));
        a = c;
    }
    out.push((a, len));
    out
}
