// Pure geometry for the pen: turning a run of OCR word boxes (a text
// selection over the reader's text layer) into per-line highlight boxes,
// and a drag gesture into a normalized region bbox. No DOM in here — the
// vitest suite pins the math.

import type { Box, OcrWord } from "./types";

const clamp01 = (v: number) => Math.min(Math.max(v, 0), 1);

/** Merge consecutive word boxes into one box per printed line. A word
 * joins the running line while their vertical bands still overlap by at
 * least half the smaller height; otherwise it starts a new line. */
export function lineBoxes(words: OcrWord[]): Box[] {
  const out: Box[] = [];
  let cur: { x0: number; y0: number; x1: number; y1: number } | null = null;
  const flush = () => {
    if (cur) out.push([cur.x0, cur.y0, cur.x1 - cur.x0, cur.y1 - cur.y0]);
  };
  for (const w of words) {
    const overlap = cur
      ? Math.min(cur.y1, w.y + w.h) - Math.max(cur.y0, w.y) >=
        0.5 * Math.min(cur.y1 - cur.y0, w.h)
      : false;
    if (cur && overlap) {
      cur.x0 = Math.min(cur.x0, w.x);
      cur.y0 = Math.min(cur.y0, w.y);
      cur.x1 = Math.max(cur.x1, w.x + w.w);
      cur.y1 = Math.max(cur.y1, w.y + w.h);
    } else {
      flush();
      cur = { x0: w.x, y0: w.y, x1: w.x + w.w, y1: w.y + w.h };
    }
  }
  flush();
  return out;
}

/** The snapshot text of a word run — what renders and searches later. */
export function selectionText(words: OcrWord[]): string {
  return words
    .map((w) => w.t)
    .join(" ")
    .trim();
}

/** Normalize a drag gesture (any corner order, may leave the page) into a
 * clamped [x, y, w, h] box. */
export function dragBox(ax: number, ay: number, bx: number, by: number): Box {
  const x0 = clamp01(Math.min(ax, bx));
  const y0 = clamp01(Math.min(ay, by));
  const x1 = clamp01(Math.max(ax, bx));
  const y1 = clamp01(Math.max(ay, by));
  return [x0, y0, x1 - x0, y1 - y0];
}

/** Too small to be a deliberate region — treat as an aborted click. */
export function negligible(b: Box): boolean {
  return b[2] < 0.01 || b[3] < 0.01;
}

// ---------------------------------------------------------------------------
// the smart drag: one gesture that starts as text selection and converts
// to region selection when it leaves the text — and back, with hysteresis
// ---------------------------------------------------------------------------

/** Fuzzy-lock radius: a press this close to a word anchors as text. */
export const SNAP_DIST = 0.015;
/** Floor for the text→region escape distance, for tiny line heights. */
export const ESCAPE_MIN = 0.03;
/** Escape threshold in multiples of the page's median line height. */
export const ESCAPE_LINES = 1.5;

export type DragUpdate =
  | { mode: "text"; w0: number; w1: number; boxes: Box[] } // w1 exclusive
  | { mode: "region"; bbox: Box };

/** Point→rect distance to the nearest OCR word; null when no words. */
export function nearestWord(
  words: OcrWord[],
  x: number,
  y: number,
): { idx: number; dist: number } | null {
  let best: { idx: number; dist: number } | null = null;
  for (let i = 0; i < words.length; i++) {
    const w = words[i];
    const dx = Math.max(w.x - x, 0, x - (w.x + w.w));
    const dy = Math.max(w.y - y, 0, y - (w.y + w.h));
    const dist = Math.hypot(dx, dy);
    if (!best || dist < best.dist) best = { idx: i, dist };
  }
  return best;
}

/** Median printed-line height on the page; fallback for empty pages. */
export function medianLineHeight(words: OcrWord[]): number {
  const heights = lineBoxes(words)
    .map((b) => b[3])
    .sort((a, b) => a - b);
  return heights.length ? heights[Math.floor(heights.length / 2)] : 0.02;
}

/** One step of the gesture state machine. `anchor.word` null means the
 * press missed the text — the gesture is a region for its whole life
 * (retro-fitting a word anchor would discard the user's corner). A
 * text-anchored gesture escapes to region when the cursor strays from
 * the words or the rect overshoots the selected lines' band, and only
 * returns when it comes back well inside both bounds — the return
 * thresholds are strictly tighter, so the boundary cannot flicker. */
export function updateDrag(
  words: OcrWord[] | null,
  anchor: { x: number; y: number; word: number | null },
  cur: { x: number; y: number },
  prevMode: "text" | "region",
  lineH: number,
): DragUpdate {
  const bbox = dragBox(anchor.x, anchor.y, cur.x, cur.y);
  if (anchor.word == null || !words?.length) return { mode: "region", bbox };

  const near = nearestWord(words, cur.x, cur.y);
  if (!near) return { mode: "region", bbox };
  const w0 = Math.min(anchor.word, near.idx);
  const w1 = Math.max(anchor.word, near.idx) + 1;
  const boxes = lineBoxes(words.slice(w0, w1));

  // vertical overshoot of the drag rect past the selected lines' band
  const bandY0 = Math.min(...boxes.map((b) => b[1]));
  const bandY1 = Math.max(...boxes.map((b) => b[1] + b[3]));
  const overshoot = Math.max(bandY0 - bbox[1], bbox[1] + bbox[3] - bandY1, 0);

  const keepText =
    prevMode === "text"
      ? near.dist <= Math.max(ESCAPE_MIN, ESCAPE_LINES * lineH) && overshoot <= ESCAPE_LINES * lineH
      : near.dist <= SNAP_DIST && overshoot < 0.5 * lineH;
  return keepText ? { mode: "text", w0, w1, boxes } : { mode: "region", bbox };
}
