// Pure notes logic: timeline ordering, backlink derivation, wiki-link
// tokenizing, and the split-whisper boundary. No DOM; vitest pins all
// of it.

import type { CardRec } from "./types";

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
