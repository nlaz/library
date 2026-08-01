// Reader metadata drawer: view + edit a document's title and collections,
// plus read-only facts (id, pages, ingest status). main.ts supplies the data
// sources and (desktop only) the write commands via initDrawer — the web
// build passes edit: null and gets a read-only panel.

import {
  type Mark,
  docMarks,
  fileAwayMark,
  markupDoc,
  onMarksChanged,
  onMarkupEnter,
  openMarkPopover,
  scheduleMarkTicks,
} from "./markup";
import { onReaderEscape, scheduleTicks } from "./reader";
import type { Collections, DocStatus } from "./types";

/** The facts the drawer shows — DocInfo minus `processing` (the web
 * endpoint doesn't carry it, and the drawer doesn't need it). */
export type DrawerDoc = {
  id: string;
  title: string | null;
  pages: number;
  collections: string[];
  status: DocStatus | null;
};

type Opts = {
  currentDoc(): string;
  getDoc(id: string): Promise<DrawerDoc>;
  getCollections(): Promise<Collections>;
  prettify(id: string): string;
  /** null = read-only (web build) */
  edit: {
    setTitle(doc: string, title: string): Promise<void>;
    moveToShelf(doc: string, shelf: string): Promise<void>;
  } | null;
  /** null = no filesystem to show it in (web build) */
  reveal: ((doc: string) => Promise<void>) | null;
  onChanged(doc: string): void;
  onError(msg: string): void;
};

const $drawer = document.getElementById("reader-drawer")!;
const $readerBody = document.getElementById("reader-body")!;
const $toggle = document.getElementById("reader-meta")!;
let opts: Opts | null = null;
let openFor = ""; // doc id the drawer is showing; "" = closed

/** The pane's width changed under the ticks — no resize event fires. */
function reflowTicks() {
  scheduleTicks();
  scheduleMarkTicks();
}

export function initDrawer(o: Opts) {
  opts = o;
  $toggle.addEventListener("click", () => {
    const doc = o.currentDoc();
    if (!doc) return;
    if (openFor === doc) closeDrawer();
    else openDrawer(doc);
  });
  // keep the marginalia section live while the drawer is open
  onMarksChanged(() => {
    if (openFor && openFor === markupDoc()) openDrawer(openFor);
  });
  // entering markup mode surfaces the doc's existing marks
  onMarkupEnter((doc) => {
    if (doc) void openDrawer(doc);
  });
  // an open drawer is a layer: Escape closes it before it closes the book
  onReaderEscape(() => {
    if (!openFor) return false;
    closeDrawer();
    return true;
  });
}

export function closeDrawer() {
  $drawer.hidden = true;
  if ($readerBody.classList.contains("drawer-open")) {
    $readerBody.classList.remove("drawer-open");
    reflowTicks();
  }
  openFor = "";
}

async function openDrawer(doc: string) {
  if (!opts) return;
  openFor = doc;
  $drawer.hidden = false;
  if (!$readerBody.classList.contains("drawer-open")) {
    $readerBody.classList.add("drawer-open");
    reflowTicks();
  }
  $drawer.replaceChildren();
  const [d, cols] = await Promise.all([opts.getDoc(doc), opts.getCollections()]);
  if (openFor !== doc) return; // closed or switched mid-fetch
  renderDrawer(d, cols);
}

function renderDrawer(d: DrawerDoc, cols: Collections) {
  const o = opts!;
  const title = d.title ?? o.prettify(d.id);

  const rows: HTMLElement[] = [];
  rows.push(row("title", o.edit ? titleInput(d, title) : text(title)));

  const shelfRow = row("shelf", shelfPicker(d, Object.keys(cols)));
  rows.push(shelfRow);

  const fileRow = row("file", text(d.id));
  if (o.reveal) fileRow.querySelector(".dval")!.append(revealButton(d.id));
  rows.push(fileRow);
  rows.push(row("pages", text(d.pages ? `${d.pages} pp.` : "—")));
  const s = d.status;
  rows.push(row("status", text(s ? s.state + (s.error ? ` — ${s.error}` : "") : "ready")));
  if (d.id === markupDoc()) rows.push(row("marginalia", marginaliaList()));

  $drawer.replaceChildren(...rows);
}

/** The id names the original in the library folder; this is the way to it —
 * the file was moved there on add, so nothing else knows where it went. */
