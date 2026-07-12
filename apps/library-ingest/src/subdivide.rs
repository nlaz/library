//! Figure subdivision: composite figures (catalog grids, multi-figure plates)
//! embed as a blur of their parts under CLIP, so component queries miss. We
//! index the whole figure AND its parts:
//!
//! - tier 1: recursive XY-cut at whitespace valleys in the darkness profile
//! - tier 2: overlapping tiles, only for large figures with no valleys
//!   (full-bleed photos, collages)
//!
//! The server groups hits per page afterwards, so parts never spam results.

use image::GrayImage;
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
/// `bbox` itself; may be empty for simple figures. Darkness profiles read
/// `luma`, the page's shared grayscale downscale; `full` is the render's
/// pixel dimensions, which the part-size gates are calibrated against.
pub fn subdivide(luma: &GrayImage, full: (u32, u32), bbox: Bbox) -> Vec<Bbox> {
    if bbox[2] * bbox[3] < SPLIT_MIN_AREA {
        return Vec::new();
    }
    let mut parts = Vec::new();
    split_rec(luma, bbox, MAX_DEPTH, &mut parts);
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
    let (iw, ih) = (full.0 as f32, full.1 as f32);
    parts.retain(|b| {
        b[2] * b[3] >= PART_MIN_AREA && b[2] * iw >= PART_MIN_PX && b[3] * ih >= PART_MIN_PX
    });
    parts
}

fn split_rec(luma: &GrayImage, bbox: Bbox, depth: u32, out: &mut Vec<Bbox>) {
    if depth == 0 {
        return;
    }
    for cell in split_once(luma, bbox) {
        out.push(cell);
        split_rec(luma, cell, depth - 1, out);
    }
}

/// One XY-cut: split `bbox` along the axis with the most whitespace valleys.
/// Empty when no interior valley exists.
fn split_once(luma: &GrayImage, bbox: Bbox) -> Vec<Bbox> {
    let (lw, lh) = (luma.width(), luma.height());
    let x0 = ((bbox[0] * lw as f32) as u32).min(lw.saturating_sub(1));
    let y0 = ((bbox[1] * lh as f32) as u32).min(lh.saturating_sub(1));
    let w = ((bbox[2] * lw as f32) as u32).clamp(1, lw - x0) as usize;
    let h = ((bbox[3] * lh as f32) as u32).clamp(1, lh - y0) as usize;
    if w < 8 || h < 8 {
        return Vec::new();
    }

    let mut cols = vec![0f32; w];
    let mut rows = vec![0f32; h];
    for y in 0..h {
        for x in 0..w {
            let d = (255 - luma.get_pixel(x0 + x as u32, y0 + y as u32).0[0]) as f32;
            cols[x] += d;
            rows[y] += d;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: (u32, u32) = (1000, 800);

    /// A white page with dark rectangles (page-normalized boxes), downscaled
    /// the way `prepare_figures` feeds this module.
    fn page(blocks: &[Bbox]) -> GrayImage {
        let (w, h) = FULL;
        let mut img = image::GrayImage::from_pixel(w, h, image::Luma([255u8]));
        for b in blocks {
            let (x0, y0) = ((b[0] * w as f32) as u32, (b[1] * h as f32) as u32);
            let (x1, y1) = (((b[0] + b[2]) * w as f32) as u32, ((b[1] + b[3]) * h as f32) as u32);
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    img.put_pixel(x, y, image::Luma([0u8]));
                }
            }
        }
        image::DynamicImage::ImageLuma8(img)
            .thumbnail(crate::PAGE_LUMA_PX, crate::PAGE_LUMA_PX)
            .into_luma8()
    }

    #[test]
    fn splits_two_columns_at_the_gutter() {
        // two solid blocks with a 10%-wide gutter between them
        let luma = page(&[[0.05, 0.125, 0.40, 0.75], [0.55, 0.125, 0.40, 0.75]]);
        let parts = subdivide(&luma, FULL, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(parts.len(), 2, "one vertical cut -> two parts: {parts:?}");
        // cut lands inside the gutter
        assert!(parts[0][0] == 0.0 && (0.45..=0.55).contains(&parts[0][2]), "{parts:?}");
        assert!((0.45..=0.55).contains(&parts[1][0]), "{parts:?}");
        // parts span the bbox on the uncut axis
        assert!(parts.iter().all(|p| p[1] == 0.0 && p[3] == 1.0), "{parts:?}");
    }

    #[test]
    fn featureless_figure_gets_tiles() {
        // a uniformly dark full-page figure: no valleys -> 2x2 tile fallback
        let luma = page(&[[0.0, 0.0, 1.0, 1.0]]);
        let parts = subdivide(&luma, FULL, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(parts.len(), 4, "{parts:?}");
        assert!(parts.iter().all(|p| (p[2] - 0.6).abs() < 1e-4 && (p[3] - 0.6).abs() < 1e-4));
    }

    #[test]
    fn small_figures_stay_whole() {
        let luma = page(&[[0.1, 0.1, 0.3, 0.3]]);
        assert!(subdivide(&luma, FULL, [0.1, 0.1, 0.3, 0.3]).is_empty());
    }
}
