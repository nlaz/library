import { describe, expect, it } from "vitest";

import { bytes, median, ms, opt, us } from "./perf-fmt";

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