function revealButton(doc: string): HTMLElement {
  const b = document.createElement("button");
  b.className = "dreveal divot quiet";
  b.textContent = "Show in Finder";
  b.addEventListener("click", async () => {
    try {
      await opts!.reveal!(doc);
    } catch (e) {
      opts?.onError(`show in Finder: ${e}`);
    }
  });
  return b;
}

/** Every mark in the open doc, page order (docMarks() is pre-sorted);
 * click jumps the scroll to the mark and opens its margin card. */
function marginaliaList(): HTMLElement {
  const el = document.createElement("div");
  el.className = "mnotes";
  const marks = docMarks();
  if (!marks.length) {
    const none = document.createElement("span");
    none.className = "dnone";
    none.textContent = "none yet — ⌘U to mark up";
    el.append(none);
    return el;
  }
  for (const m of marks) el.append(marginaliaRow(m));
  return el;
}

function marginaliaRow(m: Mark): HTMLElement {
  const r = document.createElement("div");
  r.className = "mrow";
  const loc = document.createElement("span");
  loc.className = "mloc";
  loc.textContent = `p.${m.anchor.page}${m.anchor.kind === "region" ? " · region" : ""}`;
  const t = document.createElement("span");
  t.className = "mtext";
  t.textContent = m.card.title;
  const del = document.createElement("button");
  del.className = "mdel divot quiet";
  del.title = "File away";
  del.textContent = "✕";
  del.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      await fileAwayMark(m.card.id);
    } catch (err) {
      opts?.onError(`file away: ${err}`);
    }
  });
  r.append(loc, t, del);
  r.addEventListener("click", () => openMarkPopover(m.card.id, true));
  return r;
}

function titleInput(d: DrawerDoc, initial: string): HTMLElement {
  const o = opts!;
  const input = document.createElement("input");
  input.type = "text";
  input.value = initial;
  let done = false;
  const commit = async () => {
    if (done || input.value.trim() === initial) return;
    done = true;
    // storing the prettified id would freeze the fallback; treat it as "unset"
    const v = input.value.trim() === o.prettify(d.id) ? "" : input.value;
    try {
      await o.edit!.setTitle(d.id, v);
    } catch (e) {
      o.onError(`rename: ${e}`);
    }
    o.onChanged(d.id);
    openDrawer(d.id);
  };
  input.addEventListener("keydown", (e) => {
    e.stopPropagation(); // reader hotkeys must not fire while typing
    if (e.key === "Enter") commit();
    if (e.key === "Escape") {
      input.value = initial;
      input.blur();
    }
  });
  input.addEventListener("blur", commit);
  return input;
}

/// The shelf a document is on, and the way to change it.
///
/// One shelf, not many: a shelf is the folder the file sits in, and a file
/// is in exactly one folder. Choosing here moves the file — the same act
/// the user could perform in Finder, which is why the two can never
/// disagree about where a book lives.
function shelfPicker(d: DrawerDoc, all: string[]): HTMLElement {
  const o = opts!;
  const here = d.collections[0] ?? "";
  if (!o.edit) {
    return text(here || "none");
  }

  const wrap = document.createElement("div");
  wrap.className = "dshelf";

  const move = async (shelf: string) => {
    try {
      await o.edit!.moveToShelf(d.id, shelf);
    } catch (e) {
      o.onError(`${e}`);
      return;
    }
    o.onChanged(d.id);
    openDrawer(d.id);
  };

  const pick = document.createElement("select");
  for (const name of ["", ...all]) {
    const opt = document.createElement("option");
    opt.value = name;
    opt.textContent = name || "— none —";
    opt.selected = name === here;
    pick.append(opt);
  }
  pick.addEventListener("change", () => void move(pick.value));
  pick.addEventListener("keydown", (e) => e.stopPropagation());

  const fresh = document.createElement("input");
  fresh.type = "text";
  fresh.placeholder = "new shelf…";
  fresh.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter" && fresh.value.trim()) void move(fresh.value.trim());
  });

  wrap.append(pick, fresh);
  return wrap;
}

function row(label: string, ...content: (Node | string)[]): HTMLElement {
  const r = document.createElement("div");
  r.className = "drow";
  const l = document.createElement("div");
  l.className = "dlabel";
  l.textContent = label;
  const v = document.createElement("div");
  v.className = "dval";
  v.append(...content);
  r.append(l, v);
  return r;
}

function text(s: string): HTMLElement {
  const span = document.createElement("span");
  span.textContent = s;
  return span;
}
