import { describe, expect, it } from "vitest";
import { onboardView } from "./onboard-model";
import type { DocInfo, DocStatus } from "./types";

const doc = (over: Partial<DocInfo> = {}): DocInfo => ({
  id: "a",
  title: null,
  pages: 10,
  collections: [],
  processing: false,
  status: null,
  ...over,
});

const status = (state: DocStatus["state"]): DocStatus =>
  ({ state, done: 0, total: 0 }) as DocStatus;

const marks = (docs: DocInfo[]) => onboardView(docs)?.map((r) => r.state);

describe("onboardView", () => {
  it("asks for a book when the library is empty", () => {
    expect(marks([])).toEqual(["active", "pending", "pending"]);
  });

  it("goes as soon as the first book is added, still indexing", () => {
    // the card on the shelf carries its own progress bar from that moment —
    // the panel would only be narrating what the shelf already shows
    expect(onboardView([doc({ processing: true, status: status("preparing") })])).toBeNull();
  });

  it("goes for a book that hasn't been looked at yet", () => {
    expect(onboardView([doc({ processing: true, status: null, pages: 0 })])).toBeNull();
  });

  it("goes once a book can answer a query", () => {
    expect(onboardView([doc({ status: status("ready") })])).toBeNull();
  });

  it("stays up when the only book failed, so there is still a way forward", () => {
    // the shelf shows the failed card and its Retry; the panel keeps the
    // drop target on screen rather than leaving a dead end as the whole UI
    expect(marks([doc({ status: status("failed") })])).toEqual(["active", "pending", "pending"]);
  });

  it("names the shortcut that opens the chooser", () => {
    expect(onboardView([])?.[0].sub).toContain("⌘O");
  });
});
