// Test-only: mount the real index.html chrome and the real modules over a
// stub transport, so interaction tests exercise the shipped wiring instead
// of a hand-built approximation of it. Never imported by the app.
//
// Three invariants live here rather than in each suite, because each one
// silently produces a *passing* test when it is got wrong:
//
//   1. Strip <head> and every <script>, or importing the chrome bootstraps
//      main.ts and races the suite.
//   2. Mount before the first import of anything that reaches dom.ts — its
//      getElementById calls run at module scope, and a missing element
//      resolves to null rather than throwing.
//   3. Dispatch keys from document.activeElement, not from `document`. #q
//      calls stopPropagation() on every keydown, so a key sent straight to
//      the document walks past the very listener under test.
//
// The name deliberately does not match vitest.config.ts's `src/**/*.test.ts`
// include, or this file would be collected as a suite with no tests in it.

import { readFileSync } from "node:fs";
import type { Transport } from "./transport";
import type { QueryMsg, WireHit, WireResponse } from "./types";

/** index.html's body, minus the scripts. */
export const CHROME = readFileSync(`${process.cwd()}/index.html`, "utf8")
  .replace(/[\s\S]*<body[^>]*>/, "")
  .replace(/<script[\s\S]*/, "");

export type Chrome = {
  /** Every query the box has dispatched, oldest first. */
  sent: QueryMsg[];
  /** Answer the newest query the way the engine would, a round-trip later.
   * search.ts holds one query in flight and coalesces the rest, so a suite
   * that never answers is testing a stalled engine. */
  answer: (hits?: WireHit[]) => void;
  /** A keypress from whatever has focus; false if something called
   * preventDefault (KeyboardEvent is cancelable here). */
  press: (key: string, init?: KeyboardEventInit) => boolean;
  q: () => HTMLInputElement;
  el: (id: string) => HTMLElement;
};

/** Mount the chrome and wire the real modules to a stub transport. Call once
 * per test *file* (in `beforeAll`): the modules register document-level
 * listeners and hold module-scope state that a second mount would double up.
 * `over` replaces individual Transport methods — pass a scriptable
 * `complete` to drive the search box's completions. */
export async function mountChrome(over: Partial<Transport> = {}): Promise<Chrome> {
  document.body.innerHTML = CHROME;

  const sent: QueryMsg[] = [];
  let respond: (m: WireResponse) => void = () => {};

  const state = await import("./state");
  state.setTransport({
    ready: async () => {},
    send: (q) => {
      sent.push(q);
    },
    onResponse: (cb) => {
      respond = cb;
    },
    collections: async () => ({}),
    complete: async () => [],
    ...over,
  });

  const search = await import("./search");
  await import("./viewer"); // registers the Cmd+F and Escape handlers
  search.initSearch();

  return {
    sent,
    answer: (hits = []) =>
      respond({ seq: sent.at(-1)!.seq, phase: "hybrid", us: 1000, hits }),
    press: (key, init) =>
      (document.activeElement ?? document).dispatchEvent(
        new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init }),
      ),
    q: () => document.getElementById("q") as HTMLInputElement,
    el: (id) => document.getElementById(id)!,
  };
}
