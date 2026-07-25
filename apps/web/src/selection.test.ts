import { describe, expect, it } from "vitest";
import { dragBox, lineBoxes, negligible, selectionText } from "./selection";
import type { OcrWord } from "./types";

const w = (t: string, x: number, y: number, wd = 0.05, h = 0.02): OcrWord => ({
  t,
  x,
  y,
  w: wd,
  h,
});

describe("lineBoxes", () => {
  it("merges one line of words into one box", () => {
    const boxes = lineBoxes([w("an", 0.1, 0.2), w("hundred", 0.16, 0.2), w("twenty", 0.28, 0.2)]);
    expect(boxes).toEqual([[0.1, 0.2, expect.closeTo(0.23, 5), expect.closeTo(0.02, 5)]]);
  });

  it("splits on vertical movement (line wrap)", () => {
    const boxes = lineBoxes([
      w("end", 0.7, 0.2),
      w("of", 0.76, 0.2),
      w("line", 0.1, 0.23), // next printed line
      w("two", 0.16, 0.23),
    ]);
    expect(boxes.length).toBe(2);
    expect(boxes[0][1]).toBeCloseTo(0.2);
    expect(boxes[1][1]).toBeCloseTo(0.23);
    expect(boxes[1][0]).toBeCloseTo(0.1);
  });

  it("tolerates slight baseline jitter within a line", () => {
    const boxes = lineBoxes([w("a", 0.1, 0.2), w("b", 0.16, 0.205), w("c", 0.22, 0.198)]);
    expect(boxes.length).toBe(1);
  });

  it("handles empty input", () => {
    expect(lineBoxes([])).toEqual([]);
  });
});

describe("selectionText", () => {
  it("joins with single spaces", () => {
    expect(selectionText([w("an", 0, 0), w("hundred", 0, 0)])).toBe("an hundred");
  });
});

describe("dragBox", () => {
  it("normalizes corner order", () => {
    expect(dragBox(0.5, 0.6, 0.2, 0.3)).toEqual([
      0.2,
      0.3,
      expect.closeTo(0.3, 5),
      expect.closeTo(0.3, 5),
    ]);
  });

  it("clamps to the page", () => {
    const b = dragBox(-0.2, 0.5, 1.4, 1.2);
    expect(b[0]).toBe(0);
    expect(b[0] + b[2]).toBe(1);
    expect(b[1] + b[3]).toBe(1);
  });

  it("flags aborted clicks", () => {
    expect(negligible(dragBox(0.5, 0.5, 0.502, 0.6))).toBe(true);
    expect(negligible(dragBox(0.2, 0.2, 0.5, 0.5))).toBe(false);
  });
});
