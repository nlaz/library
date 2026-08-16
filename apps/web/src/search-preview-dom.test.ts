// A result card whose page render is refused, driven through the real search
// modules and the real chrome.
//
// Page images are an evictable cache and the protocol handler sheds a render
// it has no room to start (library-app/src/render.rs), whose own rule is that
// shedding is safe "only because the reader retries". The results grid — the
// caller that comment is about, twenty-odd hits arriving at once — was the one
// surface that never came back, so a shed thumbnail stayed the browser's
// broken-image glyph until the next query. On a library past the page-cache
// budget that was most of the grid at once.
//
// See chrome-fixture.ts for the mounting rules.

import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { type Chrome, mountChrome } from "./chrome-fixture";
import { GRID_RETRIES } from "./page-retry";
import type { WireHit } from "./types";

let app: Chrome;

const hit = (page = 1): WireHit => ({
  kind: "text",
  score: 1,
  doc: "wholeearth",
  page,
  idx: 0,
  img: `/pages/wholeearth/page-${String(page).padStart(4, "0")}.jpg`,
  snippet: [{ t: "sun", m: true }],
  boxes: [[0.1, 0.1, 0.2, 0.05]],
  crop: [0, 0, 1, 1],
});

/** Run a query and hand back the one card's preview image. */
function showHit(page = 1): HTMLImageElement {
  app.q().value = `sun god ${page}`;
  app.q().dispatchEvent(new Event("input", { bubbles: true }));
  app.answer([hit(page)]);
  return document.querySelector<HTMLImageElement>("#results .card img")!;
}

const refuse = (img: HTMLImageElement) => img.dispatchEvent(new Event("error"));

beforeAll(async () => {
  app = await mountChrome();
});

beforeEach(() => {
  vi.useFakeTimers();
});

describe("a preview whose render was shed", () => {
  it("comes back for it, after a wait rather than immediately", () => {
    const img = showHit();
    const first = img.src;
    expect(first).toContain("page-0001.jpg");

    refuse(img);
    // retrying in the error handler would just re-send the flood that was
    // shed — the whole point is to come back once the queue has drained
    expect(img.src).toBe(first);

    vi.advanceTimersByTime(1000);
    expect(img.src).not.toBe(first);
    expect(img.src).toContain("r=1"); // cache-busted, or the browser re-serves the miss
  });

  it("gives up after a bounded number of tries and says what happened", () => {
    const img = showHit(2);
    for (let i = 0; i <= GRID_RETRIES; i++) {
      refuse(document.querySelector<HTMLImageElement>("#results .card img") ?? img);
      vi.advanceTimersByTime(30_000);
    }
    const card = document.querySelector("#results .card")!;
    expect(card.querySelector("img")).toBe(null);
    expect(card.querySelector(".pv-miss")?.textContent).toMatch(/unavailable/);
  });

  // The grid is rebuilt on every settled keystroke. Retrying a card that is
  // no longer in the document spends the render queue on results nobody is
  // looking at — and starves the ones on screen, which is this bug again.
  it("does not retry a card the next query has already replaced", () => {
    const stale = showHit(3);
    const before = stale.src;
    refuse(stale);

    showHit(4);
    expect(stale.isConnected).toBe(false);

    vi.advanceTimersByTime(30_000);
    expect(stale.src).toBe(before);
  });
});
