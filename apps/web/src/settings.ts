// ---------------------------------------------------------------------------
// Settings (⌘S): the folders the library watches, and what it costs on disk.
//
// A sheet in the same family as the shortcut list — a scrim and one card,
// read *about* the surface you were on and then dismissed. It is desktop-
// only: the web build has no filesystem to point at.
//
// The folder list is the whole point. Everything else on this page could be
// removed and the app would still work; without it, a user with their own
// organization has no way to tell us about it.
// ---------------------------------------------------------------------------

import { desktop } from "./state";
import {
  type RootInfo,
  type RootRow,
  rootRows,
  type Storage,
  storageRows,
  unlinkWarning,
} from "./settings-model";
import { notify } from "./toast";

const $sheet = document.getElementById("settings")!;
const $body = document.getElementById("settings-body")!;

let home = "";

export function settingsOpen(): boolean {
  return !$sheet.hidden;
}

export function closeSettings() {
  $sheet.hidden = true;
}

export function toggleSettings() {
  if (settingsOpen()) closeSettings();
  else void openSettings();
}

export async function openSettings() {
  if (!desktop) return; // no folders to manage in the browser build
  $sheet.hidden = false;
  await render();
}

function section(title: string): HTMLElement {
  const el = document.createElement("section");
  const h = document.createElement("h3");
  h.textContent = title;
  el.append(h);
  return el;
}

function rootEl(row: RootRow, onChange: () => void): HTMLElement {
  const el = document.createElement("div");
  el.className = `sroot${row.ok ? "" : " away"}`;

  const mark = document.createElement("span");
  mark.className = "smark";
  // a filled star is the drop target; a hollow ring is a linked folder that
  // isn't. An unreadable one says so with its own glyph rather than colour
  // alone, which a screenshot in greyscale would lose.
  mark.textContent = row.ok ? (row.isDefault ? "★" : "○") : "⚠";
  mark.title = row.ok
    ? row.isDefault
      ? "New books are added here"
      : "Watched"
    : "Not readable right now";

  const body = document.createElement("div");
  const path = document.createElement("b");
  path.textContent = row.label;
  const status = document.createElement("span");
  status.className = "sstatus";
  status.textContent = row.status;
  body.append(path, status);

  const actions = document.createElement("div");
  actions.className = "sactions";
  if (!row.isDefault && row.ok) {
    const mk = document.createElement("button");
    mk.className = "divot firm";
    mk.textContent = "Add here";
    mk.title = "Make this the folder new books are added to";
    mk.addEventListener("click", async () => {
      await desktop!.setDefaultRoot(row.id);
      onChange();
    });
    actions.append(mk);
  }
  if (row.canUnlink) {
    const rm = document.createElement("button");
    rm.className = "divot";
    rm.textContent = "Remove";
    rm.addEventListener("click", async () => {
      if (!(await desktop!.confirmUnlink(unlinkWarning(row)))) return;
      try {
        await desktop!.unlinkRoot(row.id);
        onChange();
      } catch (e) {
        notify(`${e}`, { sticky: true });
      }
    });
    actions.append(rm);
  }

  el.append(mark, body, actions);
  return el;
}

async function render() {
  if (!desktop) return;
  const reload = () => void render();

  let roots: RootInfo[] = [];
  try {
    roots = await desktop.listRoots();
  } catch (e) {
    notify(`could not read your folders: ${e}`, { sticky: true });
  }
  if (!home) home = await desktop.homeDir().catch(() => "");

  const folders = section("Folders");
  const list = document.createElement("div");
  list.className = "sroots";
  for (const row of rootRows(roots, home)) list.append(rootEl(row, reload));
  folders.append(list);

  const link = document.createElement("button");
  link.className = "divot firm slink";
  link.textContent = "Link a folder…";
  link.addEventListener("click", async () => {
    const picked = await desktop!.pickFolder("Choose a folder to watch");
    if (!picked) return;
    try {
      await desktop!.linkRoot(picked);
      reload();
    } catch (e) {
      notify(`could not link that folder: ${e}`, { sticky: true });
    }
  });
  folders.append(link);

  const note = document.createElement("p");
  note.className = "snote";
  note.textContent =
    "Your files stay where they are. Folders inside a watched folder become shelves.";
  folders.append(note);

  const storage = section("Storage");
  const table = document.createElement("dl");
  table.className = "sstorage";
  try {
    const s: Storage = await desktop.storageUse();
    for (const [label, value] of storageRows(s)) {
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.textContent = value;
      table.append(dt, dd);
    }
    const note = document.createElement("p");
    note.className = "snote";
    note.textContent = `Kept in ${s.path}. Everything here can be rebuilt from your files.`;
    storage.append(table, note);
  } catch {
    const p = document.createElement("p");
    p.className = "snote";
    p.textContent = "Couldn't measure storage.";
    storage.append(p);
  }

  $body.replaceChildren(folders, storage);
}

document.getElementById("settings-close")!.addEventListener("click", closeSettings);
$sheet.addEventListener("click", (e) => {
  if (e.target === $sheet) closeSettings(); // the scrim, not the card
});
