import { describe, expect, it } from "vitest";
import {
  backlinks,
  compareCards,
  displayAddr,
  splitPoint,
  threads,
  wikiTokens,
} from "./notebox-model";
import type { CardRec } from "./types";

const card = (over: Partial<CardRec>): CardRec => ({
  id: "c0",
  thread: 1,
  addr: [1],
  title: "t",
  body: "",
  evidence: [],
  links: [],
  created: 0,
  modified: 0,
  filed: false,
  split_hinted: false,
  ...over,
});

describe("displayAddr", () => {
  it("mirrors the Rust display rules", () => {
    expect(displayAddr(21, [3])).toBe("21/3");
    expect(displayAddr(21, [3, 1])).toBe("21/3a");
    expect(displayAddr(21, [3, 1, 2])).toBe("21/3a2");
    expect(displayAddr(1, [12, 26])).toBe("1/12z");
    expect(displayAddr(1, [12, 27])).toBe("1/12aa");
    expect(displayAddr(1, [12, 53])).toBe("1/12ba");
  });
});

describe("compareCards", () => {
  it("reads as a thread: branch directly after parent", () => {
    const order = [
      card({ id: "e", thread: 2, addr: [1] }),
      card({ id: "b", thread: 1, addr: [3, 1] }),
      card({ id: "d", thread: 1, addr: [4] }),
      card({ id: "a", thread: 1, addr: [3] }),
      card({ id: "c", thread: 1, addr: [3, 1, 1] }),
      card({ id: "f", thread: 1, addr: [3, 2] }),
    ]
      .sort(compareCards)
      .map((c) => c.id);
    expect(order).toEqual(["a", "b", "c", "f", "d", "e"]);
  });
});

describe("threads", () => {
  it("groups, names by trunk, floats recent, drops all-filed", () => {
    const rows = threads([
      card({ id: "a", thread: 1, addr: [1], title: "first claim", modified: 10 }),
      card({ id: "b", thread: 1, addr: [1, 1], title: "aside", modified: 50 }),
      card({ id: "c", thread: 1, addr: [2], title: "second", modified: 20, filed: true }),
      card({ id: "d", thread: 2, addr: [1], title: "other topic", modified: 30 }),
      card({ id: "e", thread: 3, addr: [1], title: "gone", filed: true }),
    ]);
    expect(rows.map((r) => r.thread)).toEqual([1, 2]); // 50 > 30
    expect(rows[0].name).toBe("first claim");
    expect(rows[0].cards.map((c) => c.id)).toEqual(["a", "b"]);
    expect(rows[0].filed).toBe(1);
  });
});

describe("backlinks", () => {
  it("finds typed links and wiki mentions, skips filed and self", () => {
    const target = card({ id: "x", title: "casting speed" });
    const cards = [
      target,
      card({ id: "l1", links: [{ to: "x", kind: "relates" }] }),
      card({ id: "l2", body: "see [[casting speed]] for the ceiling" }),
      card({ id: "l3", body: "see [[casting speed]]", filed: true }),
      card({ id: "l4", body: "unrelated" }),
    ];
    expect(backlinks(cards, target).map((c) => c.id)).toEqual(["l1", "l2"]);
  });
});

describe("wikiTokens", () => {
  it("tokenizes links in place", () => {
    expect(wikiTokens("see [[a b]] and [[c]]!")).toEqual([
      { kind: "text", text: "see " },
      { kind: "link", title: "a b" },
      { kind: "text", text: " and " },
      { kind: "link", title: "c" },
      { kind: "text", text: "!" },
    ]);
    expect(wikiTokens("plain")).toEqual([{ kind: "text", text: "plain" }]);
  });
});

describe("splitPoint", () => {
  it("stays quiet for card-sized bodies", () => {
    expect(splitPoint("a few words", 150)).toBeNull();
  });

  it("cuts at the sentence boundary after the limit", () => {
    const body = "one two three four. five six seven. eight nine";
    const cut = splitPoint(body, 3);
    expect(cut).not.toBeNull();
    expect(body.slice(0, cut!)).toBe("one two three four. ");
    expect(body.slice(cut!)).toBe("five six seven. eight nine");
  });

  it("falls back to the word boundary when no sentence ends", () => {
    const body = "one two three four five";
    const cut = splitPoint(body, 3);
    expect(body.slice(0, cut!)).toBe("one two three");
  });
});
