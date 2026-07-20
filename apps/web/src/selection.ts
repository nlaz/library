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
