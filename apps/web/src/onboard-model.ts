// What the first-run panel should say, given what's in the library. Pure —
// the DOM half is onboard.ts.
//
// The panel is not a tour: every row is a true statement about the state of
// this library, and the rows advance because ingestion advanced. That is
// what makes it worth showing at all — it doubles as the explanation for why
// the first book takes minutes, which is the moment a new user would
// otherwise decide the app is broken.

import type { DocInfo } from "./types";

export type OnboardRow = {
  title: string;
  sub: string;
  state: "done" | "active" | "pending";
};

/** A doc that can actually be searched and read right now. `text_ready` counts:
 * its pages and text are in, only figures are still indexing. */
function searchable(d: DocInfo): boolean {
  return !d.processing && d.status?.state !== "failed";
}

/**
 * The three rows, or `null` once the library can answer a query — at which
 * point the shelves say everything the panel was there to say.
 */
export function onboardView(docs: DocInfo[]): OnboardRow[] | null {
  if (docs.some(searchable)) return null;
  const reading = docs.some((d) => d.processing);

  return [
    {
      title: "Drop a PDF in this window",
      sub: "Or press ⌘O. The file moves into your library folder.",
      state: reading ? "done" : "active",
    },
    {
      title: "It gets read",
      sub: "Scans go through OCR; pictures and tables are found and indexed. A few minutes for a book.",
      state: reading ? "active" : "pending",
    },
    {
      title: "Then search it, or ask it",
      sub: "⌘F finds words and pictures. Everything stays on this Mac.",
      state: "pending",
    },
  ];
}
