import { describe, expect, it } from "vitest";
import { forgetDoc, navLabel, popNav, pushNav, surfaceOf } from "./nav-model";

const title = (doc: string) => (doc === "kant" ? "Critique of Pure Reason" : doc);

/** Walk a route, oldest hash first, and return the trail it leaves. */
function walk(...hashes: string[]): string[] {
  let trail: string[] = [];
  for (let i = 1; i < hashes.length; i++) trail = pushNav(trail, hashes[i - 1], hashes[i]);
  return trail;
}

describe("surfaceOf", () => {
  it("reads the five surfaces", () => {
    expect(surfaceOf("#/").kind).toBe("library");
    expect(surfaceOf("#/read/kant?p=12").kind).toBe("read");
    expect(surfaceOf("#/notes?card=7").kind).toBe("notes");
    expect(surfaceOf("#/notes/new").kind).toBe("sheet");
    expect(surfaceOf("#/notes/edit?card=7").kind).toBe("sheet");
    expect(surfaceOf("#/settings").kind).toBe("settings");
  });

  it("keys a book by its doc, decoded", () => {
    expect(surfaceOf("#/read/two%20words?p=3").doc).toBe("two words");
    expect(surfaceOf("#/read/kant?p=1").key).toBe(surfaceOf("#/read/kant?p=99").key);
    expect(surfaceOf("#/read/kant").key).not.toBe(surfaceOf("#/read/hume").key);
  });
});

describe("pushNav", () => {
  it("records the surface a trip started from", () => {
    expect(walk("#/", "#/read/kant")).toEqual(["#/"]);
    expect(walk("#/", "#/notes", "#/read/kant")).toEqual(["#/", "#/notes"]);
  });

  it("ignores movement inside one surface", () => {
    expect(walk("#/read/kant", "#/read/kant?p=90")).toEqual([]);
    expect(walk("#/notes", "#/notes?card=7")).toEqual([]);
  });

  it("never offers a draft as somewhere to return to", () => {
    // leaving the sheet saves and closes it; the hash that opened it is spent
    expect(walk("#/read/kant", "#/notes/new", "#/notes?card=7")).toEqual(["#/read/kant"]);
  });

  it("unwinds to an earlier visit instead of looping", () => {
    // shelves → book → notes → shelves: back from the shelves is not the book
    expect(walk("#/", "#/read/kant", "#/notes", "#/")).toEqual([]);
    expect(walk("#/", "#/read/kant", "#/notes", "#/read/kant")).toEqual(["#/"]);
  });

  it("remembers the book you opened settings from", () => {
    // ⌘S is reachable from anywhere, and ← out of it must not always be
    // the shelves — that was the whole reason this module exists
    expect(walk("#/", "#/read/kant", "#/settings")).toEqual(["#/", "#/read/kant"]);
  });

  it("bounds the trail", () => {
    const hops = ["#/"];
    for (let i = 0; i < 40; i++) hops.push(`#/read/book-${i}`);
    expect(walk(...hops)).toHaveLength(12);
  });
});

describe("popNav", () => {
  it("hands back the last leg, then nothing", () => {
    const one = popNav(["#/", "#/notes"]);
    expect(one.to).toBe("#/notes");
    expect(popNav(one.trail).to).toBe("#/");
    expect(popNav([]).to).toBeNull();
  });
});

it("forgetDoc drops every leg into a deleted book", () => {
  expect(forgetDoc(["#/", "#/read/kant?p=2", "#/notes"], "kant")).toEqual(["#/", "#/notes"]);
});

describe("navLabel", () => {
  it("names each surface the way a button would", () => {
    expect(navLabel("#/", title)).toBe("library");
    expect(navLabel("#/notes?card=7", title)).toBe("notes");
    expect(navLabel("#/settings", title)).toBe("settings");
    expect(navLabel("#/read/kant?p=3", title)).toBe("Critique of Pure Reason");
  });

  it("cuts a title that would run past the button", () => {
    const long = navLabel("#/read/x", () => "A".repeat(80));
    expect(long).toHaveLength(28);
    expect(long.endsWith("…")).toBe(true);
  });
});
