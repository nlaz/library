// The card catalog over the real chrome and the real modules. Most of this
// file is about one thing: ⌘K opens from everywhere, and Escape closes it and
// nothing else. That is the property the rest of the feature is built on, and
// the only one that cannot be fixed later without breaking someone's habits.
//
// See chrome-fixture.ts for the mounting rules these depend on — in
// particular that keys are dispatched from document.activeElement, which is
// exactly what makes the "opens from inside a text field" cases real.

import { beforeAll, beforeEach, expect, it } from "vitest";
import { type Chrome, mountChrome } from "./chrome-fixture";
import type { CardRec, Collections } from "./types";

const tick = () => new Promise((r) => setTimeout(r, 0));

let c: Chrome;
let palette: typeof import("./palette");
/** Collections the catalog asked main.ts to apply. */
const chosen: string[] = [];
/** Books the catalog reported as edited. */
const changed: string[] = [];

/** ⌘K from whatever currently has focus. */
const cmdK = () => c.press("k", { metaKey: true });

const rows = () => [...document.querySelectorAll("#pal-list .pal-row")];
const labels = () => rows().map((r) => r.querySelector(".pal-label")!.textContent);
const selected = () => document.querySelector("#pal-list .pal-row.on");
const crumb = () => document.getElementById("pal-crumb")!;

function type(v: string) {
  const q = document.getElementById("pal-q") as HTMLInputElement;
  q.value = v;
  q.dispatchEvent(new Event("input", { bubbles: true }));
}

const CARDS: CardRec[] = [
  {
    id: "card-1",
    title: "the gramophone as a writing machine",
    body: "",
    evidence: [],
    links: [],
    created: 1_700_000_000,
    modified: 1_700_000_000,
    filed: false,
    split_hinted: false,
  },
];

const COLS: Collections = {
  "Media Theory": ["kittler-gramophone", "mcluhan"],
  Photography: ["sontag"],
};

beforeAll(async () => {
  // the web build reaches marginalia over fetch; answer the reads only
  globalThis.fetch = (async (url: string) => ({
    ok: true,
    status: 200,
    json: async () => (String(url).startsWith("/api/cards") ? CARDS : {}),
    text: async () => "",
  })) as unknown as typeof fetch;

  location.hash = "#/";
  c = await mountChrome({ collections: async () => COLS });
  palette = await import("./palette");
  palette.initPalette({
    chooseCol: (col) => {
      chosen.push(col);
    },
    onDocChanged: (doc) => {
      changed.push(doc);
    },
  });
});

beforeEach(() => {
  palette.closePalette();
});

it("⌘K opens the catalog and puts the caret in it", () => {
  expect(c.el("palette").hidden).toBe(true);

  expect(cmdK()).toBe(false); // preventDefault: the webview gets nothing
  expect(palette.paletteOpen()).toBe(true);
  expect(document.activeElement).toBe(c.el("pal-q"));
  expect(labels().length).toBeGreaterThan(0);
});

it("⌘K opens from inside the search box — the field cannot swallow it", () => {
  // #q calls stopPropagation on every keydown it sees, which is why the
  // catalog listens on window in the capture phase rather than on document
  c.q().focus();
  expect(document.activeElement).toBe(c.q());

  cmdK();
  expect(palette.paletteOpen()).toBe(true);
});

it("⌘K closes it again, and gives the caret back to where it came from", () => {
  c.q().focus();
  cmdK();
  expect(document.activeElement).toBe(c.el("pal-q"));

  cmdK();
  expect(palette.paletteOpen()).toBe(false);
  expect(document.activeElement).toBe(c.q());
});

it("Escape closes the catalog and nothing underneath it", async () => {
  const reader = await import("./reader");
  const notebox = await import("./notebox");
  notebox.closeNotes();
  location.hash = "#/read/kant";
  await tick();
  reader.openReader("kant", 3, 1, "Kant");
  expect(reader.readerOpen()).toBe(true);
  // reading, not typing: #q swallows every key it is sent, so leaving the
  // caret there would test the search box rather than the reader
  (document.activeElement as HTMLElement | null)?.blur();

  cmdK();
  expect(palette.paletteOpen()).toBe(true);

  // one Escape, one layer: the book the catalog was opened over is still open
  expect(c.press("Escape")).toBe(false);
  expect(palette.paletteOpen()).toBe(false);
  expect(reader.readerOpen()).toBe(true);
  expect(location.hash).toBe("#/read/kant");

  // and the next Escape reaches the reader, as it always did. It leaves by
  // navigating; main.ts's route() is what closes the surface, and the fixture
  // deliberately never boots it — so the hash is the observable here
  c.press("Escape");
  await tick();
  expect(location.hash).toBe("#/");
  reader.closeReader();
});

it("Escape over the search popover closes the catalog, not the query", async () => {
  const viewer = await import("./viewer");
  viewer.openSearchPop();
  c.q().value = "kant";
  expect(c.el("search-pop").hidden).toBe(false);

  cmdK();
  c.press("Escape");

  expect(palette.paletteOpen()).toBe(false);
  expect(c.el("search-pop").hidden).toBe(false);
  expect(c.q().value).toBe("kant"); // viewer.ts's Escape would have cleared it
  c.q().value = "";
});

