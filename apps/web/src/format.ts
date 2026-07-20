// Pure display/mapping helpers — no DOM access, no imports of other
// modules' state, so everything here is directly unit-testable.

import type { DocInfo, IngestEvent, WireHit } from "./types";

// The one piece of shared data these helpers close over: the known-docs
// list. It is owned HERE (set by home rendering / drawer refreshes, read by
// docTitle and pagesOf) so no two modules hold their own copy.
let docList: DocInfo[] = [];

export function getDocList(): DocInfo[] {
  return docList;
}

export function setDocList(ds: DocInfo[]) {
  docList = ds;
}

export function prettify(id: string): string {
  return id
    .split("-")
    .map((w) => (w.length > 2 ? w[0].toUpperCase() + w.slice(1) : w))
    .join(" ");
}

export function displayTitle(d: DocInfo): string {
  return d.title ?? prettify(d.id);
}

/** The doc's display name (its title, or a prettified id), for any doc id
 * shown in the UI — search results never show the raw file name. */
export function docTitle(id: string): string {
  const d = docList.find((x) => x.id === id);
  return d ? displayTitle(d) : prettify(id);
}

// cross-page dedup: each continuation is its own fjall snapshot, so a
// mid-ingest index change can drift the slices slightly
export const hitKey = (h: WireHit) => `${h.kind}|${h.doc}|${h.page}|${h.idx}`;

export const STAGE_LABEL: Record<string, string> = {
  ocr: "reading pages",
  clean: "cleaning text",
  embed: "indexing text",
  figures: "finding figures",
  clip: "indexing figures",
  indexing: "committing",
  queued: "queued",
  staged: "waiting to index",
};

/** Progress-shaped view of a persisted status, for cards with no live
 * event yet (e.g. right after launch while the doc is mid-ingest). */
export function statusEvent(d: DocInfo): IngestEvent | undefined {
  const s = d.status;
  if (!s) return undefined;
  const stage = s.state === "preparing" ? (s.stage ?? "queued") : s.state;
  return { doc: d.id, stage: stage as IngestEvent["stage"], done: s.done, total: s.total, message: "" };
}
