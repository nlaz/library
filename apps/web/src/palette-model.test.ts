// The catalog's ranking. The property under all of it: the same keystrokes
// always select the same row, so a user who types blind is never surprised.

import { describe, expect, it } from "vitest";
import { crumbs, flatten, type Item, match, rank, type Stage, step } from "./palette-model";

const item = (label: string, group = "commands"): Item => ({
  id: label,
  label,
  group,
  run: () => {},
});

/** The winning label, for the many cases that only care which row is first. */
const top = (labels: string[], q: string) =>
  flatten(rank(labels.map((l) => item(l)), q))[0]?.item.label;

describe("match", () => {
  it("finds the characters in order, anywhere in the label", () => {
    expect(match("Corpus atlas", "cats")).not.toBeNull();
    expect(match("Corpus atlas", "clas")).not.toBeNull();
    expect(match("Corpus atlas", "xyz")).toBeNull();
    expect(match("Corpus atlas", "salta")).toBeNull(); // right letters, wrong order
  });

  it("marks the runs it matched, so the row can show its work", () => {
    expect(match("Settings", "set")?.spans).toEqual([[0, 3]]);
    // two runs, not three single characters: adjacent hits coalesce
    expect(match("Corpus atlas", "corat")?.spans).toEqual([
      [0, 3],
      [7, 9],
    ]);
  });

  it("an empty query keeps everything, at a flat score", () => {
    expect(match("Settings", "")).toEqual({ score: 0, spans: [] });
    expect(match("Settings", "   ")).toEqual({ score: 0, spans: [] });
  });
});

describe("rank", () => {
  it("prefers the front of a label to the middle of one", () => {
    expect(top(["Corpus atlas", "Start a note"], "at")).toBe("Corpus atlas");
  });

  it("prefers a word boundary to a letter inside a word", () => {
    // "note" starts a word in one and is buried in the other
    expect(top(["Denoted", "Start a note"], "note")).toBe("Start a note");
  });

  it("finds a book by a word from the middle of its title", () => {
    // the case the catalog exists for: nobody types a title from the front
    expect(top(["Kittler · Gramophone, Film, Typewriter", "Grammars of Creation"], "typewriter"))
      .toBe("Kittler · Gramophone, Film, Typewriter");
    expect(top(["Kittler · Gramophone, Film, Typewriter", "Grammars of Creation"], "gramm"))
      .toBe("Grammars of Creation");
  });

  it("does not charge a long title for the distance to the word that matched", () => {
    // the regression this pins: a word start is a word start wherever it sits,
    // so a real word late in a long title beats letters buried in a short one
    expect(top(["Programme", "Media · Gramophone"], "gram")).toBe("Media · Gramophone");
  });

  it("breaks a tie toward the shorter label", () => {
    expect(top(["Notes", "Notes and other things"], "notes")).toBe("Notes");
  });

  it("drops the misses", () => {
    const rows = flatten(rank([item("Settings"), item("Corpus atlas")], "zzz"));
    expect(rows).toEqual([]);
  });

  it("orders groups by their best row, not by a fixed list", () => {
    const items = [item("Settings", "commands"), item("Sontag · On Photography", "books")];
    // a query that names the book puts BOOKS on top...
    expect(rank(items, "sontag")[0].title).toBe("books");
    // ...and one that names the command puts COMMANDS there
    expect(rank(items, "settings")[0].title).toBe("commands");
  });

  it("caps a group so a big library cannot bury the commands", () => {
    const books = Array.from({ length: 40 }, (_, i) => item(`Book about seeing ${i}`, "books"));
    const sections = rank([...books, item("Settings", "commands")], "se", 6);
    expect(sections.find((s) => s.title === "books")!.rows).toHaveLength(6);
    expect(sections.find((s) => s.title === "commands")!.rows).toHaveLength(1);
  });

  it("an empty query leaves the caller's order alone", () => {
    const order = ["Search the library", "Start a note", "Settings"];
    expect(flatten(rank(order.map((l) => item(l)), "")).map((r) => r.item.label)).toEqual(order);
  });
});

describe("crumbs", () => {
  const stage = (crumb: string): Stage => ({ crumb, placeholder: "", items: () => [] });

  it("names the root when nothing has been drilled into", () => {
    expect(crumbs([])).toBe("card catalog");
  });

  it("reads as a path once it has", () => {
    expect(crumbs([stage("Kittler Gramophone"), stage("rename")])).toBe(
      "card catalog  ›  Kittler Gramophone  ›  rename",
    );
  });
});

describe("step", () => {
  it("wraps at both ends rather than dead-ending", () => {
    expect(step(3, 0, 1)).toBe(1);
    expect(step(3, 2, 1)).toBe(0);
    expect(step(3, 0, -1)).toBe(2);
  });

  it("stays put when there is nothing to walk", () => {
    expect(step(0, 0, 1)).toBe(0);
    expect(step(0, 0, -1)).toBe(0);
  });
});