it("typing narrows the list, and the arrows walk what is left", async () => {
  cmdK();
  await tick();
  const all = labels().length;

  type("search the library");
  expect(labels().length).toBeLessThan(all);
  expect(labels()[0]).toContain("Search the library");

  // the first row is selected the moment the list changes — Enter always has
  // a target, so the query can be answered without ever pressing an arrow
  expect(selected()).toBe(rows()[0]);

  type("o"); // loose enough to reach past the commands into the books
  const n = rows().length;
  expect(n).toBeGreaterThan(1);
  c.press("ArrowDown");
  expect(selected()).toBe(rows()[1]);
  c.press("ArrowUp");
  c.press("ArrowUp"); // off the top, round to the bottom
  expect(selected()).toBe(rows()[n - 1]);
});

it("offers three verbs and no more — the catalog is not a menu", () => {
  // the desktop-only Settings is absent from this build, so two here; what
  // matters is that the box opens onto a list you do not have to read
  cmdK();
  expect(labels()).toEqual(["Search the library", "Notes"]);

  // and the verbs that lost their row kept their chord and their handler:
  // they are simply not things you come to this box looking for
  type("performance");
  expect(labels()).not.toContain("Performance");
  type("keyboard");
  expect(labels()).not.toContain("Keyboard shortcuts");
  type("start a note");
  expect(labels()).not.toContain("Start a note");
});

it("renders one flat list — no group headings", async () => {
  cmdK();
  await tick();
  type("kittler");

  expect(document.querySelectorAll("#pal-list .pal-group")).toHaveLength(0);
  // every child of the list is a row; nothing is there to be read past
  expect(document.querySelectorAll("#pal-list li").length).toBe(rows().length);
});

it("a command the web build cannot honour is absent, not broken", () => {
  // desktop-only, and this suite has no Tauri handle — the row must not be
  // sitting there waiting to fail, the same way the drawer takes no edit path
  cmdK();
  type("settings");
  expect(labels().join(" ")).not.toContain("Settings");
});

it("finds a shelf, and filtering by it is the same act as clicking its tab", async () => {
  cmdK();
  await tick(); // the collections arrive a promise after the box opens

  type("photog");
  expect(labels()[0]).toBe("Photography");
  expect(rows()[0].querySelector(".pal-meta")!.textContent).toBe("1 book");

  chosen.length = 0;
  c.press("Enter");
  expect(chosen).toEqual(["Photography"]);
});

it("finds a book by a word from the middle of its title", async () => {
  cmdK();
  await tick();

  // no `docs` command in the web build, so the shelves are the census and
  // the prettified id is the name
  type("gramo");
  expect(labels()).toContain("Kittler Gramophone");

  type("kittler");
  expect(labels()[0]).toBe("Kittler Gramophone");
  c.press("Enter");
  await tick();
  expect(location.hash).toBe("#/read/kittler-gramophone");
  location.hash = "#/";
  await tick();
});

it("finds a note, and lands on it in the ledger", async () => {
  cmdK();
  await tick();

  type("writing machine");
  expect(labels()[0]).toContain("writing machine");

  c.press("Enter");
  await tick();
  expect(location.hash).toBe("#/notes?card=card-1");
  location.hash = "#/";
  await tick();
});

it("a query with no answer is handed to the search that can answer it", () => {
  // the catalog matches names; when no name matches, the books themselves
  // might still. It never shows an empty list with nothing to do in it.
  cmdK();
  type("qqqzzz");
  expect(labels()[0]).toContain("Search the library for");

  c.press("Enter");
  expect(palette.paletteOpen()).toBe(false);
  expect(c.el("search-pop").hidden).toBe(false);
  expect(c.q().value).toBe("qqqzzz");
  expect(c.sent.at(-1)!.q).toBe("qqqzzz");
  c.q().value = "";
});

it("Enter runs the selected row, and the catalog gets out of the way first", async () => {
  const notebox = await import("./notebox");
  notebox.closeNotes();

  cmdK();
  type("search the library");
  expect(labels()[0]).toContain("Search the library");

  c.press("Enter");
  expect(palette.paletteOpen()).toBe(false);
  expect(c.el("search-pop").hidden).toBe(false);
  c.q().value = "";

  // ...and the command drops off the list while its own view is open, rather
  // than sitting there ready to close something behind the scrim
  await notebox.openNotes(null);
  cmdK();
  expect(labels()).not.toContain("Notes");

  palette.closePalette();
  notebox.closeNotes();
});

it("the catalog stays global — the reader does not add a section to it", async () => {
  // "this book · …" is cut for now. A book's verbs are still one ⇥ away from
  // its own row; what is gone is the box changing shape under the user
  // depending on which surface it was opened over.
  const reader = await import("./reader");
  (await import("./notebox")).closeNotes();
  location.hash = "#/read/kittler-gramophone";
  await tick();
  reader.openReader("kittler-gramophone", 3, 1, "Kittler Gramophone");
  (document.activeElement as HTMLElement | null)?.blur();

  cmdK();
  expect(labels()).toEqual(["Search the library", "Notes"]);
  expect(labels()).not.toContain("Markup mode");

  palette.closePalette();
  reader.closeReader();
  location.hash = "#/";
  await tick();
});

