// ---------------------------------------------------------------------------
// Settings (⌘S): a page, not a sheet.
//
// It was a modal card for as long as it held one thing — the folders the
// library watches. A card you read *about* the surface behind it is the
// wrong shape for what it holds now: dimming the shelves while a folder is
// scanned says the library is unavailable, which is a lie, and a scrim is a
// promise that you were only going to be a moment.
//
// So: the notes ledger's pattern. A section beside <main>, under the app's
// one header, reached at #/settings and left by the same ← as anywhere
// else. Desktop-only — the web build has no filesystem to point at.
//
// The folder list is still the whole point. Everything else on this page
// could be removed and the app would work; without it, a user with their
// own organization has no way to tell us about it.
// ---------------------------------------------------------------------------

import { chatEnabled, chatSupported, chatUnavailableReason, setChatEnabled } from "./chat";
import { $main, setPressed } from "./dom";
import { loadCollections, renderHome } from "./home";
import { goBack, originLabel } from "./nav";
import {
  activeSection,
  BUDGET_CHOICES,
  formatBytes,
  librarianRow,
  PINNED_NOTE,
  type RootInfo,
  type RootRow,
  rootRows,
  SECTIONS,
  type SectionId,
  type Storage,
  storageRows,
  unlinkWarning,
  type UpdateState,
  updateRow,
} from "./settings-model";
import { desktop } from "./state";
import { notify } from "./toast";

const $settings = document.getElementById("settings")!;
const $body = document.getElementById("settings-body")!;
const $toggle = document.getElementById("settings-toggle")!;

let home = "";
/** Rail buttons and the panes they point at, paired by index. Rebuilt by
 * every render, because a render can change how tall a section is. */
let rail: HTMLElement[] = [];
let panes: HTMLElement[] = [];

export function settingsOpen(): boolean {
  return !$settings.hidden;
}

export function closeSettings() {
  if ($settings.hidden) return;
  $settings.hidden = true;
  $main.hidden = false;
  setPressed($toggle, false);
}

export async function openSettings() {
  if (!desktop) return; // no folders to manage in the browser build
  $settings.hidden = false;
  $main.hidden = true;
  setPressed($toggle, true);
  await render();
  $body.scrollTop = 0;
  spy();
}

// ---------------------------------------------------------------------------
// the rail
// ---------------------------------------------------------------------------

/** Each pane's distance from the top of the scroller's content. Measured
 * rather than remembered: a folder list grows by a row when you link one. */
function offsets(): number[] {
  const origin = $body.getBoundingClientRect().top - $body.scrollTop;
  return panes.map((p) => p.getBoundingClientRect().top - origin);
}

function spy() {
  if (!panes.length) return;
  const i = activeSection($body.scrollTop, offsets(), {
    height: $body.clientHeight,
    total: $body.scrollHeight,
  });
  rail.forEach((b, n) => {
    b.classList.toggle("sel", n === i);
    // aria-current, not aria-pressed: the rail reports where you are, it
    // does not hold a setting down
    if (n === i) b.setAttribute("aria-current", "true");
    else b.removeAttribute("aria-current");
  });
}

$body.addEventListener("scroll", spy, { passive: true });

// ---------------------------------------------------------------------------
// sections
// ---------------------------------------------------------------------------

function pane(id: SectionId, title: string): HTMLElement {
  const el = document.createElement("section");
  el.id = `st-${id}`;
  const h = document.createElement("h3");
  h.textContent = title;
  el.append(h);
  return el;
}

