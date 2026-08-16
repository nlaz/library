// Retry policy for page images outside the reader — the results grid and the
// viewer it opens.
//
// Page renders are an evictable cache, so a request can miss a page whose
// document is perfectly intact: the protocol handler rasterizes it on demand
// behind a two-worker gate and sheds with 503 past eight requests in flight
// (library-app/src/render.rs). That gate is explicit that shedding is only
// backpressure if the client comes back for the page. The reader came back
// (reader-model.ts); the grid did not, so a shed thumbnail stayed the
// browser's broken-image glyph until the next query. On a library larger
// than the page-cache budget — where nearly every hit is a miss — that was
// most of the grid.
//
// Two things differ from the reader's policy, both because a grid asks for a
// screenful of pages at once rather than a few in scroll order:
//
//   longer    the reader has to land inside its prefetch runway or the hole
//             is seen; a card can wait. A dozen misses at ~160ms across two
//             render workers is a second of rasterizing before the last one
//             is even started, and the shed ones queue behind that.
//   jittered  a dozen cards shed together would otherwise retry together,
//             rebuilding the very queue that shed them. Spreading them is
//             what makes the retry backpressure rather than a second flood.

/** Attempts after the first before a page image gives up. */
export const GRID_RETRIES = 4;

/** Spread applied to each delay, ±40%. */
const JITTER = 0.4;

/** How long to wait before retry `attempt` (1-based), or null to give up.
 *
 * `rand` is injectable so the schedule can be asserted without randomness —
 * 0.5 is the centre of the spread, and the delays below are quoted at it. */
export function gridRetryDelay(attempt: number, rand: () => number = Math.random): number | null {
  if (!Number.isInteger(attempt) || attempt < 1 || attempt > GRID_RETRIES) return null;
  const base = 500 * 2 ** (attempt - 1); // 500ms, 1s, 2s, 4s
  return Math.round(base * (1 - JITTER + 2 * JITTER * rand()));
}
