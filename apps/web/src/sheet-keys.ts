// What a keystroke means inside the sheet's two fields. Lives outside
// sheet.ts so it can be unit-tested without that module's top-level DOM
// lookups.

export type SheetKeyAction =
  /** Close the [[ completion list; the sheet stays open. */
  | "dismiss-completions"
  /** Flush the draft and walk back to wherever the note was started. */
  | "leave"
  /** Hop from the title into the prose. */
  | "focus-body"
  /** Nothing special — let the field have the key. */
  | null;

export type SheetKeyContext = {
  field: "title" | "body";
  /** Whether the [[ completion list is showing. */
  acOpen: boolean;
  /** ⌘ or ctrl. */
  mod: boolean;
  shift: boolean;
};

/** The sheet saves as you write, so esc is a way *out*, not a discard — but
 * an open completion list owns esc first. Dismissing a popup must never
 * also walk out of the sheet: that reads as "cancel" and instead commits
 * whatever half-typed `[[` fragment was on screen. */
export function sheetKey(key: string, ctx: SheetKeyContext): SheetKeyAction {
  if (key === "Escape") {
    return ctx.field === "body" && ctx.acOpen ? "dismiss-completions" : "leave";
  }
  if (key === "Enter" && ctx.mod) return "leave";
  if (key === "Enter" && ctx.field === "title" && !ctx.shift) return "focus-body";
  return null;
}
