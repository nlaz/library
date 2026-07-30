// The Cmd+F contract, driven through the real modules against the real
// index.html chrome: one press opens the popover on everything, each press
// after it steps the unlabelled kind cycle, reopening starts that cycle
// over, and Escape ends the search session (back to the shelves, not the
// results grid).
//
// Keys are dispatched from the *focused* element, which is what makes this
// a regression test: opening the popover focuses #q, and #q stops
// propagation on every keydown — a Cmd+F listener in the bubble phase never
// sees a second press, so the cycle silently sticks on its first step.
//
// A stub transport stands in for the engine — `answer()` plays the
// round-trip the single-flight machinery in search.ts waits for, so the
// coalescing path is exercised rather than stubbed past.

import { beforeAll, describe, expect, it } from "vitest";
import { type Chrome, mountChrome } from "./chrome-fixture";

let app: Chrome;

/** Cmd+F as the user presses it: from whatever has focus. */
const cmdF = () => app.press("f", { metaKey: true });
const escape = () => app.press("Escape");
const answer = () => app.answer();

const $ = (id: string) => app.el(id);
const $q = () => app.q();
const popOpen = () => !$("search-pop").hidden;
/** The kind the box would query with — there is no label to read. */
const liveKind = () => app.sent.at(-1)?.kind;

/** Type a query and let it settle, so later presses start from a live grid. */
function typeQuery(text: string) {
  $q().value = text;
  $q().dispatchEvent(new Event("input", { bubbles: true }));
  answer();
}

beforeAll(async () => {
  app = await mountChrome();
});

describe("cmd+f", () => {
  it("opens the popover and focuses the box, without stepping the cycle", () => {
    expect(popOpen()).toBe(false);
    cmdF();
    expect(popOpen()).toBe(true);
    expect(document.activeElement).toBe($q());
    typeQuery("escapement");
    expect(liveKind()).toBe(""); // everything, the default
  });

  it("steps all -> figures -> text/notes on every press after, from the box", () => {
    // focus is in #q here — the bubble-phase bug lived exactly on this path
    expect(document.activeElement).toBe($q());
    cmdF();
    expect(liveKind()).toBe("images");
    answer();
    cmdF();
    expect(liveKind()).toBe("text");
    answer();
    cmdF();
    expect(liveKind()).toBe(""); // wrapped
    answer();
  });

  it("coalesces a press made while an answer is still in flight", () => {
    cmdF(); // -> figures, sent
    cmdF(); // -> text/notes, while that one is still out
    expect(liveKind()).toBe("images");
    answer(); // the pending answer lands; the dirty catch-up goes out
    expect(liveKind()).toBe("text");
    answer();
    // a live query means the results grid, not the shelves
    expect($("search").hidden).toBe(false);
    expect($("home").hidden).toBe(true);
  });
});

describe("escape", () => {
  it("ends the search and returns to the shelf view", () => {
    escape();
    expect(popOpen()).toBe(false);
    expect($q().value).toBe("");
    expect($("search").hidden).toBe(true);
    expect($("home").hidden).toBe(false);
  });

  it("starts the next session over on everything", () => {
    // the cycle was left on text/notes above; reopening must not inherit it
    cmdF();
    typeQuery("escapement");
    expect(liveKind()).toBe("");
    // so the first press after opening is always the figures filter
    cmdF();
    expect(liveKind()).toBe("images");
    answer();
    // and that choice, too, is dropped by the next open
    escape();
    cmdF();
    typeQuery("escapement");
    expect(liveKind()).toBe("");
  });
});
