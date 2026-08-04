// Layout for the search flame chart. DOM-free on purpose (the house
// model/render split — see atlas-model.ts): the geometry is where the bugs
// would be, and this way they're unit-testable without standing up the
// overlay's markup.
//
// This is a flame *chart*, not a flamegraph: x is real elapsed time, not an
// aggregated stack profile. That distinction is the whole point here. The
// spans come from one search, carrying offsets from a single origin shared by
// both tracks (see perf.rs::Trace), so overlap on the x-axis means the two
// tracks genuinely ran at once — which is exactly what answer()'s
// thread::scope does and what the old percent-of-sum waterfall could not say.

import type { Span } from "./types";

/** A span placed on the timeline. `left`/`width` are percentages of the whole
 * search; `row` is the y-slot, already offset for the span's lane. */
export type Block = {
  name: string;
  us: number;
  /** Duration not claimed by any direct child — a stage's own cost. */
  self_us: number;
  at_us: number;
  left: number;
  width: number;
  row: number;
  track: number;
  depth: number;
};

export type Flame = {
  blocks: Block[];
  /** Total y-slots, including the implicit root and the inter-lane gap. */
  rows: number;
  /** The timeline's full width in µs — what 100% means. */
  span_us: number;
  /** Measured time on the widest lane, against `span_us`. */
  lanes: { track: number; from: number; to: number }[];
};

/** Row 0 is the implicit root drawn from `total_us`; every span sits below. */
const ROOT_ROW = 1;
/** A blank row between the two lanes, so overlap reads as concurrency rather
 * than as one deep tree that happens to double back in time. */
const LANE_GAP = 1;
/** Percent floor, so a 40µs `blend` next to a 20ms search is still a target
 * you can hit with a pointer. The waterfall this replaces used the same. */
const MIN_WIDTH = 0.5;

/**
 * Place a record's spans on one timeline.
 *
 * `total_us` sets the root's width, but the timeline is stretched to whatever
 * is larger — a span is timed inside `answer` and the total outside it, so
 * clock granularity can put a child a microsecond past its parent, and a
 * block drawn past 100% would overflow its own root.
 */
export function layout(spans: Span[], total_us: number): Flame {
  const reach = spans.reduce((m, s) => Math.max(m, s.at_us + s.us), 0);
  const span_us = Math.max(total_us, reach, 1);

  // depth-first within a lane: sort by start, then by depth, so a parent
  // always precedes the children it encloses
  const sorted = [...spans].sort((a, b) =>
    a.track !== b.track ? a.track - b.track : a.at_us !== b.at_us ? a.at_us - b.at_us : a.depth - b.depth,
  );

  // lanes stack: the image track starts below the text track's deepest row.
  // Tracks are numbered but need not be contiguous or start at 0 (an
  // images-only query records no text spans at all).
  const tracks = [...new Set(sorted.map((s) => s.track))].sort((a, b) => a - b);
  const rowOf = new Map<number, number>();
  let next = ROOT_ROW;
  for (const t of tracks) {
    rowOf.set(t, next);
    const deepest = Math.max(...sorted.filter((s) => s.track === t).map((s) => s.depth));
    next += deepest + 1 + LANE_GAP;
  }

  const blocks: Block[] = sorted.map((s) => ({
    name: s.name,
    us: s.us,
    self_us: s.us - childrenUs(s, sorted),
    at_us: s.at_us,
    left: (s.at_us / span_us) * 100,
    width: Math.max(MIN_WIDTH, (s.us / span_us) * 100),
    row: (rowOf.get(s.track) ?? ROOT_ROW) + s.depth,
    track: s.track,
    depth: s.depth,
  }));

  const lanes = tracks.map((t) => {
    const own = sorted.filter((s) => s.track === t);
    return {
      track: t,
      from: Math.min(...own.map((s) => s.at_us)),
      to: Math.max(...own.map((s) => s.at_us + s.us)),
    };
  });

  return { blocks, rows: tracks.length ? next - LANE_GAP : ROOT_ROW, span_us, lanes };
}

/**
 * Time claimed by `s`'s direct children: same track, one deeper, contained.
 *
 * Containment does the work rather than adjacency, because emission order
 * carries no meaning — a span closes when its work ends, so a parent trails
 * the children it encloses. The `depth + 1` bound is what keeps a
 * grandchild's µs from being subtracted twice.
 */
function childrenUs(s: Span, all: Span[]): number {
  return all
    .filter(
      (c) =>
        c !== s &&
        c.track === s.track &&
        c.depth === s.depth + 1 &&
        c.at_us >= s.at_us &&
        c.at_us + c.us <= s.at_us + s.us,
    )
    .reduce((a, c) => a + c.us, 0);
}

/** Monospace advance at --fs-micro, plus the block's 3px padding either
 * side. Approximate on purpose — this only decides whether a label is worth
 * attempting, and being a character out either way costs nothing. */
const CH_PX = 6.2;
const PAD_PX = 8;

/**
 * How much of a block's label fits in `px`.
 *
 * A block narrow enough to clip its own name mid-word renders as two or three
 * stray glyphs, and a row of those reads as corruption rather than as small
 * spans. Below that width it shows nothing and the popover carries the name —
 * every block stays hoverable however thin it is.
 */
export function labelMode(name: string, duration: string, px: number): "full" | "name" | "none" {
  if (px >= (name.length + 1 + duration.length) * CH_PX + PAD_PX) return "full";
  if (px >= name.length * CH_PX + PAD_PX) return "name";
  return "none";
}

/**
 * Every span the search path can emit — the two tracks, their stages, and the
 * ranker's four fusion children (see search_api.rs, rank.rs, tools.rs).
 *
 * The chart marks each block up as a glossary term, keyed on the name the
 * server sent, so nothing in perf.ts couples to these literals. This list is
 * what perf-gloss.test.ts holds against GLOSS instead — otherwise a span
 * renamed in Rust would quietly start popping up an empty tooltip.
 */
export const SPAN_NAMES = [
  "text_track",
  "img_track",
  "ese_embed",
  "term_expand",
  "lex_search",
  "vec_search",
  "fuse+resolve",
  "fuse",
  "maxsim",
  "mmr",
  "resolve",
  "clip_embed",
  "image_search",
  "blend",
  // the agent tool's own stages
  "search_tool",
  "search",
  "dedup+cutoff",
  "top_hit_page",
] as const;

/** The depth-1 spans, in time order — the collapsed row's one-line summary.
 * Depth 0 is the track itself (its total says nothing the row doesn't), and
 * deeper is detail the expanded chart is for. */
export function summary(spans: Span[]): Span[] {
  return spans.filter((s) => s.depth === 1).sort((a, b) => a.at_us - b.at_us);
}

/** Median µs of a named span across a set of records — the header's live
 * evidence for the stages worth watching. Null when nothing recorded it. */
export function spanMedian(records: { spans: Span[] }[], name: string): number | null {
  const xs = records
    .map((r) => r.spans.find((s) => s.name === name)?.us)
    .filter((v): v is number => v !== undefined)
    .sort((a, b) => a - b);
  if (!xs.length) return null;
  const m = xs.length >> 1;
  return xs.length % 2 ? xs[m] : (xs[m - 1] + xs[m]) / 2;
}
