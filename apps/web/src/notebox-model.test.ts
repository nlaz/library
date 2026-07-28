import { describe, expect, it } from "vitest";
import {
  backlinks,
  cardShown,
  cropCSS,
  docCounts,
  evidenceDocs,
  impliedTitle,
  pruneSelection,
  splitPoint,
  timeline,
  wikiTokens,
} from "./notebox-model";
import type { CardRec, QuoteAnchor } from "./types";

const card = (over: Partial<CardRec>): CardRec => ({
  id: "c0",
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

describe("timeline", () => {
  it("reads newest-first by birth, edits don't reshuffle", () => {
    const { live } = timeline([
      card({ id: "a", created: 10, modified: 99 }), // edited late — stays put
      card({ id: "b", created: 30 }),
      card({ id: "c", created: 20 }),
    ]);
    expect(live.map((c) => c.id)).toEqual(["b", "c", "a"]);
  });

  it("splits filed cards out, both halves ordered", () => {
    const { live, filed } = timeline([
      card({ id: "a", created: 10 }),
      card({ id: "b", created: 30, filed: true }),
      card({ id: "c", created: 20 }),
      card({ id: "d", created: 5, filed: true }),
    ]);
    expect(live.map((c) => c.id)).toEqual(["c", "a"]);
    expect(filed.map((c) => c.id)).toEqual(["b", "d"]);
  });

  it("breaks created ties on id so the order is total", () => {
    const { live } = timeline([
      card({ id: "x", created: 10 }),
      card({ id: "y", created: 10 }),
    ]);
    expect(live.map((c) => c.id)).toEqual(["y", "x"]);
    expect(
      timeline([card({ id: "y", created: 10 }), card({ id: "x", created: 10 })]).live.map(
        (c) => c.id,
      ),
    ).toEqual(["y", "x"]);
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

const region = (doc: string, page = 1): QuoteAnchor => ({
  doc,
  page,
  kind: "region",
  bbox: [0.1, 0.2, 0.4, 0.1],
});

describe("ledger filters", () => {
  const cards = [
    card({ id: "a", evidence: [region("lwec"), region("lwec", 2)] }),
    card({ id: "b", evidence: [region("lwec")] }),
    card({ id: "c", evidence: [region("cookery")] }),
    card({ id: "d" }), // no evidence — belongs to no document
  ];

  it("evidenceDocs dedupes in first-seen order", () => {
    expect(evidenceDocs(cards[0])).toEqual(["lwec"]);
    expect(evidenceDocs(card({ evidence: [region("b"), region("a"), region("b")] }))).toEqual([
      "b",
      "a",
    ]);
  });

  it("docCounts counts cards, not anchors", () => {
    expect([...docCounts(cards)]).toEqual([
      ["lwec", 2],
      ["cookery", 1],
    ]);
  });

  it("cardShown intersects collection scope and rail selection", () => {
    const none = new Set<string>();
    expect(cardShown(cards[0], null, none)).toBe(true);
    expect(cardShown(cards[0], new Set(["lwec"]), none)).toBe(true);
    expect(cardShown(cards[2], new Set(["lwec"]), none)).toBe(false);
    expect(cardShown(cards[0], null, new Set(["cookery"]))).toBe(false);
    expect(cardShown(cards[0], new Set(["lwec"]), new Set(["lwec"]))).toBe(true);
    // in scope but not selected — the selection still filters
    expect(cardShown(cards[2], new Set(["lwec", "cookery"]), new Set(["lwec"]))).toBe(false);
  });

  it("any active filter hides evidence-less cards", () => {
    expect(cardShown(cards[3], null, new Set())).toBe(true);
    expect(cardShown(cards[3], new Set(["lwec"]), new Set())).toBe(false);
    expect(cardShown(cards[3], null, new Set(["lwec"]))).toBe(false);
  });

  it("pruneSelection drops docs the scope hid", () => {
    expect(pruneSelection(new Set(["lwec", "cookery"]), new Set(["cookery"]))).toEqual(
      new Set(["cookery"]),
    );
  });
});

describe("cropCSS", () => {
  it("windows the scan to the bbox with exact percentages", () => {
    expect(cropCSS([0.1, 0.2, 0.4, 0.1])).toEqual({
      width: "250%",
      left: "-25%",
      top: "-200%",
    });
  });

  it("rejects degenerate boxes", () => {
    expect(cropCSS([0.5, 0.5, 0, 0.1])).toBeNull();
    expect(cropCSS([0.5, 0.5, 0.1, 0])).toBeNull();
  });
});

describe("impliedTitle", () => {
  it("prefers the explicit title", () => {
    expect(impliedTitle("  a claim ", "body text")).toBe("a claim");
  });

  it("falls back to the body's first line, truncated", () => {
    expect(impliedTitle("", "  first line\nsecond line")).toBe("first line");
    expect(impliedTitle("", `${"x".repeat(100)}\nrest`)).toBe(`${"x".repeat(79)}…`);
    expect(impliedTitle("", "")).toBe("");
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
