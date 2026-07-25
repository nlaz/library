import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { GLOSS, evidence } from "./perf-gloss";

const EMPTY = { searches: [], ingest: [], agent: [] };

describe("GLOSS", () => {
  // The header marks terms up as term("MIN_REL", …); the string literal is
  // the only coupling to this registry, so a renamed or new fact would
  // otherwise silently produce a term that pops up nothing.
  it("covers every term the perf renderers mark up", () => {
    const src = ["./src/perf.ts", "./src/perf-agent.ts"]
      .map((p) => readFileSync(new URL(p, `file://${process.cwd()}/`), "utf8"))
      .join("\n");
    const marked = [...src.matchAll(/\bterm\(\s*"([^"]+)"/g)].map((m) => m[1]);

    expect(marked.length).toBeGreaterThan(0);
    expect([...new Set(marked)].filter((t) => !GLOSS[t])).toEqual([]);
  });

  it("gives every term a non-empty reading", () => {
    for (const [k, g] of Object.entries(GLOSS)) {
      expect(g.what.length, `${k} has an empty gloss`).toBeGreaterThan(20);
    }
  });
});

describe("evidence", () => {
  // The header must say nothing rather than "NaN" or "undefined" — an empty
  // ring is the normal state right after opening the view.
  it("stays silent or literal on empty rings", () => {
    for (const k of Object.keys(GLOSS)) {
      const out = evidence(k, EMPTY);
      expect(out === null || typeof out === "string", `${k} returned ${out}`).toBe(true);
      if (out !== null) expect(out).not.toMatch(/NaN|undefined/);
    }
  });

  it("summarizes the relevance floor against the search ring", () => {
    const rec = (rel_killed: number, zero: boolean) =>
      ({ ts_ms: 0, rel_killed, zero, served: 5, lex_n: 10, img_fetched: 0, img_killed: 0, total_us: 1000 }) as never;
    const out = evidence("MIN_REL", {
      ...EMPTY,
      searches: [rec(2, false), rec(6, false), rec(10, true)],
    });
    expect(out).toContain("median of 6");
    expect(out).toContain("1 returned nothing");
  });

  it("returns null for a term with no live signal", () => {
    expect(evidence("emb_dim", EMPTY)).toBeNull();
  });
});
