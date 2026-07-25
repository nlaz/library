import { describe, expect, it } from "vitest";
import { backlinks, splitPoint, timeline, wikiTokens } from "./notebox-model";
import type { CardRec } from "./types";

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
