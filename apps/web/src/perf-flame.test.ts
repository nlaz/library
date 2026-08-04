import { describe, expect, it } from "vitest";

import { labelMode, layout, spanMedian, summary } from "./perf-flame";
import type { Span } from "./types";

const span = (name: string, at_us: number, us: number, depth = 1, track = 0): Span => ({
  name,
  at_us,
  us,
  depth,
  track,
});

/** A hybrid+img search: both tracks running at once, the ranker's four
 * children under the fuse+resolve that encloses them. */
function hybrid(): Span[] {
  return [
    span("ese_embed", 0, 1800),
    span("term_expand", 1800, 100),
    span("lex_search", 1900, 3100),
    span("vec_search", 5000, 2200),
    span("fuse", 7200, 300, 2),
    span("maxsim", 7500, 7300, 2),
    span("mmr", 14800, 400, 2),
    span("resolve", 15200, 600, 2),
    span("fuse+resolve", 7200, 8600),
    span("text_track", 0, 15900, 0),
    span("clip_embed", 100, 4400, 1, 1),
    span("image_search", 4500, 9100, 1, 1),
    span("img_track", 100, 13500, 0, 1),
    span("blend", 16000, 40, 0),
  ];
}

describe("layout", () => {
  it("places blocks as a share of the whole search", () => {
    const f = layout([span("lex_search", 2500, 5000)], 10_000);
    const [b] = f.blocks;
    expect(f.span_us).toBe(10_000);
    expect(b.left).toBe(25);
    expect(b.width).toBe(50);
  });

  // Spans are timed inside answer() and total_us outside it, so a child can
  // land a microsecond past the total. A block drawn past 100% would hang off
  // the end of its own root.
  it("stretches the timeline rather than overflowing the root", () => {
    const f = layout([span("resolve", 9000, 2000)], 10_000);
    expect(f.span_us).toBe(11_000);
    expect(f.blocks[0].left + f.blocks[0].width).toBeCloseTo(100);
    for (const b of f.blocks) expect(b.left + b.width).toBeLessThanOrEqual(100.001);
  });

  it("never draws a block too narrow to hit", () => {
    const f = layout([span("blend", 0, 40), span("maxsim", 40, 20_000)], 20_040);
    expect(f.blocks[0].width).toBeGreaterThanOrEqual(0.5);
  });

  it("survives a zero-duration span", () => {
    const f = layout([span("term_expand", 100, 0)], 1000);
    expect(f.blocks[0].width).toBe(0.5);
    expect(f.blocks[0].self_us).toBe(0);
  });

  it("has nothing to lay out for a record with no spans", () => {
    const f = layout([], 1000);
    expect(f.blocks).toEqual([]);
    expect(f.lanes).toEqual([]);
    expect(f.rows).toBe(1);
  });
});

describe("layout lanes", () => {
  // The reason this is a flame chart and not a waterfall: the two tracks run
  // on concurrent threads, so their spans overlap in x. Stacking them by
  // depth alone would draw a parent/child relationship that doesn't exist.
  it("gives each track its own lane below the last", () => {
    const f = layout(hybrid(), 16_100);
    const row = (n: string) => f.blocks.find((b) => b.name === n)!.row;

    // text lane: root is row 0, so track/stage/child are 1/2/3
    expect(row("text_track")).toBe(1);
    expect(row("lex_search")).toBe(2);
    expect(row("maxsim")).toBe(3);
    // image lane clears the text lane's deepest row plus a blank one
    expect(row("img_track")).toBe(5);
    expect(row("clip_embed")).toBe(6);
    expect(f.rows).toBe(7);
  });

  it("keeps the concurrent tracks overlapping in time", () => {
    const f = layout(hybrid(), 16_100);
    const text = f.blocks.find((b) => b.name === "text_track")!;
    const img = f.blocks.find((b) => b.name === "img_track")!;
    expect(img.left).toBeLessThan(text.left + text.width);
    expect(img.row).toBeGreaterThan(text.row);
  });

  // An images-only query records no text spans at all; the lane numbering
  // must not assume track 0 is present.
  it("handles a lone image track", () => {
    const f = layout(
      [span("clip_embed", 0, 4400, 1, 1), span("img_track", 0, 4400, 0, 1)],
      4500,
    );
    expect(f.blocks.find((b) => b.name === "img_track")!.row).toBe(1);
    expect(f.rows).toBe(3);
  });
});

