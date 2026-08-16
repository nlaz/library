import { describe, expect, it } from "vitest";

import { GRID_RETRIES, gridRetryDelay } from "./page-retry";

/** The centre of the jitter spread — the schedule without the noise. */
const mid = () => 0.5;

describe("gridRetryDelay", () => {
  it("retries a bounded number of times, then gives up", () => {
    expect(gridRetryDelay(1, mid)).toBe(500);
    expect(gridRetryDelay(GRID_RETRIES, mid)).toBe(4000);
    expect(gridRetryDelay(GRID_RETRIES + 1, mid)).toBe(null);
  });

  it("backs off rather than re-flooding the queue that shed it", () => {
    for (let a = 2; a <= GRID_RETRIES; a++) {
      expect(gridRetryDelay(a, mid)!).toBeGreaterThan(gridRetryDelay(a - 1, mid)!);
    }
  });

  // A screenful is ~12 cards, and on a library past the page-cache budget
  // nearly all of them miss. Two render workers at ~160ms clear about twelve
  // a second, and the shed ones only start after those — so the window has
  // to outlast several seconds of rasterizing, where the reader's ~1.6s of
  // prefetch runway would give up in the middle of it.
  it("waits long enough for a cold screenful to rasterize", () => {
    let total = 0;
    for (let a = 1; a <= GRID_RETRIES; a++) total += gridRetryDelay(a, mid)!;
    expect(total).toBeGreaterThan(5000);
  });

  // The jitter is the difference between backpressure and a second flood: a
  // dozen cards refused in the same instant must not come back in one.
  it("spreads a wave of retries instead of re-sending it as a wave", () => {
    const earliest = gridRetryDelay(1, () => 0)!;
    const latest = gridRetryDelay(1, () => 1)!;
    expect(earliest).toBeLessThan(latest);
    expect(latest - earliest).toBeGreaterThan(earliest / 2);
  });

  it("refuses nonsense attempt numbers", () => {
    expect(gridRetryDelay(0, mid)).toBe(null);
    expect(gridRetryDelay(-1, mid)).toBe(null);
    expect(gridRetryDelay(1.5, mid)).toBe(null);
  });
});
