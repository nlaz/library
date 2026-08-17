// The shelf, over the real chrome: when the first-run card holds the pane,
// and which way a book's "⋯" menu opens. Both are decisions the pure modules
// can't make — one is about what else is in the pane, the other about where
// the card landed in the grid. See chrome-fixture.ts for the mounting rules.

import { beforeAll, beforeEach, expect, it, vi } from "vitest";
import { mountChrome } from "./chrome-fixture";
import type { DocInfo, DocStatus } from "./types";

const doc = (over: Partial<DocInfo> = {}): DocInfo => ({
  id: "kant",
  title: "the critique",
  pages: 10,
  collections: [],
  processing: false,
  status: { state: "ready", done: 10, total: 10 } as DocStatus,
  ...over,
});

let docs: DocInfo[] = [];
let home: typeof import("./home");

/** What the browser would measure for the open panel; happy-dom lays nothing
 * out, so the flip decision has to be fed its one input. */
let panelLeft = 40;

/** Doc ids the menu asked Finder to show. */
let revealed: string[] = [];

/** Doc ids whose indexing was cancelled, and what the confirm answers. */
let cancelled: string[] = [];
let confirmAnswer = true;

beforeAll(async () => {
  await mountChrome();
  const state = await import("./state");
  state.setDesktop({
    docs: async () => docs,
    revealDoc: async (id: string) => {
      revealed.push(id);
    },
    confirmCancel: async () => confirmAnswer,
    cancelDoc: async (id: string) => {
      cancelled.push(id);
    },
  } as unknown as typeof import("./tauri"));
  home = await import("./home");

  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(
    () => ({ left: panelLeft }) as DOMRect,
  );
});

beforeEach(() => {
  panelLeft = 40;
  revealed = [];
  cancelled = [];
  confirmAnswer = true;
});

const $home = () => document.getElementById("home")!;
const openMenu = () => {
  $home().querySelector<HTMLElement>(".bmenu")!.click();
  return $home().querySelector<HTMLElement>(".book-menu")!;
};

it("gives the whole pane to the first-run card while the library is empty", async () => {
  docs = [];
  await home.renderHome({});
  // :only-child is what centres it (onboard.css) — anything else in #home
  // silently drops it back into top-left flow
  expect($home().children).toHaveLength(1);
  expect($home().firstElementChild?.className).toBe("onboard");
});

it("drops the first-run card the moment a book is added, still indexing", async () => {
  docs = [doc({ processing: true, pages: 0, status: null })];
  await home.renderHome({});
  expect($home().querySelector(".onboard")).toBeNull();
  // the card's own progress bar is what reports the indexing from here
  expect($home().querySelector(".book.processing .progress")).not.toBeNull();
});

it("a card mid-ingest offers cancel, behind the confirm", async () => {
  const tick = () => new Promise((r) => setTimeout(r, 0));
  docs = [
    doc({ processing: true, pages: 0, status: { state: "queued", done: 0, total: 0 } as DocStatus }),
  ];
  await home.renderHome({});
  const btn = $home().querySelector<HTMLElement>(".book.processing .bcancel");
  expect(btn).not.toBeNull();

  // declining the dialog cancels nothing
  confirmAnswer = false;
  btn!.click();
  await tick();
  expect(cancelled).toEqual([]);

  confirmAnswer = true;
  btn!.click();
  await tick();
  expect(cancelled).toEqual(["kant"]);
});

it("opens a menu leftward when there is room for it", async () => {
  docs = [doc()];
  await home.renderHome({});
  expect(openMenu().classList.contains("rightward")).toBe(false);
});

it("flips a menu rightward when it would hang off the window", async () => {
  docs = [doc()];
  await home.renderHome({});
  panelLeft = -2;
  expect(openMenu().classList.contains("rightward")).toBe(true);
});

it("shows a book's original in Finder and closes the menu behind it", async () => {
  docs = [doc()];
  await home.renderHome({});
  const menu = openMenu();
  const reveal = [...menu.querySelectorAll("button")].find(
    (b) => b.textContent === "Show in Finder",
  )!;
  expect(reveal).toBeDefined();
  reveal.click();
  await Promise.resolve();
  expect(revealed).toEqual(["kant"]);
  // the menu is done once it has acted — nothing left hanging over the shelf
  expect($home().querySelector(".book-menu")).toBeNull();
});
