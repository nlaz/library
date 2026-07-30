// The divot's pressed face is driven by a class, but a screen reader only
// hears the attribute. setPressed is the one place that writes both, so what
// is worth pinning is that they cannot come apart — and that the toggles
// which start life in index.html announce themselves as unpressed rather
// than as plain buttons.

import { beforeAll, expect, test } from "vitest";
import { mountChrome } from "./chrome-fixture";

let setPressed: (el: Element, on: boolean) => void;

beforeAll(async () => {
  await mountChrome();
  // after the mount: dom.ts resolves its handles at module scope
  ({ setPressed } = await import("./dom"));
});

// A toggle with no aria-pressed at rest reads as an ordinary button, so the
// "off" state is silent — the failure you cannot see in a screenshot.
test.each([["notes-toggle"], ["reader-pen"]])("%s boots unpressed", (id) => {
  expect(document.getElementById(id)!.getAttribute("aria-pressed")).toBe("false");
});

test("setPressed moves the class and the attribute together", () => {
  const el = document.createElement("button");

  setPressed(el, true);
  expect(el.classList.contains("on")).toBe(true);
  expect(el.getAttribute("aria-pressed")).toBe("true");

  setPressed(el, false);
  expect(el.classList.contains("on")).toBe(false);
  expect(el.getAttribute("aria-pressed")).toBe("false");
});
