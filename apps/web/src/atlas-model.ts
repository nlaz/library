// Pure geometry/lookup for the atlas view — no DOM, so the math is testable
// without standing up the canvas (the perf-fmt.ts pattern).

import type { Atlas, AtlasPoint, AtlasTrail } from "./types";

/** Uniform scale + translate: screen = (x * s + tx, y * s + ty). */
export type View = { s: number; tx: number; ty: number };

/** Fit the point cloud into w×h with `pad` px margins, preserving aspect
 * ratio and centering. A degenerate cloud (one point, or all coincident)
 * lands centered at scale 1. */
export function fitView(pts: { x: number; y: number }[], w: number, h: number, pad: number): View {
  if (pts.length === 0) return { s: 1, tx: w / 2, ty: h / 2 };
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
  const dx = x1 - x0;
  const dy = y1 - y0;
  const s = dx === 0 && dy === 0 ? 1 : Math.min((w - 2 * pad) / (dx || 1e-9), (h - 2 * pad) / (dy || 1e-9));
  return {
    s,
    tx: w / 2 - s * (x0 + dx / 2),
    ty: h / 2 - s * (y0 + dy / 2),
  };
}

export function toScreen(v: View, x: number, y: number): [number, number] {
  return [x * v.s + v.tx, y * v.s + v.ty];
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
    const sx = p.x * v.s + v.tx - px;
    const sy = p.y * v.s + v.ty - py;
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
