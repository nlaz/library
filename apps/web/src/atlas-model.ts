// Pure geometry/lookup for the atlas view — no DOM, so the math is testable
// without standing up the canvas (the perf-fmt.ts pattern).

import type { Atlas, AtlasPoint, AtlasTrail } from "./types";

/** Per-axis scale + translate: screen = (x * sx + tx, y * sy + ty). */
export type View = { sx: number; sy: number; tx: number; ty: number };

/** Fit the point cloud into w×h with `pad` px margins. Axes scale
 * independently — PCA axes are abstract, so stretching to fill the frame
 * costs nothing and spreads the clusters as far as the viewport allows.
 * The viewport is locked: every dot is always inside the view. A
 * degenerate axis (all values equal) lands centered. */
export function fitView(pts: { x: number; y: number }[], w: number, h: number, pad: number): View {
  if (pts.length === 0) return { sx: 1, sy: 1, tx: w / 2, ty: h / 2 };
  let x0 = Infinity;
  let x1 = -Infinity;
  let y0 = Infinity;
  let y1 = -Infinity;
  for (const p of pts) {
    if (p.x < x0) x0 = p.x;
    if (p.x > x1) x1 = p.x;
    if (p.y < y0) y0 = p.y;
    if (p.y > y1) y1 = p.y;
  }
  const sx = x1 === x0 ? 1 : (w - 2 * pad) / (x1 - x0);
  const sy = y1 === y0 ? 1 : (h - 2 * pad) / (y1 - y0);
  return {
    sx,
    sy,
    tx: w / 2 - sx * (x0 + (x1 - x0) / 2),
    ty: h / 2 - sy * (y0 + (y1 - y0) / 2),
  };
}

export function toScreen(v: View, x: number, y: number): [number, number] {
  return [x * v.sx + v.tx, y * v.sy + v.ty];
}

/** Index of the point nearest (px, py) within `rMax` px, honoring an
 * optional filter; null when nothing is close enough. Linear scan — the
 * sampled cloud is ≤ ~20k points, fine per mousemove. */
export function hitTest(
  pts: AtlasPoint[],
  v: View,
  px: number,
  py: number,
  rMax: number,
  filter?: (p: AtlasPoint) => boolean,
): number | null {
  let best = -1;
  let bd = rMax * rMax;
  for (let i = 0; i < pts.length; i++) {
    const p = pts[i];
    if (filter && !filter(p)) continue;
    const sx = p.x * v.sx + v.tx - px;
    const sy = p.y * v.sy + v.ty - py;
    const d2 = sx * sx + sy * sy;
    if (d2 < bd) {
      bd = d2;
      best = i;
    }
  }
  return best === -1 ? null : best;
}

export function trailFor(atlas: Atlas, themeId: number): AtlasTrail | null {
  return atlas.trails.find((t) => t.c === themeId) ?? null;
}

/** Map point matching a trail step's (doc, page); prefers a point in the
 * step's theme when several chunks share the page. Null when the sampled
 * cloud has no dot for that page (an off-theme fallback step). */
export function findPoint(pts: AtlasPoint[], d: number, p: number, theme: number): number | null {
  let fallback: number | null = null;
  for (let i = 0; i < pts.length; i++) {
    if (pts[i].d !== d || pts[i].p !== p) continue;
    if (pts[i].c === theme) return i;
    fallback = fallback ?? i;
  }
  return fallback;
}
