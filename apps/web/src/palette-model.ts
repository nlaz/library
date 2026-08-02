// Matching and grouping for the card catalog (⌘K). Pure — no DOM, no module
// state, no imports of anyone else's — so the ranking can be argued with in
// a unit test instead of by squinting at a running palette.
//
// The matcher is a plain left-to-right subsequence. Something cleverer would
// score marginally better on contrived labels and would stop being
// predictable, which is the wrong trade for a box people type into blind:
// the same keystrokes must always select the same row.

export type Span = [start: number, end: number];

export type Item = {
  id: string;
  label: string;
  group: string;
  /** Right-hand mono metadata: a shelf and page count, a book count, a date. */
  meta?: string;
  /** The chord that does this without the catalog. Shown so the row teaches
   * its own shortcut and the user eventually stops needing the catalog. */
  chord?: string;
  /** What Enter does. Absent on a row that only drills in. */
  run?: () => void | Promise<void>;
  /** What Tab does: push a second stage into the same box. This is how
   * renaming and reshelving happen without inventing another modal. */
  drill?: () => Stage;
};

/** One level of the drill-down. A stack of these, not a route: the hash does
 * not move until a stage commits, so backing out of a half-finished one
 * leaves nothing behind — no trail leg, no history entry, nothing to undo. */
export type Stage = {
  /** This level's name, for the header. */
  crumb: string;
  placeholder: string;
  /** True when what is typed is *content* rather than a filter — a new title,
   * a page number. Those rows are built from the query and must not then be
   * matched against it. */
  raw?: boolean;
  items: (query: string) => Item[];
};

/** The header line: where in the drill-down the box currently is. */
export function crumbs(stack: Stage[], root = "card catalog"): string {
  return [root, ...stack.map((s) => s.crumb)].join("  ›  ");
}

export type Row = { item: Item; spans: Span[]; score: number };
export type Section = { title: string; rows: Row[] };

/** Characters after which a letter reads as the start of a word. Hyphens and
 * middots matter here: book titles are full of "Kittler · Gramophone" and
 * "Anti-Oedipus", and typing `gram` or `oed` should find them. */
const BOUNDARY = /[\s\-_·/([.,:;'"]/;

/** How many rows any one group may contribute. Without a cap a library of a
 * few hundred books buries every command under BOOKS on a two-letter query. */
export const GROUP_CAP = 6;

/** Score `query` against `label`, or null if the label doesn't contain the
 * query's characters in order. Spans are the matched runs, for highlighting.
 *
 * An empty query matches everything at zero, which is what makes the
 * caller's input order survive to the empty palette. */
export function match(label: string, query: string): { score: number; spans: Span[] } | null {
  const q = query.trim().toLowerCase();
  if (!q) return { score: 0, spans: [] };

  const l = label.toLowerCase();
  const spans: Span[] = [];
  let score = 0;
  let from = 0; // where the next character may be found
  let prev = -2; // where the last one was

  for (const ch of q) {
    if (ch === " ") continue; // spaces separate words in the query, never match
    const at = l.indexOf(ch, from);
    if (at < 0) return null;

    if (at === 0) score += 18; // the label starts with what was typed
    else if (BOUNDARY.test(l[at - 1])) score += 12; // a word inside it does
    else if (at === prev + 1) score += 8; // running on from the last match
    else {
      // An interior letter, reached by skipping. This is the only case where
      // the distance says anything: a word is a word wherever it sits, and
      // charging for the skip to reach one would rank every long book title
      // below every short one. Half this library's titles are "Kittler ·
      // Gramophone, Film, Typewriter", and nobody types those from the front.
      score += 1;
      score -= Math.min(at - from, 6);
    }

    if (at === prev + 1 && spans.length) spans[spans.length - 1][1] = at + 1;
    else spans.push([at, at + 1]);
    prev = at;
    from = at + 1;
  }

  if (l.includes(q)) score += 10; // the whole query, contiguous
  score -= label.length * 0.05; // ties go to the shorter label

  return { score, spans };
}

/** Match every item, drop the misses, bin the survivors by group.
 *
 * Groups are ordered by their best row rather than by a fixed priority list,
 * so a query that clearly names a book puts BOOKS on top and one that names
 * a verb puts COMMANDS there, with no rule to keep in sync. Both sorts are
 * stable, so an empty query — every score zero — falls through with the
 * caller's order intact. */
export function rank(items: Item[], query: string, cap = GROUP_CAP): Section[] {
  const byGroup = new Map<string, Row[]>();
  for (const item of items) {
    const m = match(item.label, query);
    if (!m) continue;
    const row: Row = { item, spans: m.spans, score: m.score };
    const rows = byGroup.get(item.group);
    if (rows) rows.push(row);
    else byGroup.set(item.group, [row]);
  }

  const sections: Section[] = [];
  for (const [title, rows] of byGroup) {
    rows.sort((a, b) => b.score - a.score);
    sections.push({ title, rows: rows.slice(0, cap) });
  }
  sections.sort((a, b) => (b.rows[0]?.score ?? 0) - (a.rows[0]?.score ?? 0));
  return sections;
}

/** The rows in the order the arrow keys walk them. */
export function flatten(sections: Section[]): Row[] {
  return sections.flatMap((s) => s.rows);
}

/** Next selection index, wrapping at both ends. Wrapping rather than
 * clamping because the list is short and the alternative is a dead key at
 * the bottom of it. */
export function step(n: number, i: number, dir: 1 | -1): number {
  if (n <= 0) return 0;
  return (i + dir + n) % n;
}
