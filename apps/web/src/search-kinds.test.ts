// The Cmd+F cycle is the whole feature's contract — there is no label to
// read, so the starting kind, the order and the wrap all have to be exact.

import { describe, expect, it } from "vitest";
import { DEFAULT_KIND, KINDS, type Kind, nextKind } from "./search-kinds";

describe("nextKind", () => {
  it("walks all -> figures -> text/notes and wraps", () => {
    expect(nextKind("")).toBe("images");
    expect(nextKind("images")).toBe("text");
    expect(nextKind("text")).toBe("");
  });

  it("returns to the starting kind after one full cycle", () => {
    let k: Kind = "";
    const seen: Kind[] = [];
    for (let i = 0; i < KINDS.length; i++) {
      k = nextKind(k);
      seen.push(k);
    }
    expect(k).toBe("");
    expect(new Set(seen).size).toBe(KINDS.length); // every kind, exactly once
  });
});

describe("DEFAULT_KIND", () => {
  it("is everything, one step short of figures", () => {
    expect(DEFAULT_KIND).toBe("");
    expect(nextKind(DEFAULT_KIND)).toBe("images");
  });
});
