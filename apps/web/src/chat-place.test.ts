import { describe, expect, it } from "vitest";

import { placeToolRow } from "./chat-place";

function el(name: string): HTMLElement {
  const e = document.createElement("div");
  e.textContent = name;
  return e;
}

function order(log: HTMLElement): string[] {
  return [...log.children].map((c) => c.textContent ?? "");
}

describe("placeToolRow", () => {
  it("appends when no assistant row exists yet", () => {
    const log = el("log");
    placeToolRow(log, el("tool"), null);
    expect(order(log)).toEqual(["tool"]);
  });

  it("inserts above the streaming assistant row", () => {
    const log = el("log");
    const user = el("user");
    const assistant = el("assistant");
    log.append(user, assistant);
    placeToolRow(log, el("tool"), assistant);
    expect(order(log)).toEqual(["user", "tool", "assistant"]);
  });

  it("keeps multiple late tool rows in arrival order, all above the answer", () => {
    const log = el("log");
    const assistant = el("assistant");
    log.append(assistant);
    placeToolRow(log, el("tool-1"), assistant);
    placeToolRow(log, el("tool-2"), assistant);
    expect(order(log)).toEqual(["tool-1", "tool-2", "assistant"]);
  });

  it("appends when the anchor is no longer in the log", () => {
    const log = el("log");
    const detached = el("assistant");
    placeToolRow(log, el("tool"), detached);
    expect(order(log)).toEqual(["tool"]);
  });
});
