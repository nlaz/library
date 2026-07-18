// hitKey backs the cross-page `seen` Set in search.ts: page-1 renders seed
// the set, continuation appends drop any hit whose key is already present
// (each continuation is its own fjall snapshot, so slices can drift).
// These tests pin the key's identity semantics.

import { describe, expect, it } from "vitest";
import { hitKey } from "./format";
import type { WireHit } from "./types";

const hit = (over: Partial<WireHit> = {}): WireHit => ({
  kind: "text",
  score: 0.5,
  doc: "moby-dick",
  page: 3,
  idx: 7,
  img: "/pages/moby-dick/page-0003.jpg",
  snippet: [],
  boxes: [],
  crop: [0, 0, 1, 1],
  ...over,
});

describe("hitKey", () => {
  it("is stable for equal hits, even when non-identity fields differ", () => {
    expect(hitKey(hit())).toBe(hitKey(hit()));
    // score/snippet/boxes drift between snapshots; identity must not
    expect(hitKey(hit({ score: 0.9, snippet: [{ t: "whale", m: true }] }))).toBe(hitKey(hit()));
  });

  it("differs when any identity field (kind/doc/page/idx) differs", () => {
    const base = hitKey(hit());
    expect(hitKey(hit({ kind: "image" }))).not.toBe(base);
    expect(hitKey(hit({ doc: "art-of-war" }))).not.toBe(base);
    expect(hitKey(hit({ page: 4 }))).not.toBe(base);
    expect(hitKey(hit({ idx: 8 }))).not.toBe(base);
  });

  it("dedupes drifted continuation slices the way the seen-set does", () => {
    // mirror search.ts: render() seeds `seen` from page-1, appendResults()
    // filters the next slice against it and adds what survives
    const seen = new Set<string>();
    const page1 = [hit({ idx: 1 }), hit({ idx: 2 })];
    for (const h of page1) seen.add(hitKey(h));

    const page2 = [hit({ idx: 2, score: 0.1 }), hit({ idx: 3 }), hit({ kind: "image", idx: 2 })];
    const fresh = page2.filter((h) => !seen.has(hitKey(h)));
    expect(fresh.map((h) => `${h.kind}:${h.idx}`)).toEqual(["text:3", "image:2"]);
  });
});