function note(text: string): HTMLElement {
  const p = document.createElement("p");
  p.className = "snote";
  p.textContent = text;
  return p;
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

async function foldersPane(reload: () => void): Promise<HTMLElement> {
  let roots: RootInfo[] = [];
  try {
    roots = await desktop!.listRoots();
  } catch (e) {
    notify(`could not read your folders: ${e}`, { sticky: true });
  }
  if (!home) home = await desktop!.homeDir().catch(() => "");

  const el = pane("folders", "Folders");
  const list = document.createElement("div");
  list.className = "sroots";
  for (const row of rootRows(roots, home)) list.append(rootEl(row, reload));

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

  el.append(
    list,
    link,
    note("Your files stay where they are. Folders inside a watched folder become shelves."),
  );
  return el;
}

async function storagePane(): Promise<HTMLElement> {
  const el = pane("storage", "Storage");
  try {
    const s: Storage = await desktop!.storageUse();
    const table = document.createElement("dl");
    table.className = "sstorage";
    for (const [label, value] of storageRows(s)) {
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.textContent = value;
      table.append(dt, dd);
    }
    el.append(table);
    if (s.pinned_bytes) el.append(note(PINNED_NOTE));

    // The budget, and a way to act on it now. Page images are the only line
    // here the app deletes on its own, so the control for that belongs
    // beside the number rather than buried somewhere else.
    if (s.page_bytes !== undefined) {
      const row = document.createElement("div");
      row.className = "srow";
      const label = document.createElement("b");
      label.textContent = "Keep up to";
      const pick = document.createElement("select");
      const current = s.page_budget_bytes ?? 0;
      for (const [text, bytes] of BUDGET_CHOICES) {
        const opt = document.createElement("option");
        opt.value = String(bytes);
        opt.textContent = text;
        // the stored value is clamped server-side, so match on the nearest
        // choice rather than requiring an exact hit
        if (bytes === current) opt.selected = true;
        pick.append(opt);
      }
      pick.addEventListener("change", () => {
        void desktop!.setPref("cache.pages.budget_bytes", pick.value);
      });
      const free = document.createElement("button");
      free.className = "ghost";
      free.textContent = "Free up space now";
      free.addEventListener("click", async () => {
        free.disabled = true;
        free.textContent = "Freeing…";
        const freed = await desktop!.sweepCache().catch(() => 0);
        free.textContent = freed > 0 ? `Freed ${formatBytes(freed)}` : "Nothing to free";
      });
      row.append(label, pick, free);
      el.append(row);
    }

    el.append(
      note(
        `Kept in ${s.path}. Page images are re-created from your files as you read, ` +
          `so removing them costs a moment, never a document.`,
      ),
    );
  } catch {
    el.append(note("Couldn't measure storage."));
  }
  return el;
}

function librarianPane(): HTMLElement {
  const el = pane("librarian", "Librarian");

  const line = document.createElement("div");
  line.className = "srow";
  const label = document.createElement("b");
  label.textContent = "Ask the librarian";
  const btn = document.createElement("button");
  btn.className = "divot firm stoggle";

  const paint = () => {
    const row = librarianRow({
      supported: chatSupported(),
      reason: chatUnavailableReason(),
      enabled: chatEnabled(),
    });
    btn.textContent = row.label;
    setPressed(btn, row.on);
    btn.disabled = !row.can;
    return row;
  };
  const row = paint();
  if (!row.can) btn.title = "Not available on this Mac";

  btn.addEventListener("click", async () => {
    const next = !chatEnabled();
    try {
      await desktop!.setFlag(desktop!.CHAT_ENABLED, next);
    } catch (e) {
      // the header button must not disagree with what is on disk
      notify(`could not save that: ${e}`, { sticky: true });
      return;
    }
    setChatEnabled(next);
    paint();
  });

  line.append(label, btn);
  el.append(
    line,
    note("A chat drawer that searches your books and cites what it read. Off until you ask for it."),
  );
  if (row.warn) {
    const w = note(row.warn);
    w.classList.add("swarn");
    el.append(w);
  }
  return el;
}

// ---------------------------------------------------------------------------
// about, and updating
//
// Checks are manual. An app that phones home on launch has to be trusted
// about what else it sends, and a library is a record of what someone
// reads — the check costs one button press and buys not having to make
// that promise.
// ---------------------------------------------------------------------------

let version = "";
let canUpdate = false;
let update: UpdateState = { at: "idle" };
/** Redraw the About row alone. A whole render would re-scan the folders
 * and re-measure the cache on every download progress event. */
let repaint = () => {};
let wired = false;

function wireProgress() {
  if (wired || !desktop) return;
  wired = true;
  desktop.onUpdateProgress((p) => {
    if (update.at !== "downloading") return;
    update = { ...update, downloaded: p.downloaded, total: p.total };
    repaint();
  });
}

async function check() {
  update = { at: "checking" };
  repaint();
  try {
    const found = await desktop!.checkUpdate();
    update = found
      ? { at: "found", version: found.version, notes: found.notes }
      : { at: "current" };
  } catch (e) {
    update = { at: "failed", why: `${e}` };
  }
  repaint();
}

async function install(v: string) {
  wireProgress();
  update = { at: "downloading", version: v, downloaded: 0, total: null };
  repaint();
  try {
    await desktop!.installUpdate();
    update = { at: "staged", version: v };
  } catch (e) {
    update = { at: "failed", why: `${e}` };
  }
  repaint();
}

function act() {
  switch (update.at) {
    case "idle":
    case "current":
    case "failed":
      void check();
      break;
    case "found":
      void install(update.version);
      break;
    case "staged":
      desktop!.restartApp();
      break;
  }
}

function aboutPane(): HTMLElement {
  const el = pane("about", "About");

  const id = document.createElement("div");
  id.className = "srow";
  const name = document.createElement("b");
  name.textContent = "The Library";
  const ver = document.createElement("span");
  ver.className = "sver";
  ver.textContent = version || "—";
  id.append(name, ver);
  el.append(id);

  if (!canUpdate) {
    // a dev build runs from target/debug and has no bundle to replace;
    // offering to update it is an invitation to a confusing failure
    repaint = () => {};
    el.append(note("A development build. It changes when you rebuild it."));
    return el;
  }

  const row = document.createElement("div");
  row.className = "srow";
  const left = document.createElement("div");
  left.className = "sprogress";
  const status = document.createElement("span");
  status.className = "sstatus";
  const bar = document.createElement("div");
  bar.className = "sbar";
  const fill = document.createElement("i");
  bar.append(fill);
  left.append(status, bar);
  const btn = document.createElement("button");
  btn.className = "divot firm";
  btn.addEventListener("click", act);
  row.append(left, btn);

  const detail = note("");
  el.append(row, detail);

  repaint = () => {
    const r = updateRow(update);
    status.textContent = r.status;
    status.hidden = !r.status;
    bar.hidden = r.progress === null;
    fill.style.width = `${Math.round((r.progress ?? 0) * 100)}%`;
    btn.textContent = r.action ?? "";
    btn.hidden = r.action === null;
    detail.textContent = r.detail ?? "";
    detail.hidden = !r.detail;
  };
  repaint();
  return el;
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// Linking or unlinking a folder changes which books exist, and the shelves
/// behind this page are still showing the old set — leaving Settings to
/// find a folder you removed still sitting there reads as a failed removal.
async function changed() {
  await render();
  await renderHome(await loadCollections());
}

async function render() {
  if (!desktop) return;
  const reload = () => void changed();
  // both are fixed for the life of the process, so they are asked once
  if (!version) version = await desktop.appVersion().catch(() => "");
  canUpdate = await desktop.updatesSupported().catch(() => false);

  // the ledger's crumb, verbatim: settings is reached from the shelves,
  // from a book, from the notes, and ← has to name whichever it was
  const crumbs = document.createElement("div");
  crumbs.className = "st-crumbs";
  const back = document.createElement("button");
  back.className = "nb-crumb divot quiet";
  const here = document.createElement("span");
  here.className = "here";
  here.textContent = "/ settings";
  back.append(`← ${originLabel()} `, here);
  back.addEventListener("click", () => goBack("#/"));
  crumbs.append(back);

  const built: Record<SectionId, HTMLElement> = {
    folders: await foldersPane(reload),
    storage: await storagePane(),
    librarian: librarianPane(),
    about: aboutPane(),
  };

  const content = document.createElement("div");
  content.className = "st-content";
  const nav = document.createElement("nav");
  nav.className = "st-rail";
  nav.setAttribute("aria-label", "Settings sections");
  const col = document.createElement("div");
  col.className = "st-panes";

  rail = [];
  panes = [];
  SECTIONS.forEach(({ id, label }, i) => {
    const b = document.createElement("button");
    // bare, like the ledger's rail: a groove around every line would fight
    // the "whitespace instead of boxes" rule this surface is built on
    b.className = "st-railitem divot bare";
    b.textContent = label;
    b.addEventListener("click", () => {
      $body.scrollTop = Math.max(0, offsets()[i] - 24);
      spy();
    });
    nav.append(b);
    rail.push(b);
    col.append(built[id]);
    panes.push(built[id]);
  });

  content.append(nav, col);
  $body.replaceChildren(crumbs, content);
  spy();
}

// ---------------------------------------------------------------------------
// getting in and out
// ---------------------------------------------------------------------------

/** ⌘S and the header cog. A toggle puts you back where you were, which is
 * not always the shelves. */
export function toggleSettings() {
  if (!desktop) return; // no folders to manage in the browser build
  if (settingsOpen()) goBack("#/");
  else location.hash = "#/settings";
}

// the browser build watches no folders and cannot update itself: there is
// nothing behind the cog, so it is not in the header either
if (desktop) $toggle.addEventListener("click", toggleSettings);
else $toggle.hidden = true;

document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || $settings.hidden) return;
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
  goBack("#/");
});
