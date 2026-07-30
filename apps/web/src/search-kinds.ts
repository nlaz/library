// The result-kind filter behind Cmd+F — an unlabelled hotkey, so the cycle
// order is the whole of its interface. Pure, so it is unit-testable away
// from search.ts's transport and DOM.

/** Wire `kind` values, in the order Cmd+F walks them:
 *
 *   ""       everything — page text, notes, and figures blended (the default)
 *   "images" figures only
 *   "text"   the text index only: page text *and* notes, no figures
 *
 * Must match `Query::kind` in library-core/src/search_api.rs. */
export const KINDS = ["", "images", "text"] as const;

export type Kind = (typeof KINDS)[number];

/** Where every search session starts: everything, so the first press after
 * the popover opens moves to figures. */
export const DEFAULT_KIND: Kind = "";

/** The next kind in the cycle: all → figures → text/notes → all. */
export function nextKind(k: Kind): Kind {
  return KINDS[(KINDS.indexOf(k) + 1) % KINDS.length];
}
