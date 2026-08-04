import { describe, expect, it } from "vitest";

import { PAGE_RETRIES, pageErrorText, pageRetryDelay } from "./reader-model";

describe("pageRetryDelay", () => {
  // Load-bearing once the server sheds render requests under load: a 503 is
  // only backpressure if the client comes back for it. Before this, a failed
  // page image was a permanently blank box.
  it("retries a bounded number of times, then gives up", () => {
    expect(pageRetryDelay(1)).toBe(400);
    expect(pageRetryDelay(2)).toBe(1200);
    expect(pageRetryDelay(PAGE_RETRIES + 1)).toBe(null);
  });

  it("backs off rather than re-flooding the queue that shed it", () => {
    const first = pageRetryDelay(1)!;
    const second = pageRetryDelay(2)!;
    expect(second).toBeGreaterThan(first);
  });

  // the reader prefetches ~3 viewports ahead, roughly 1-2s of runway at a
  // normal scroll — both retries have to fit inside it or the hole is seen
  it("fits both attempts inside the reader's prefetch runway", () => {
    let total = 0;
    for (let a = 1; a <= PAGE_RETRIES; a++) total += pageRetryDelay(a)!;
    expect(total).toBeLessThanOrEqual(2000);
  });

  it("refuses nonsense attempt numbers", () => {
    expect(pageRetryDelay(0)).toBe(null);
    expect(pageRetryDelay(-1)).toBe(null);
  });
});

describe("pageErrorText", () => {
  it("names the cause when the protocol gave us one", () => {
    expect(pageErrorText("no-source")).toMatch(/moved/);
    expect(pageErrorText("offline")).toMatch(/iCloud/);
    expect(pageErrorText("render-failed")).toMatch(/recreated/);
  });

  // an img error event carries no headers, so this is the common case and
  // must not claim to know why
  it("stays vague when there is no reason to report", () => {
    const text = pageErrorText();
    expect(text).toBe(pageErrorText(null));
    expect(text).not.toMatch(/iCloud|moved/);
    expect(text.length).toBeGreaterThan(0);
  });
});