describe("self_us", () => {
  it("is a parent's time minus its direct children", () => {
    const f = layout(hybrid(), 16_100);
    const self = (n: string) => f.blocks.find((b) => b.name === n)!.self_us;
    // 8600 total, children 300 + 7300 + 400 + 600 = 8600
    expect(self("fuse+resolve")).toBe(0);
    // the track's own time: 15900 minus the five depth-1 spans under it
    expect(self("text_track")).toBe(15_900 - (1800 + 100 + 3100 + 2200 + 8600));
  });

  it("is the whole span when nothing nests inside it", () => {
    const f = layout(hybrid(), 16_100);
    expect(f.blocks.find((b) => b.name === "maxsim")!.self_us).toBe(7300);
  });

  // Emission order carries no meaning — a span closes when its work ends, so
  // a parent trails its children in the array. Containment, not adjacency, is
  // what makes a child a child.
  it("finds children that were emitted after their parent", () => {
    const f = layout(
      [span("maxsim", 100, 500, 2), span("fuse+resolve", 0, 1000, 1)],
      1000,
    );
    expect(f.blocks.find((b) => b.name === "fuse+resolve")!.self_us).toBe(500);
  });

  it("does not subtract a grandchild twice", () => {
    const f = layout(
      [span("text_track", 0, 1000, 0), span("fuse+resolve", 0, 800, 1), span("maxsim", 0, 600, 2)],
      1000,
    );
    // text_track's only direct child is fuse+resolve; maxsim is one deeper
    expect(f.blocks.find((b) => b.name === "text_track")!.self_us).toBe(200);
  });

  // A span on the other lane overlapping in time is not a child of anything
  // on this one, however the depths line up.
  it("ignores a span on another track", () => {
    const f = layout(
      [span("text_track", 0, 1000, 0), span("clip_embed", 0, 400, 1, 1)],
      1000,
    );
    expect(f.blocks.find((b) => b.name === "text_track")!.self_us).toBe(1000);
  });
});

describe("summary", () => {
  it("is the depth-1 spans in time order", () => {
    expect(summary(hybrid()).map((s) => s.name)).toEqual([
      "ese_embed",
      "clip_embed",
      "term_expand",
      "lex_search",
      "image_search",
      "vec_search",
      "fuse+resolve",
    ]);
  });
});

describe("labelMode", () => {
  it("shows name and duration when both fit", () => {
    expect(labelMode("lex_search", "3.1ms", 200)).toBe("full");
  });

  it("drops the duration before the name", () => {
    expect(labelMode("lex_search", "3.1ms", 80)).toBe("name");
  });

  // Two stray glyphs on a 6px block read as corruption, not as a small span.
  // The popover still names it, so nothing is lost by showing nothing.
  it("shows nothing on a block too thin for its own name", () => {
    expect(labelMode("lex_search", "3.1ms", 20)).toBe("none");
    expect(labelMode("maxsim", "59.7ms", 6)).toBe("none");
  });

  it("scales the threshold with the name's length", () => {
    // the same width holds "fuse" but not "fuse+resolve"
    expect(labelMode("fuse", "1µs", 40)).toBe("name");
    expect(labelMode("fuse+resolve", "1µs", 40)).toBe("none");
  });
});

describe("spanMedian", () => {
  it("summarizes one stage across the ring", () => {
    const recs = [
      { spans: [span("maxsim", 0, 7000, 2)] },
      { spans: [span("maxsim", 0, 9000, 2)] },
      { spans: [span("maxsim", 0, 8000, 2)] },
    ];
    expect(spanMedian(recs, "maxsim")).toBe(8000);
  });

  it("skips records the stage never ran in", () => {
    const recs = [{ spans: [span("lex_search", 0, 100)] }, { spans: [span("maxsim", 0, 500, 2)] }];
    expect(spanMedian(recs, "maxsim")).toBe(500);
    expect(spanMedian(recs, "vec_search")).toBeNull();
  });
});
