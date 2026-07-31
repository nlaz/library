// What the launch screen should show for a given startup status. Pure, so
// the wording, the ordering and the byte arithmetic are testable without a
// window — the DOM half lives in launch.ts and does nothing but apply this.

import type { StartupStatus } from "./types";

export type RowState = "done" | "active" | "pending";

export type LaunchRow = {
  step: string;
  label: string;
  state: RowState;
  /** Byte count for the active row, e.g. "84/335 MB"; empty otherwise. */
  note: string;
};

export type LaunchView = {
  /** False once startup is done: the card comes down and stays down. */
  show: boolean;
  rows: LaunchRow[];
};

/**
 * The startup sequence, in order, with the labels the card shows.
 *
 * The client owns these rather than rendering the status's own `detail`,
 * because a checklist has to name the steps it hasn't reached yet — and only
 * this side knows what's coming. `detail` is the fallback for a step this
 * build doesn't know about, so an older frontend against a newer engine
 * degrades to "show what it said" instead of showing nothing.
 */
const STEPS: { step: string; label: string }[] = [
  { step: "stores", label: "Opening the library" },
  { step: "layout", label: "Fetching the page-layout model" },
  { step: "clip", label: "Fetching the figure-search model" },
  { step: "vision", label: "Fetching the figure-indexing model" },
];

const MB = (b: number) => Math.round(b / (1024 * 1024));

export function launchView(s: StartupStatus): LaunchView {
  if (s.step === "ready") return { show: false, rows: [] };

  let at = STEPS.findIndex((r) => r.step === s.step);
  const rows = STEPS.map((r) => ({ ...r, state: "pending" as RowState, note: "" }));
  if (at < 0) {
    // unknown step: append it rather than dropping the status on the floor
    at = rows.length;
    rows.push({ step: s.step, label: s.detail, state: "pending", note: "" });
  }

  for (let i = 0; i < rows.length; i++) {
    rows[i].state = i < at ? "done" : i === at ? "active" : "pending";
  }
  // Only the step in flight gets a count, and only when it has a real
  // denominator — a "0/0 MB" on a step that isn't downloading reads as a
  // stall rather than as work.
  if (s.total > 0) rows[at].note = `${MB(s.done)}/${MB(s.total)} MB`;

  return { show: true, rows };
}
