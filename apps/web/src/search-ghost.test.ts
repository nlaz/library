// The completion's arithmetic: which word gets asked about, and what the
// faint tail after it must be. The invariant every case is really checking
// is that `value + tail` is a sentence the user would have typed — the right
// word finished, earlier words and their casing untouched.

import { describe, expect, it } from "vitest";
import { completionPrefix, type GhostCtx, ghostTail } from "./search-ghost";

describe("completionPrefix", () => {
  it("asks about the last word, lowercased", () => {
    expect(completionPrefix("esc")).toBe("esc");
    expect(completionPrefix("Escap")).toBe("escap");
    expect(completionPrefix("GEAR Tra")).toBe("tra");
  });

  it("tokenizes the way the engine does, so the dictionary can answer", () => {
    // text.rs::tokenize drops inner punctuation rather than splitting on it
    expect(completionPrefix("gear-tra")).toBe("geartra");
    expect(completionPrefix("café")).toBe("café");
  });

  it("asks nothing when there is no word to continue", () => {
    expect(completionPrefix("")).toBe("");
    expect(completionPrefix("gear ")).toBe(""); // ends in whitespace
    expect(completionPrefix("esc.")).toBe(""); // would graft onto the period
    expect(completionPrefix("e")).toBe(""); // under MIN_PREFIX
    expect(completionPrefix("gear t")).toBe(""); // ...and so is a lone letter
  });
});

const ctx = (over: Partial<GhostCtx> = {}): GhostCtx => ({
  value: "esc",
  candidates: ["escapement", "escape"],
  caretAtEnd: true,
  composing: false,
  readerFind: false,
  overflowing: false,
  ...over,
});

describe("ghostTail", () => {
  it("continues the word with the engine's first (most frequent) candidate", () => {
    expect(ghostTail(ctx())).toBe("apement");
  });

  it("skips a candidate that is the typed word echoed back", () => {
    expect(ghostTail(ctx({ candidates: ["esc", "escape"] }))).toBe("ape");
    expect(ghostTail(ctx({ candidates: ["esc"] }))).toBe("");
  });

  it("keeps the user's casing and their earlier words", () => {
    const escap = ctx({ value: "Escap", candidates: ["escapement"] });
    expect("Escap" + ghostTail(escap)).toBe("Escapement");
    const gear = ctx({ value: "gear tra", candidates: ["train"] });
    expect("gear tra" + ghostTail(gear)).toBe("gear train");
    // punctuation inside the word survives, because the tail is sliced off
    // the candidate by the *prefix* length, not by what was typed
    const hyphen = ctx({ value: "gear-tra", candidates: ["geartrain"] });
    expect("gear-tra" + ghostTail(hyphen)).toBe("gear-train");
    const accent = ctx({ value: "Café", candidates: ["cafés"] });
    expect("Café" + ghostTail(accent)).toBe("Cafés");
  });

  it("shows nothing when a gate is closed", () => {
    expect(ghostTail(ctx({ caretAtEnd: false }))).toBe("");
    expect(ghostTail(ctx({ composing: true }))).toBe("");
    expect(ghostTail(ctx({ readerFind: true }))).toBe("");
    expect(ghostTail(ctx({ overflowing: true }))).toBe("");
    expect(ghostTail(ctx({ candidates: [] }))).toBe("");
  });

  it("re-checks a cached candidate list against the current word", () => {
    // fetched for "esc", still typing: correct tail, no round trip needed
    expect(ghostTail(ctx({ value: "escap" }))).toBe("ement");
    // ...and once the word moves on, the stale list simply shows nothing
    expect(ghostTail(ctx({ value: "gear" }))).toBe("");
  });
});
