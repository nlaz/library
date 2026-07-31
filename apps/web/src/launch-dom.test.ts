// The launch card against the real index.html chrome. The pure half is
// covered in launch-model.test.ts; what this pins is the wiring — element
// ids that exist, the grace period that stops a warm launch from flashing,
// and the escape hatch that stops a slow one from being a wall.

import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { CHROME } from "./chrome-fixture";
import type { StartupStatus } from "./types";

const status = (over: Partial<StartupStatus> = {}): StartupStatus => ({
  step: "stores",
  detail: "Opening the library",
  done: 0,
  total: 0,
  ...over,
});

const el = (id: string) => document.getElementById(id)!;

let initLaunch: typeof import("./launch").initLaunch;

beforeAll(async () => {
  // mount before the import: launch.ts resolves its elements at module scope
  document.body.innerHTML = CHROME;
  ({ initLaunch } = await import("./launch"));
});

afterEach(() => vi.useRealTimers());

describe("the launch card", () => {
  it("never draws for a startup that finishes inside the grace period", async () => {
    vi.useFakeTimers();
    await initLaunch({
      poll: async () => status({ step: "ready", detail: "" }),
      subscribe: () => {},
    });
    await vi.advanceTimersByTimeAsync(5000);
    expect(el("launch").hidden).toBe(true);
    expect(el("launch-rows").children).toHaveLength(0);
  });
});

describe("the launch card, on a cold start", () => {
  // a second suite so it gets a fresh module: launch.ts latches `dismissed`
  // for the lifetime of one startup, which is the behaviour under test above
  beforeAll(async () => {
    vi.resetModules();
    document.body.innerHTML = CHROME;
    ({ initLaunch } = await import("./launch"));
  });

  it("draws every step, stamps the finished ones, and counts the bytes", async () => {
    vi.useFakeTimers();
    let push: (s: StartupStatus) => void = () => {};
    await initLaunch({
      poll: async () => status(),
      subscribe: (cb) => {
        push = cb;
      },
    });
    await vi.advanceTimersByTimeAsync(500);
    expect(el("launch").hidden).toBe(false);

    push(status({ step: "layout", done: 21 << 20, total: 59 << 20 }));
    const rows = [...el("launch-rows").children];
    expect(rows.map((r) => r.className)).toEqual([
      "lrow done",
      "lrow active",
      "lrow pending",
      "lrow pending",
    ]);
    expect(rows[1].textContent).toContain("21/59 MB");
    expect(rows[0].querySelector(".lmark")?.textContent).toBe("✓");

    // and a stalled startup eventually offers a way past itself
    expect((el("launch-skip") as HTMLButtonElement).hidden).toBe(true);
    await vi.advanceTimersByTimeAsync(4000);
    expect((el("launch-skip") as HTMLButtonElement).hidden).toBe(false);

    (el("launch-skip") as HTMLButtonElement).click();
    expect(el("launch").hidden).toBe(true);
  });
});
