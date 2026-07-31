// What the first-run panel should say, given what's in the library. Pure —
// the DOM half is onboard.ts.
//
// The panel is not a tour: it is what an empty library has to say for itself,
// and it names the three things that happen to a book so the minutes the
// first one takes are expected rather than alarming. It goes the moment a
// book lands on the shelf — from there the card itself reports its own
// progress bar, and a panel narrating what the shelf is already showing is
// just something in the way.

import type { DocInfo } from "./types";

export type OnboardRow = {
  title: string;
  sub: string;
  state: "done" | "active" | "pending";
};

/** A book that means this library has begun: added and not failed, whether or
 * not it has finished indexing. A failed one doesn't count — nothing came of
 * it, so the way in still has to be on screen. */
function started(d: DocInfo): boolean {
  return d.status?.state !== "failed";
}

/**
 * The three rows, or `null` once this library holds a book — at which point
 * the shelf, and the ingest bar on the card, say it better.
 */
export function onboardView(docs: DocInfo[]): OnboardRow[] | null {
  if (docs.some(started)) return null;

  return [
    {
      title: "Drop a PDF in this window",
      sub: "Or press ⌘O. The file moves into your library folder.",
      state: "active",
    },
    {
      title: "It gets read",
      sub: "Scans go through OCR; pictures and tables are found and indexed. A few minutes for a book.",
      state: "pending",
    },
    {
      title: "Then search it, or ask it",
      sub: "⌘F finds words and pictures. Everything stays on this Mac.",
      state: "pending",
    },
  ];
}
