// A page image that fails to load. Under an evictable render cache this is
// an ordinary event — 404 while the render is being made again, or 503
// because the render queue shed the request — and the reader's response to
// it is what makes shedding safe: a refused request is only backpressure if
// the client comes back. Before this the failure left a permanently blank
// box with no retry and no message.
//
// Over the real module, because the retry lives in the same listener that
// sets the aspect ratio and the two must not drift apart.

import { beforeAll, beforeEach, expect, it, vi } from "vitest";
import { mountChrome } from "./chrome-fixture";

let reader: typeof import("./reader");

beforeAll(async () => {
  await mountChrome();
  reader = await import("./reader");
});

beforeEach(() => {
  vi.useFakeTimers();
});

function page(): HTMLElement {
  const el = document.createElement("div");
  el.className = "rpage";
  document.body.append(el);
  return el;
}

/** jsdom never fetches, so failure is dispatched rather than awaited. */
function fail(img: HTMLImageElement) {
  img.dispatchEvent(new Event("error"));
}

it("retries a failed page before giving up on it", () => {
  const el = page();
  const img = reader.loadPage(el, "kant", 4);
  el.append(img);
  const first = img.src;
  expect(first).toContain("page-0004");

  fail(img);
  expect(el.querySelector(".rpage-miss")).toBe(null); // not yet — it retries
  vi.advanceTimersByTime(500);
  expect(img.src).not.toBe(first);
  expect(img.src).toContain("page-0004");

  fail(img);
  vi.advanceTimersByTime(1500);
  expect(el.querySelector(".rpage-miss")).toBe(null); // one more attempt

  fail(img);
  const miss = el.querySelector(".rpage-miss");
  expect(miss).not.toBe(null);
  expect(el.querySelector("img")).toBe(null);
  expect(miss!.textContent).toMatch(/didn't load/);
});

it("offers a way back that doesn't need scrolling out of range", () => {
  const el = page();
  const img = reader.loadPage(el, "kant", 1);
  el.append(img);
  for (let i = 0; i < 3; i++) {
    fail(el.querySelector("img")!);
    vi.advanceTimersByTime(2000);
  }
  const again = el.querySelector<HTMLButtonElement>(".rpage-miss button")!;
  expect(again).not.toBe(null);

  again.click();
  expect(el.querySelector(".rpage-miss")).toBe(null);
  expect(el.querySelector("img")).not.toBe(null);
});

// the retry must not resurrect a page the scroller already recycled, or a
// fast scroll through a broken stretch leaves images attached to nothing
it("abandons the retry once the page has been recycled", () => {
  const el = page();
  const img = reader.loadPage(el, "kant", 2);
  el.append(img);
  const first = img.src;

  fail(img);
  el.replaceChildren(); // out of range: the loader gives the memory back
  vi.advanceTimersByTime(1000);
  // no refetch for a detached page
  expect(img.src).toBe(first);
});
