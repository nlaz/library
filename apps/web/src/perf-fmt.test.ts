import { describe, expect, it } from "vitest";

import { bytes, median, memTotal, ms, opt, us } from "./perf-fmt";
import type { MemoryBreakdown } from "./types";

describe("duration formatting", () => {
  it("rolls µs over to ms at 1000", () => {
    expect(us(999)).toBe("999µs");
    expect(us(1000)).toBe("1.0ms");
    expect(us(29200)).toBe("29.2ms");
  });

  it("rolls ms over to seconds at 1000", () => {
    expect(ms(0)).toBe("0ms");
    expect(ms(450.4)).toBe("450ms");
    expect(ms(999)).toBe("999ms");
    expect(ms(1000)).toBe("1.00s");
    expect(ms(12_340)).toBe("12.34s");
  });
});

describe("bytes", () => {
  it("uses binary units with one decimal past KiB", () => {
    expect(bytes(0)).toBe("0B");
    expect(bytes(1023)).toBe("1023B");
    expect(bytes(1024)).toBe("1.0KiB");
    expect(bytes(32 * 1024 * 1024)).toBe("32.0MiB");
    expect(bytes(1_610_612_736)).toBe("1.5GiB");
  });

  // load-bearing: the unaccounted remainder can be negative and must not
  // render as a giant positive number or NaN
  it("keeps the sign on negative values", () => {
    expect(bytes(-1024)).toBe("-1.0KiB");
  });
});

describe("memTotal", () => {
  const m = (footprint: number | null, rss: number | null) =>
    ({ footprint_bytes: footprint, rss_bytes: rss }) as MemoryBreakdown;

  // The ordering is the whole fix. rss excludes compressed pages, so an idle
  // app reports a fraction of what it holds — picking it over footprint is
  // what made the panel show accounted=205MiB against rss=23.5MiB and an
  // unaccounted remainder of -181.6MiB. Mirrors HostMem::total in Rust.
  it("prefers footprint over rss", () => {
    expect(memTotal(m(500, 23))).toBe(500);
  });

  it("falls back to rss when the host had no footprint probe", () => {
    expect(memTotal(m(null, 23))).toBe(23);
  });

  it("is null when neither probe landed", () => {
    expect(memTotal(m(null, null))).toBe(null);
  });

  // a real zero must not read as "no probe" and silently promote rss
  it("keeps a zero footprint rather than falling through", () => {
    expect(memTotal(m(0, 23))).toBe(0);
  });
});

describe("opt", () => {
  // load-bearing: a stage that ran and took no measurable time must not read
  // the same as a stage that was never recorded
  it("distinguishes a real 0 from not-recorded", () => {
    expect(opt(0)).toBe("0");
    expect(opt(undefined)).toBe("—");
    expect(opt(null)).toBe("—");
  });
});

describe("median", () => {
  it("returns null on empty rather than NaN", () => {
    expect(median([])).toBeNull();
  });

  it("averages the middle pair on even counts", () => {
    expect(median([3, 1, 2])).toBe(2);
    expect(median([4, 1, 3, 2])).toBe(2.5);
  });
});
