// Pure notes logic: timeline ordering, backlink derivation, wiki-link
// tokenizing, the split-whisper boundary, and the ledger's filters and
// region-crop math. No DOM; vitest pins all of it.

import type { Box, CardRec } from "./types";

/** The notes view's order: a reverse-chronological journal by birth
 * stamp (edits don't reshuffle it; ties break on id so the order is
 * total), filed cards split out for the divider. */
export function timeline(cards: CardRec[]): { live: CardRec[]; filed: CardRec[] } {
  const sorted = [...cards].sort(
    (a, b) => b.created - a.created || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0),
  );
  return {
    live: sorted.filter((c) => !c.filed),
    filed: sorted.filter((c) => c.filed),
  };
}

/** Cards that point at `target`: typed links plus [[Title]] mentions. */
export function backlinks(cards: CardRec[], target: CardRec): CardRec[] {
  const mention = `[[${target.title}]]`;
  return cards.filter(
    (c) =>
      c.id !== target.id &&
      !c.filed &&
      (c.links.some((l) => l.to === target.id) || c.body.includes(mention)),
  );
}

export type WikiToken = { kind: "text"; text: string } | { kind: "link"; title: string };

/** Split a body into text and [[wiki-link]] tokens for rendering. */
export function wikiTokens(body: string): WikiToken[] {
  const out: WikiToken[] = [];
  const re = /\[\[([^\][]+)\]\]/g;
  let last = 0;
  for (let m = re.exec(body); m; m = re.exec(body)) {
    if (m.index > last) out.push({ kind: "text", text: body.slice(last, m.index) });
    out.push({ kind: "link", title: m[1] });
    last = m.index + m[0].length;
  }
  if (last < body.length) out.push({ kind: "text", text: body.slice(last) });
  return out;
}

/** The split whisper's threshold: past this many words a card is
 * becoming an essay. */
export const SPLIT_WORDS = 150;

/** Where to cut an overlong body: the first sentence boundary at or after
 * SPLIT_WORDS words (falling back to the word boundary). Returns the char
 * index of the cut, or null when the body is still card-sized. */
export function splitPoint(body: string, limit = SPLIT_WORDS): number | null {
  const words = [...body.matchAll(/\S+/g)];
  if (words.length <= limit) return null;
  const from = words[limit - 1].index + words[limit - 1][0].length;
  const rest = body.slice(from);
  const m = rest.match(/[.!?]["')\]]?\s/);
  return m && m.index !== undefined ? from + m.index + m[0].length : from;
}

// ---------------------------------------------------------------------------
// the ledger's filters: a document rail (multi-select) crossed with the
// header's collection scope
// ---------------------------------------------------------------------------

/** Unique docs a card's evidence touches, in first-seen order. */
export function evidenceDocs(c: CardRec): string[] {
  const out: string[] = [];
  for (const q of c.evidence) if (!out.includes(q.doc)) out.push(q.doc);
  return out;
}

/** Rail rows: per-doc card counts, keyed in first-appearance order of the
 * cards passed in (callers pass the timeline's live half, so the rail
 * reads in the ledger's own order). */
export function docCounts(cards: CardRec[]): Map<string, number> {
  const out = new Map<string, number>();
  for (const c of cards) {
    for (const d of evidenceDocs(c)) out.set(d, (out.get(d) ?? 0) + 1);
  }
  return out;
}

/** The intersection filter: a card shows when it matches the collection
 * scope (null = no scope) AND the rail selection (empty = no selection).
 * Evidence-less cards belong to no document, so any active filter hides
 * them. */
export function cardShown(
  c: CardRec,
  colDocs: ReadonlySet<string> | null,
  sel: ReadonlySet<string>,
): boolean {
  const docs = evidenceDocs(c);
  if (colDocs && !docs.some((d) => colDocs.has(d))) return false;
  if (sel.size && !docs.some((d) => sel.has(d))) return false;
  return true;
}

/** A doc the collection scope hides can't stay selected — it would ghost-
 * filter the ledger from an invisible rail row. */
export function pruneSelection(
  sel: ReadonlySet<string>,
  visible: ReadonlySet<string>,
): Set<string> {
  return new Set([...sel].filter((d) => visible.has(d)));
}

// ---------------------------------------------------------------------------
// region crops: the page scan windowed to the mark's bbox with CSS alone
// ---------------------------------------------------------------------------

/** Percentages that window a page scan to a normalized [x, y, w, h] bbox
 * inside an overflow-hidden container: the region's top-left lands on the
 * container's, at full region width. Exact without knowing the scan's
 * pixel size — left/width are % of container width, top is % of container
 * height. Null for degenerate boxes. */
export function cropCSS(b: Box): { width: string; left: string; top: string } | null {
  const [x, y, w, h] = b;
  if (!(w > 0) || !(h > 0)) return null;
  const pct = (v: number) => `${((v * 1000) | 0) / 10}%`;
  return { width: pct(1 / w), left: pct(-x / w), top: pct(-y / h) };
}

/** The sheet never demands a title: an explicit one wins, else the entry's
 * first line stands in. */
export function impliedTitle(title: string, body: string): string {
  const t = title.trim();
  if (t) return t;
  const first = body.trim().split("\n")[0].trim();
  return first.length > 80 ? `${first.slice(0, 79)}…` : first;
}

export function fmtStamp(secs: number): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  return d
    .toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: sameYear ? undefined : "numeric",
    })
    .toLowerCase();
}
