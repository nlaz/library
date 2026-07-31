// Navigation, over the real chrome and the real modules: where ← goes, what
// Escape unwinds, and the two ways a live query used to strand a surface.
// See chrome-fixture.ts for the mounting rules these depend on.

import { beforeAll, expect, it } from "vitest";
import { mountChrome, type Chrome } from "./chrome-fixture";
import type { CardRec } from "./types";

/** happy-dom fires hashchange a tick after the assignment. */
const tick = () => new Promise((r) => setTimeout(r, 0));

const card = (over: Partial<CardRec> = {}): CardRec => ({
  id: "card-1",
  title: "on the antinomies",
  body: "the third one, mostly",
  evidence: [{ doc: "kant", page: 12, kind: "region", bbox: [0.1, 0.1, 0.2, 0.2] }],
  links: [],
  created: 1_700_000_000,
  modified: 1_700_000_000,
  filed: false,
  split_hinted: false,
  ...over,
});

const CARDS = [card(), card({ id: "card-2", title: "a second thought" })];

let c: Chrome;
let nav: typeof import("./nav");
let notebox: typeof import("./notebox");
let search: typeof import("./search");
let sheet: typeof import("./sheet");

beforeAll(async () => {
  // the web build reaches marginalia over fetch; answer the reads only
  globalThis.fetch = (async (url: string) => ({
    ok: true,
    status: 200,
    json: async () => (String(url).startsWith("/api/cards") ? CARDS : {}),
    text: async () => "",
  })) as unknown as typeof fetch;

  location.hash = "#/";
  c = await mountChrome();
  nav = await import("./nav");
  notebox = await import("./notebox");
  search = await import("./search");
  sheet = await import("./sheet");
});

/** Land on a surface the way the app does — hash first, then its opener. */
async function go(hash: string, open?: () => Promise<void> | void) {
  location.hash = hash;
  await tick();
  await open?.();
}

it("a standing query does not bounce the ledger back to the library", async () => {
  await go("#/notes", () => notebox.openNotes(null));
  // what route() does after every navigation: re-scope the query in the box.
  // It used to run showSearch(true), which bounced any open ledger to "#/" —
  // so the notes glyph did nothing whenever the box had text in it.
  c.q().value = "kant";
  search.sendQuery();

  expect(notebox.notesOpen()).toBe(true);
  expect(location.hash).toBe("#/notes");
});

it("a query typed over the sheet leaves for the grid instead of hiding under it", async () => {
  notebox.closeNotes();
  await go("#/notes/new", () => sheet.openSheet("new", null));
  expect(document.querySelector("main")!.hidden).toBe(true); // the sheet owns the page

  c.q().value = "kant";
  c.q().dispatchEvent(new Event("input", { bubbles: true }));
  expect(location.hash).toBe("#/");

  // ...and once that navigation lands, the grid is on screen to receive it
  sheet.closeSheet();
  await tick();
  search.sendQuery();
  expect(document.querySelector("main")!.hidden).toBe(false);
  expect(c.el("search").hidden).toBe(false);
  c.q().value = "";
});

it("← returns to the surface each one was entered from", async () => {
  await go("#/");
  await go("#/notes");
  await go("#/read/kant?p=12");

  nav.goBack("#/");
  await tick();
  expect(location.hash).toBe("#/notes"); // not the shelves

  nav.goBack("#/");
  await tick();
  expect(location.hash).toBe("#/");

  nav.goBack("#/"); // trail spent — the fallback holds
  await tick();
  expect(location.hash).toBe("#/");
});

it("a note started while reading walks back to the book, not the ledger", async () => {
  await go("#/");
  await go("#/read/kant?p=12");
  await go("#/notes/new", () => sheet.openSheet("new", null));
  expect(c.el("sheet-back").textContent).toBe("← Kant"); // it says so, too

  c.el("sheet-back").click();
  await tick();
  expect(location.hash).toBe("#/read/kant?p=12");
  sheet.closeSheet();
});

it("Escape unwinds the ledger one layer per press", async () => {
  await go("#/");
  await go("#/notes", () => notebox.openNotes("card-1"));
  (document.querySelector(".docitem") as HTMLButtonElement).click(); // a rail filter
  expect(document.querySelector(".lentry.active")).not.toBeNull();
  expect(document.querySelector(".docitem.sel")).not.toBeNull();

  c.press("Escape"); // the open entry
  expect(document.querySelector(".lentry.active")).toBeNull();
  expect(document.querySelector(".docitem.sel")).not.toBeNull();
  expect(location.hash).toBe("#/notes");

  c.press("Escape"); // then the filters
  expect(document.querySelector(".docitem.sel")).toBeNull();
  expect(location.hash).toBe("#/notes");

  c.press("Escape"); // then the ledger itself
  await tick();
  expect(location.hash).toBe("#/");
});

it("Escape in the reader closes the drawer before it closes the book", async () => {
  const reader = await import("./reader");
  const drawer = await import("./drawer");
  drawer.initDrawer({
    currentDoc: reader.readerDoc,
    getDoc: async (id) => ({ id, title: null, pages: 3, collections: [], status: null }),
    getCollections: async () => ({}),
    prettify: (id) => id,
    edit: null,
    onChanged: () => {},
    onError: () => {},
  });

  await go("#/");
  notebox.closeNotes(); // route() does this on the way to a book
  await go("#/read/kant", () => reader.openReader("kant", 3, 1, "Kant"));
  c.el("reader-meta").click();
  await tick();
  expect(c.el("reader-drawer").hidden).toBe(false);

  c.press("Escape"); // the drawer...
  expect(c.el("reader-drawer").hidden).toBe(true);
  expect(reader.readerOpen()).toBe(true);
  expect(location.hash).toBe("#/read/kant");

  c.press("Escape"); // ...then the book
  await tick();
  expect(location.hash).toBe("#/");
  reader.closeReader();
});

it("the open entry carries a visible way to write in it", async () => {
  await go("#/notes", () => notebox.openNotes("card-1"));
  const edit = document.querySelector<HTMLButtonElement>(".lentry.active .ledit");
  expect(edit?.textContent).toContain("edit");

  edit!.click();
  expect(location.hash).toBe("#/notes/edit?card=card-1");
});
