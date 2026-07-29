import { describe, expect, it } from "vitest";

import { type SheetKeyContext, sheetKey } from "./sheet-keys";

const ctx = (over: Partial<SheetKeyContext> = {}): SheetKeyContext => ({
  field: "body",
  acOpen: false,
  mod: false,
  shift: false,
  ...over,
});

describe("sheetKey", () => {
  it("esc walks back to the ledger", () => {
    expect(sheetKey("Escape", ctx())).toBe("leave");
    expect(sheetKey("Escape", ctx({ field: "title" }))).toBe("leave");
  });

  // the regression: esc used to fall straight through to leave(), so
  // dismissing the [[ list saved the half-typed fragment as a real note
  it("esc dismisses the completion list instead, while it is open", () => {
    expect(sheetKey("Escape", ctx({ acOpen: true }))).toBe("dismiss-completions");
  });

  it("a second esc then leaves, the list having closed", () => {
    expect(sheetKey("Escape", ctx({ acOpen: false }))).toBe("leave");
  });

  it("the title's completion list does not exist, so esc still leaves", () => {
    expect(sheetKey("Escape", ctx({ field: "title", acOpen: true }))).toBe("leave");
  });

  it("cmd-enter leaves from either field, list open or not", () => {
    expect(sheetKey("Enter", ctx({ mod: true }))).toBe("leave");
    expect(sheetKey("Enter", ctx({ field: "title", mod: true }))).toBe("leave");
    expect(sheetKey("Enter", ctx({ mod: true, acOpen: true }))).toBe("leave");
  });

  it("enter in the title hops to the prose; shift-enter and the body's own enter do not", () => {
    expect(sheetKey("Enter", ctx({ field: "title" }))).toBe("focus-body");
    expect(sheetKey("Enter", ctx({ field: "title", shift: true }))).toBeNull();
    expect(sheetKey("Enter", ctx())).toBeNull();
  });

  it("every other key belongs to the field", () => {
    expect(sheetKey("a", ctx())).toBeNull();
    expect(sheetKey("ArrowDown", ctx({ acOpen: true }))).toBeNull();
  });
});
