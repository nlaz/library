import { describe, expect, it } from "vitest";
import { findPoint, fitView, hitTest, toScreen, trailFor } from "./atlas-model";
import type { Atlas, AtlasPoint } from "./types";

const pt = (x: number, y: number, extra: Partial<AtlasPoint> = {}): AtlasPoint => ({
  x,
  y,
  d: 0,
  c: -1,
  e: 0,
  p: 1,
  s: "",
  ...extra,
});

describe("fitView", () => {
  it("stretches each axis to fill the padded frame, centered", () => {
    const pts = [pt(0, 0), pt(2, 1)];
    const v = fitView(pts, 100, 100, 10);
    const [x0, y0] = toScreen(v, 0, 0);
    const [x1, y1] = toScreen(v, 2, 1);
    // every dot inside the padding — the viewport is locked
    expect(x0).toBeCloseTo(10);
    expect(x1).toBeCloseTo(90);
    expect(y0).toBeCloseTo(10);
    expect(y1).toBeCloseTo(90);
    // centered: margins equal on each axis
    expect(x0).toBeCloseTo(100 - x1);
    expect(y0).toBeCloseTo(100 - y1);
  });

  it("handles a degenerate single-point cloud", () => {
    const v = fitView([pt(5, 5)], 100, 80, 10);
    const [sx, sy] = toScreen(v, 5, 5);
    expect(sx).toBeCloseTo(50);
    expect(sy).toBeCloseTo(40);
  });

  it("handles an empty cloud", () => {
    const v = fitView([], 100, 80, 10);
    expect(v).toEqual({ sx: 1, sy: 1, tx: 50, ty: 40 });
  });
});

describe("hitTest", () => {
  const pts = [pt(0, 0), pt(10, 0), pt(0, 10)];
  const v = { sx: 1, sy: 1, tx: 0, ty: 0 };

  it("picks the nearest point inside the radius", () => {
    expect(hitTest(pts, v, 9, 1, 5)).toBe(1);
    expect(hitTest(pts, v, 1, 1, 5)).toBe(0);
  });

  it("returns null outside the radius", () => {
    expect(hitTest(pts, v, 50, 50, 5)).toBeNull();
  });

  it("honors the filter", () => {
    expect(hitTest(pts, v, 9, 1, 20, (p) => p.x === 0)).toBe(0);
  });
});

describe("trail lookup", () => {
  const atlas = {
    trails: [{ c: 3, steps: [] }],
  } as unknown as Atlas;

  it("finds the trail for a theme and null otherwise", () => {
    expect(trailFor(atlas, 3)?.c).toBe(3);
    expect(trailFor(atlas, 4)).toBeNull();
  });
});

describe("findPoint", () => {
  const pts = [
    pt(0, 0, { d: 1, p: 5, c: -1 }),
    pt(1, 0, { d: 1, p: 5, c: 3 }),
    pt(2, 0, { d: 2, p: 7, c: -1 }),
  ];

  it("prefers the in-theme dot when a page has several", () => {
    expect(findPoint(pts, 1, 5, 3)).toBe(1);
  });

  it("falls back to any dot on the page", () => {
    expect(findPoint(pts, 2, 7, 3)).toBe(2);
  });

  it("returns null for an unsampled page", () => {
    expect(findPoint(pts, 9, 9, 3)).toBeNull();
  });
});
