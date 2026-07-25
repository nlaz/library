import { describe, expect, it } from "vitest";
import {
  SNAP_DIST,
  dragBox,
  lineBoxes,
  medianLineHeight,
  nearestWord,
  negligible,
  selectionText,
  updateDrag,
} from "./selection";
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

// three printed lines, three words each, line height 0.02
const page: OcrWord[] = [0.2, 0.24, 0.28].flatMap((y) =>
  [0.1, 0.16, 0.22].map((x) => w(`w${x}${y}`, x, y)),
);
const LINE_H = 0.02;

describe("nearestWord", () => {
  it("is zero inside a word and grows outside", () => {
    expect(nearestWord(page, 0.12, 0.21)).toEqual({ idx: 0, dist: 0 });
    const off = nearestWord(page, 0.27 + 0.05, 0.29)!;
    expect(off.idx).toBe(8);
    expect(off.dist).toBeCloseTo(0.05);
  });

  it("returns null for empty pages", () => {
    expect(nearestWord([], 0.5, 0.5)).toBeNull();
  });
});

describe("medianLineHeight", () => {
  it("reads the page's printed lines", () => {
    expect(medianLineHeight(page)).toBeCloseTo(LINE_H);
  });

  it("falls back on empty pages", () => {
    expect(medianLineHeight([])).toBeCloseTo(0.02);
  });
});

describe("updateDrag", () => {
  const textAnchor = { x: 0.12, y: 0.21, word: 0 };

  it("region-anchored gestures stay regions for life", () => {
    const anchor = { x: 0.6, y: 0.5, word: null };
    const up = updateDrag(page, anchor, { x: 0.12, y: 0.21 }, "region", LINE_H);
    expect(up.mode).toBe("region"); // even directly over a word
  });

  it("no-OCR pages are region-only", () => {
    const up = updateDrag(null, textAnchor, { x: 0.3, y: 0.3 }, "text", LINE_H);
    expect(up.mode).toBe("region");
  });

  it("tracks words across lines while the drag fits the text", () => {
    const up = updateDrag(page, textAnchor, { x: 0.24, y: 0.29 }, "text", LINE_H);
    expect(up).toMatchObject({ mode: "text", w0: 0, w1: 9 });
    if (up.mode === "text") expect(up.boxes.length).toBe(3);
  });

  it("escapes to region when the cursor strays from the words", () => {
    const up = updateDrag(page, textAnchor, { x: 0.9, y: 0.24 }, "text", LINE_H);
    expect(up.mode).toBe("region");
    if (up.mode === "region") expect(up.bbox[0]).toBeCloseTo(0.12);
  });

  it("escapes by vertical overshoot even while near words", () => {
    // small print: the distance floor would keep this text, the
    // band-overshoot rule converts it
    const smallH = 0.008;
    const cur = { x: 0.12, y: 0.32 }; // 0.02 below the last line
    const up = updateDrag(page, textAnchor, cur, "text", smallH);
    expect(up.mode).toBe("region");
  });

  it("returns to text only well inside the bounds (hysteresis)", () => {
    // 0.02 below the last line: too far to return, not far enough to escape
    const between = { x: 0.12, y: 0.28 + LINE_H + 0.02 };
    expect(updateDrag(page, textAnchor, between, "text", LINE_H).mode).toBe("text");
    expect(updateDrag(page, textAnchor, between, "region", LINE_H).mode).toBe("region");
    // back on the words: converts back
    const on = { x: 0.17, y: 0.25 };
    expect(updateDrag(page, textAnchor, on, "region", LINE_H).mode).toBe("text");
    expect(SNAP_DIST).toBeLessThan(0.03); // return strictly tighter than escape
  });
});
