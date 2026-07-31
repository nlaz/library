import { describe, expect, it } from "vitest";
import { launchView } from "./launch-model";
import type { StartupStatus } from "./types";

const status = (over: Partial<StartupStatus> = {}): StartupStatus => ({
  step: "stores",
  detail: "Opening the library",
  done: 0,
  total: 0,
  ...over,
});

const marks = (s: StartupStatus) => launchView(s).rows.map((r) => r.state);

describe("launchView", () => {
  it("takes the card down on ready", () => {
    expect(launchView(status({ step: "ready", detail: "" })).show).toBe(false);
  });

  it("lists every step from the first frame, nothing stamped yet", () => {
    const v = launchView(status());
    expect(v.rows.map((r) => r.label)).toEqual([
      "Opening the library",
      "Fetching the page-layout model",
      "Fetching the figure-search model",
      "Fetching the figure-indexing model",
    ]);
    expect(marks(status())).toEqual(["active", "pending", "pending", "pending"]);
  });

  it("stamps everything behind the step in flight", () => {
    expect(marks(status({ step: "layout" }))).toEqual([
      "done",
      "active",
      "pending",
      "pending",
    ]);
    expect(marks(status({ step: "vision" }))).toEqual(["done", "done", "done", "active"]);
  });

  it("counts bytes on the active row only", () => {
    const v = launchView(status({ step: "layout", done: 21 << 20, total: 59 << 20 }));
    expect(v.rows.map((r) => r.note)).toEqual(["", "21/59 MB", "", ""]);
  });

  it("says nothing about bytes when there is no denominator", () => {
    const v = launchView(status({ step: "clip" }));
    expect(v.rows.every((r) => r.note === "")).toBe(true);
  });

  it("appends an unknown step rather than dropping it", () => {
    // an older frontend against a newer engine must still show the wait
    const v = launchView(status({ step: "warming" as StartupStatus["step"], detail: "Warming" }));
    expect(v.rows.at(-1)).toMatchObject({ label: "Warming", state: "active" });
    expect(marks(status({ step: "warming" as StartupStatus["step"] }))).toEqual([
      "done",
      "done",
      "done",
      "done",
      "active",
    ]);
  });
});
