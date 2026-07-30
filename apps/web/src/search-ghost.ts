// What the search box paints as a faint continuation of the word being
// typed, and what it asks the engine to complete. Pure, so the gates (caret,
// composition, overflow) and the token/casing arithmetic are testable away
// from search.ts's DOM and transport.

/** Shortest word worth completing. Also the engine's own floor: `tokenize`
 * drops 1-char tokens, so a single letter is not a term — and as a *prefix*
 * it matches so much of the dictionary that the ghost would just thrash. */
export const MIN_PREFIX = 2;

/** The lowercase term prefix to hand `transport.complete()`: the word being
 * typed, tokenized the way the engine does it — whitespace-split, inner
 * non-alphanumerics dropped, lowercased (library-core/src/text.rs::tokenize,
 * which is where TermDict's keys come from, so anything else returns no
 * matches at all).
 *
 * `""` means ask for nothing and show nothing: too short a word, a value that
 * ends in whitespace, or a word ending in punctuation — after a "." there is
 * no word left to continue. */
export function completionPrefix(value: string): string {
  const word = /\S*$/.exec(value)?.[0] ?? "";
  // a trailing non-alphanumeric ends the word; completing past it would
  // graft the suggestion onto the punctuation ("esc." -> "esc.apement")
  if (!/[\p{L}\p{N}]$/u.test(word)) return "";
  const prefix = word.replace(/[^\p{L}\p{N}]/gu, "").toLowerCase();
  return prefix.length < MIN_PREFIX ? "" : prefix;
}

export type GhostCtx = {
  /** The box's value, verbatim — casing, leading spaces and all. */
  value: string;
  /** `transport.complete()`'s answer: lowercase terms, most frequent first.
   * Deliberately kept across keystrokes, so typing on inside one word
   * repaints from cache with no round trip. */
  candidates: string[];
  /** Caret collapsed at the end of the value. A completion drawn anywhere
   * else lies about where the text would land. */
  caretAtEnd: boolean;
  /** IME composition in flight — half-composed input is never a term. */
  composing: boolean;
  /** Doc-scoped reader find; the term dictionary is library-wide. */
  readerFind: boolean;
  /** The value already overflows the field, so the input has scrolled its
   * text and the overlay has not. */
  overflowing: boolean;
};

/** The faint continuation to paint after `value`; `""` for no ghost.
 *
 * Always a suffix of a real term whose prefix is the value's last word, so
 * accepting is exactly `value + tail` — which is what preserves the user's
 * casing ("Escap" + "ement") and every earlier word ("gear " + "tra" + "in").
 *
 * The tail is sliced off the *candidate* by the prefix's length, never by
 * what the user typed, so punctuation inside the word survives: "gear-tra"
 * asks for "geartra", matches "geartrain", and completes to "gear-train".
 *
 * Staleness is structurally impossible here: whatever prefix `candidates`
 * was fetched for, every one is re-checked against the current word. A list
 * fetched for "esc" still ghosts correctly for "escap" (just ranked by the
 * older query until the next fetch lands) and ghosts nothing for "gear". */
export function ghostTail(ctx: GhostCtx): string {
  if (ctx.composing || ctx.readerFind || ctx.overflowing || !ctx.caretAtEnd) return "";
  const prefix = completionPrefix(ctx.value);
  if (!prefix) return "";
  for (const c of ctx.candidates) {
    const term = c.toLowerCase();
    // a candidate equal to the prefix is the engine echoing it back: it has
    // nothing to add, which is the same thing as no ghost at all
    if (term.startsWith(prefix) && term.length > prefix.length) {
      return term.slice(prefix.length);
    }
  }
  return "";
}
