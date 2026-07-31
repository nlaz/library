// The shelf, over the real chrome: which way a book's "⋯" menu opens. That
// is the one thing the stylesheet can't decide on its own — which column a
// card landed in is a grid autoflow fact CSS won't name. See
// chrome-fixture.ts for the mounting rules this depends on.

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

beforeAll(async () => {
  await mountChrome();
  const state = await import("./state");
  state.setDesktop({
    docs: async () => docs,
  } as unknown as typeof import("./tauri"));
  home = await import("./home");

  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(
    () => ({ left: panelLeft }) as DOMRect,
  );
});

beforeEach(() => {
  panelLeft = 40;
});

const $home = () => document.getElementById("home")!;
const openMenu = () => {
  $home().querySelector<HTMLElement>(".bmenu")!.click();
  return $home().querySelector<HTMLElement>(".book-menu")!;
};

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
