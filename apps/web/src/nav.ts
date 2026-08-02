// The one live navigation trail. Every surface change goes through the
// hash, so this module can watch hashchange and keep the record without a
// single call site having to remember to file one.
//
// Registration order matters and is not accidental: this module is pulled
// in by reader.ts/sheet.ts/notebox.ts, i.e. during main.ts's imports, so
// its listener is in place before main.ts's own route() — the surface being
// opened can already ask where it was opened from.

import { docTitle } from "./format";
import { forgetDoc as forget, navLabel, popNav, pushNav } from "./nav-model";

const HOME = "#/";

let trail: string[] = [];
let here = location.hash || HOME;
/** Return trips still owed a hashchange. A count rather than a flag: each
 * assignment queues its own event, so two Escapes inside one frame would
 * otherwise leave the second looking like a fresh leg of the trail. */
let returning = 0;

window.addEventListener("hashchange", () => {
  const to = location.hash || HOME;
  if (returning) returning--;
  else trail = pushNav(trail, here, to);
  here = to;
});

/** The hash the current surface was entered from, without consuming it. */
export function origin(): string | null {
  return popNav(trail).to;
}

/** What to call that surface on a button — "library", "notes", or the
 * book's title. `fallback` covers a deep link, where there is no trail. */
export function originLabel(fallback = "library"): string {
  const o = origin();
  return o ? navLabel(o, docTitle) : fallback;
}

/** The surfaces behind this one, nearest first, without consuming any of
 * them — what the card catalog offers before anything has been typed.
 *
 * A copy, deliberately: the trail is this module's, and a caller that could
 * hold a reference to it could reorder someone's way back. */
export function recentTrail(): string[] {
  return [...trail].reverse();
}

/** Take the origin off the trail; the caller navigates (with returnTo).
 * Consuming and navigating are separate because leaving a surface is not
 * always going back to it — the sheet's "keep" files the note in the
 * ledger, and the leg it was written from still has to come off. */
export function takeOrigin(): string | null {
  const { to, trail: rest } = popNav(trail);
  trail = rest;
  return to;
}

/** Navigate as a return: `dest` does not become a new leg of the trail. */
export function returnTo(dest: string) {
  if (dest === (location.hash || HOME)) return;
  returning++;
  location.hash = dest;
}

/** Leave the current surface for the one it was entered from — or
 * `fallback` when the trail is empty (a deep link, or a first paint). */
export function goBack(fallback = HOME) {
  returnTo(takeOrigin() ?? fallback);
}

/** A deleted book must not stay on anyone's way back. */
export function forgetDoc(doc: string) {
  trail = forget(trail, doc);
}
