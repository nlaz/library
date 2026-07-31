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

  it("advances to the reading step while the first book ingests", () => {
    expect(marks([doc({ processing: true, status: status("preparing") })])).toEqual([
      "done",
      "active",
      "pending",
    ]);
  });

  it("disappears once one book can answer a query", () => {
    expect(onboardView([doc({ status: status("ready") })])).toBeNull();
  });

  it("counts text_ready as answerable — only figures are still indexing", () => {
    expect(onboardView([doc({ status: status("text_ready") })])).toBeNull();
  });

  it("stays up when the only book failed, so there is still a way forward", () => {
    // the shelf shows the failed card and its Retry; the panel keeps the
    // drop target on screen rather than leaving a dead end as the whole UI
    expect(marks([doc({ status: status("failed") })])).toEqual([
      "active",
      "pending",
      "pending",
    ]);
  });

  it("names the shortcut that opens the chooser", () => {
    expect(onboardView([])?.[0].sub).toContain("⌘O");
  });
});