it("an empty box does not offer where you have just been", async () => {
  // the trail is cut too: with the verbs capped at three, an empty box that
  // also replayed five recents would be back to being a menu
  const reader = await import("./reader");
  (await import("./notebox")).closeNotes();
  location.hash = "#/";
  await tick();
  location.hash = "#/read/sontag?p=12";
  await tick();
  reader.openReader("sontag", 20, 12, "Sontag");
  location.hash = "#/notes";
  await tick();
  reader.closeReader();
  (document.activeElement as HTMLElement | null)?.blur();

  cmdK();
  expect(labels()).not.toContain("Sontag");

  palette.closePalette();
  location.hash = "#/";
  await tick();
});

it("⇥ drills into a book's verbs, and Escape backs out one level at a time", async () => {
  cmdK();
  await tick();
  type("kittler");
  expect(labels()[0]).toBe("Kittler Gramophone");

  // the trail appears only now that there is one — at the root the card has
  // no header at all
  expect(crumb().hidden).toBe(true);
  expect(c.press("Tab")).toBe(false);
  expect(crumb().hidden).toBe(false);
  expect(crumb().textContent).toContain("Kittler Gramophone");
  expect(labels()).toContain("Open in the reader");
  expect((c.el("pal-q") as HTMLInputElement).value).toBe(""); // a fresh field per stage

  // one Escape backs out of the stage, exactly as it unwinds every other
  // layer in this app — it does not close the whole box
  c.press("Escape");
  expect(palette.paletteOpen()).toBe(true);
  expect(crumb().hidden).toBe(true);

  c.press("Escape");
  expect(palette.paletteOpen()).toBe(false);
});

it("⌫ on an empty field backs out too, but never eats a character", async () => {
  cmdK();
  await tick();
  type("kittler");
  c.press("Tab");
  expect(crumb().textContent).toContain("Kittler Gramophone");

  type("open"); // there is something to delete: ⌫ belongs to the text
  expect(c.press("Backspace")).toBe(true);
  expect(crumb().textContent).toContain("Kittler Gramophone");

  type("");
  expect(c.press("Backspace")).toBe(false);
  expect(crumb().hidden).toBe(true);
  palette.closePalette();
});

it("an empty box reports what the library is still chewing on", async () => {
  const format = await import("./format");
  format.setDocList([
    {
      id: "berger",
      title: "Ways of Seeing",
      pages: 176,
      collections: [],
      processing: true,
      status: { state: "preparing", stage: "ocr", done: 41, total: 176, updated: 0 },
    },
    {
      id: "bad-scan",
      title: "Anon scan",
      pages: 0,
      collections: [],
      processing: false,
      status: { state: "failed", done: 0, total: 0, updated: 0, error: "no text" },
    },
  ]);

  cmdK();
  // the failure leads: it is the one that needs a person
  expect(labels()[0]).toBe("Anon scan");
  expect(labels()[1]).toBe("Ways of Seeing");
  expect(rows()[1].querySelector(".pal-meta")!.textContent).toBe("reading pages 41/176");

  // and once there is a name to match, the book's own row carries the state
  // instead — this is not a second copy of the library
  type("ways of seeing");
  expect(labels()).toEqual(["Ways of Seeing"]);

  palette.closePalette();
  format.setDocList([]);
});

it("the document-level ? key stands down while the catalog is open", async () => {
  const keys = await import("./keys");
  cmdK();

  // it guards on the event target being an input, and the catalog's field is
  // a real one — this is why it is an <input> and not a contenteditable
  c.press("?");
  expect(keys.keysOpen()).toBe(false);
});

// Keep this test last: setDesktop cannot be undone, and the rest of the file
// exercises the browser build's verb-less rows.
it("a book still indexing gets cancel where delete would be", async () => {
  const format = await import("./format");
  const state = await import("./state");
  const cancelled: string[] = [];
  state.setDesktop({
    cancelDoc: async (id: string) => {
      cancelled.push(id);
    },
    // once desktop is set, marginalia reads route through it too
    listCards: async () => CARDS,
  } as unknown as NonNullable<typeof state.desktop>);
  format.setDocList([
    {
      id: "berger",
      title: "Ways of Seeing",
      pages: 176,
      collections: [],
      processing: true,
      status: { state: "preparing", stage: "ocr", done: 41, total: 176, updated: 0 },
    },
  ]);

  cmdK();
  await tick();
  type("ways of seeing");
  c.press("Tab");
  expect(labels()).toContain("Cancel indexing…");
  expect(labels()).not.toContain("Remove from the library…");

  // the confirm is still a stage you walk into, same as remove
  type("cancel");
  c.press("Tab");
  expect(labels()).toEqual(["Stop indexing “Ways of Seeing”"]);
  c.press("Enter");
  await tick();
  expect(cancelled).toEqual(["berger"]);
  expect(changed).toContain("berger");

  format.setDocList([]);
});
