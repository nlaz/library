// The completion as the user meets it, driven through the real modules and
// the real index.html chrome: a faint tail appears after the debounce, Tab
// (or → / End) takes it and re-runs the search, and every way of leaving the
// word behind wipes it. What the spans hold IS what Tab accepts, so the
// assertions read the same DOM the user is looking at.

import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { type Chrome, mountChrome } from "./chrome-fixture";

let app: Chrome;
/** Scriptable engine: what the next complete() resolves with, and how many
 * times it has been asked (the cache tests turn on that count). */
let terms: string[] = [];
let completions = 0;
let failing = false;

const $q = () => app.q();
const typed = () => app.el("q-typed").textContent;
const tail = () => app.el("q-tail").textContent;

/** A keystroke: set the value the way typing would. The engine answers the
 * query it triggers, because search.ts holds one query in flight and
 * coalesces the rest — a suite that never answers is testing a stalled
 * engine, and the next accepted word would be swallowed by the catch-up. */
function type(text: string) {
  $q().value = text;
  // a keystroke leaves a collapsed caret after what it typed; happy-dom
  // keeps whatever selection anchor was there before the assignment, and a
  // stale one reads as "caret not at the end" and suppresses the ghost
  $q().setSelectionRange(text.length, text.length);
  $q().dispatchEvent(new Event("input", { bubbles: true }));
  if (app.sent.length) app.answer();
}

/** Run out the 80ms debounce and let complete()'s promise resolve. */
const settle = () => vi.advanceTimersByTimeAsync(80);

beforeAll(async () => {
  vi.useFakeTimers();
  app = await mountChrome({
    complete: async () => {
      completions++;
      if (failing) throw new Error("engine down");
      return terms;
    },
  });
  terms = ["escapement", "escape"];
});

afterAll(() => vi.useRealTimers());

describe("ghost text", () => {
  it("paints the likeliest continuation after the debounce", async () => {
    app.press("f", { metaKey: true }); // open the popover, focus the box
    type("esc");
    expect(tail()).toBe(""); // nothing cached yet — no blind guess
    await settle();
    expect(typed()).toBe("esc");
    expect(tail()).toBe("apement");
  });

  it("repaints from cache on the next keystroke, with no round trip", async () => {
    const before = completions;
    type("escap");
    expect(tail()).toBe("ement"); // synchronous: the ghost never blinks
    expect(completions).toBe(before);
    await settle(); // the refresh lands later and agrees
    expect(tail()).toBe("ement");
  });

  it("accepts on Tab, and the completed word is a new query", () => {
    expect(app.press("Tab")).toBe(false); // preventDefault: focus stays put
    expect($q().value).toBe("escapement");
    expect(typed()).toBe("");
    expect(tail()).toBe("");
    expect(app.sent.at(-1)).toMatchObject({ q: "escapement" });
  });

  it("leaves Tab alone when there is nothing to accept", () => {
    expect(tail()).toBe("");
    expect(app.press("Tab")).toBe(true); // not prevented: Tab moves focus
  });

  it("also accepts on ArrowRight and End", async () => {
    type("esc");
    await settle();
    expect(app.press("ArrowRight")).toBe(false);
    expect($q().value).toBe("escapement");

    type("esc");
    await settle();
    expect(app.press("End")).toBe(false);
    expect($q().value).toBe("escapement");
    // ...but not with a modifier held: Shift+→ is a selection, not an accept
    type("esc");
    await settle();
    expect(app.press("ArrowRight", { shiftKey: true })).toBe(true);
    expect($q().value).toBe("esc");
  });

  it("drops a response that a newer keystroke superseded", async () => {
    type("esc");
    terms = ["escapology"]; // the answer to a query that is already stale
    const slow = settle();
    type("gear");
    terms = ["gears"];
    await slow;
    await settle();
    expect(tail()).toBe("s"); // "gear" + "s", never "escapology"
  });

  it("hides when the caret leaves the end of the value", async () => {
    type("esc");
    terms = ["escapement", "escape"];
    await settle();
    expect(tail()).toBe("apement");
    $q().setSelectionRange(1, 1);
    $q().dispatchEvent(new KeyboardEvent("keyup", { key: "ArrowLeft", bubbles: true }));
    expect(tail()).toBe("");
  });

  it("hides while text is selected, so a reopened box does not lie", () => {
    $q().setSelectionRange(0, $q().value.length); // what openSearchPop does
    $q().dispatchEvent(new Event("select", { bubbles: true }));
    expect(tail()).toBe("");
  });

  it("hides while an IME is composing, and comes back after", async () => {
    $q().setSelectionRange(3, 3);
    $q().dispatchEvent(new Event("select", { bubbles: true }));
    expect(tail()).toBe("apement");

    const before = completions;
    $q().dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    expect(tail()).toBe("");
    await settle();
    expect(completions).toBe(before); // half-composed input is never a term

    $q().dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
    await settle();
    expect(tail()).toBe("apement");
  });

  it("hides on blur, and on the Escape that ends the search", async () => {
    $q().dispatchEvent(new Event("blur"));
    expect(tail()).toBe("");

    type("esc");
    await settle();
    expect(tail()).toBe("apement");
    app.press("Escape");
    expect($q().value).toBe("");
    expect(tail()).toBe("");
  });

  it("does not resurrect a completion when the popover reopens", async () => {
    type("esc");
    await settle();
    app.press("Escape"); // clears the query and closes
    app.press("f", { metaKey: true }); // reopen
    expect(tail()).toBe("");
  });

  it("survives an engine that rejects", async () => {
    failing = true;
    type("esc");
    await settle();
    expect(tail()).toBe(""); // no ghost, no unhandled rejection
    failing = false;
    type("esc");
    await settle();
    expect(tail()).toBe("apement"); // and it recovers on the next word
  });
});
