// ---------------------------------------------------------------------------
// The first-run panel: three states a book passes through, shown until this
// library holds one that can answer a query. Rendered by home.ts above the
// shelves, so a book that is still being read appears underneath the row
// that explains what is happening to it.
//
// The key strip at the foot is the other half: this app's vocabulary is
// entirely in its keyboard, and the empty library is the one moment we can
// be sure someone is looking for it.
// ---------------------------------------------------------------------------

import { onboardView, type OnboardRow } from "./onboard-model";
import type { DocInfo } from "./types";

const MARK: Record<OnboardRow["state"], string> = { done: "✓", active: "›", pending: "·" };

// Injected rather than imported: the picker lives in ingest-ui, which
// already imports home.ts, which renders this panel. Same shape as state.ts
// — one owner sets it during startup, everyone else reads.
let onPick: () => void = () => {};

export function setOnboardPicker(f: () => void) {
  onPick = f;
}

/** The panel, or null when the library speaks for itself. */
export function onboardPanel(docs: DocInfo[]): HTMLElement | null {
  const rows = onboardView(docs);
  if (!rows) return null;

  const el = document.createElement("section");
  el.className = "onboard";

  const list = document.createElement("div");
  list.className = "osteps";
  for (const r of rows) {
    const row = document.createElement("div");
    row.className = `ostep ${r.state}`;
    const mark = document.createElement("span");
    mark.className = "omark";
    mark.textContent = MARK[r.state];
    const body = document.createElement("span");
    const title = document.createElement("b");
    title.textContent = r.title;
    const sub = document.createElement("span");
    sub.className = "osub";
    sub.textContent = r.sub;
    body.append(title, sub);
    row.append(mark, body);

    // The picker is reachable by ⌘O, but a shortcut nobody has been told
    // about is not an affordance — the button is how anyone finds it.
    if (r.state === "active" && r.title.startsWith("Drop")) {
      const pick = document.createElement("button");
      pick.className = "divot firm";
      pick.textContent = "Choose files…";
      pick.addEventListener("click", onPick);
      row.append(pick);
    }
    list.append(row);
  }

  const keys = document.createElement("div");
  keys.className = "okeys";
  for (const [k, what] of [
    ["⌘F", "search"],
    ["c", "note"],
    ["?", "all shortcuts"],
  ]) {
    const item = document.createElement("span");
    const kbd = document.createElement("kbd");
    kbd.textContent = k;
    item.append(kbd, ` ${what}`);
    keys.append(item);
  }

  el.append(list, keys);
  return el;
}
