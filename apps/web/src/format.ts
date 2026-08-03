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

/** What to call a book: the title the user gave it, else the name of its
 * file, else the id.
 *
 * The id is the last resort and used to be the first: ids were slugs made
 * from the filename, so prettifying one read like a title. They are minted
 * now — opaque, so a Finder rename can't orphan a book — which turned every
 * untitled book on the shelf into `D01713FA82AD0`. The filename was in the
 * database the whole time; it just wasn't asked for.
 *
 * The name is shown verbatim, because it arrives already cleaned:
 * `library-core::naming` drops the download tags and expands the slugs, and
 * it is the side that knows a filename from a slug. `prettify` here is only
 * the *slug* rule — it reads every hyphen as a word separator, which is
 * right for an old id and wrong for a filename (`Julia Child - Mastering…`
 * would go to a gap), so it is applied to the id and nothing else. */
export function displayTitle(d: DocInfo): string {
  return d.title ?? d.name ?? prettify(d.id);
}

/** The doc's display name, for any doc id shown in the UI. */
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
  download: "fetching the figure model",
  indexing: "committing",
  queued: "queued",
  staged: "waiting to index",
};

/** Stages whose done/total are bytes rather than pages or records. The
 * one-time model fetch is the only one — see `Progress::Download`. */
const BYTE_STAGES = new Set(["download"]);

/** The count that follows a stage label on a book card: "12/300" for pages,
 * "84/335 MB" for the model fetch, nothing at all for a stage with no
 * denominator to report. */
export function stageCount(stage: string, done: number, total: number): string {
  if (total <= 0) return "";
  if (!BYTE_STAGES.has(stage)) return ` ${done}/${total}`;
  const mb = (b: number) => Math.round(b / (1024 * 1024));
  return ` ${mb(done)}/${mb(total)} MB`;
}

/** Progress-shaped view of a persisted status, for cards with no live
 * event yet (e.g. right after launch while the doc is mid-ingest). */
export function statusEvent(d: DocInfo): IngestEvent | undefined {
  const s = d.status;
  if (!s) return undefined;
  const stage = s.state === "preparing" ? (s.stage ?? "queued") : s.state;
  return { doc: d.id, stage: stage as IngestEvent["stage"], done: s.done, total: s.total, message: "" };
}
