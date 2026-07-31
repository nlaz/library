// The metadata drawer's one desktop-only affordance: adding a book moves the
// file into the library folder, so "Show in Finder" — sitting under the file
// id it acts on — is the only way back to the original. Over the real chrome,
// because the button is wired through the drawer's open/render path.

import { beforeAll, beforeEach, expect, it } from "vitest";
import { mountChrome } from "./chrome-fixture";
import type { DrawerDoc } from "./drawer";

const doc: DrawerDoc = { id: "kant", title: null, pages: 3, collections: [], status: null };

let revealed: string[] = [];
let revealFails = false;
let errors: string[] = [];
let drawer: typeof import("./drawer");

beforeAll(async () => {
  await mountChrome();
  drawer = await import("./drawer");
  drawer.initDrawer({
    currentDoc: () => doc.id,
    getDoc: async () => doc,
    getCollections: async () => ({}),
    prettify: (id) => id,
    edit: null,
    reveal: async (id) => {
      revealed.push(id);
      if (revealFails) throw new Error("gone");
    },
    onChanged: () => {},
    onError: (msg) => errors.push(msg),
  });
});

beforeEach(() => {
  revealed = [];
  errors = [];
  revealFails = false;
});

/** Open the drawer the way the reader's ⓘ button does, and hand back its
 * reveal control. The render is a round-trip away (getDoc is async). */
async function openDrawer(): Promise<HTMLElement | null> {
  drawer.closeDrawer(); // ⓘ toggles — an already-open drawer would close
  document.getElementById("reader-meta")!.click();
  await Promise.resolve();
  await Promise.resolve();
  return document.querySelector<HTMLElement>("#reader-drawer .dreveal");
}

it("reveals the doc whose facts the drawer is showing", async () => {
  const btn = await openDrawer();
  expect(btn).not.toBeNull();
  // it belongs to the file row — the id above it is what gets revealed
  expect(btn!.closest(".drow")!.querySelector(".dlabel")!.textContent).toBe("file");
  btn!.click();
  await Promise.resolve();
  expect(revealed).toEqual([doc.id]);
  expect(errors).toEqual([]);
});

it("reports a missing original instead of failing silently", async () => {
  revealFails = true;
  const btn = await openDrawer();
  btn!.click();
  await Promise.resolve();
  await Promise.resolve();
  expect(errors).toEqual(["show in Finder: Error: gone"]);
});
