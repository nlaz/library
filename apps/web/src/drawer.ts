// Reader metadata drawer: view + edit a document's title and collections,
// plus read-only facts (id, pages, ingest status). main.ts supplies the data
// sources and (desktop only) the write commands via initDrawer — the web
// build passes edit: null and gets a read-only panel.

import {
  annotations,
  annotationsDoc,
  onAnnotationsChanged,
  openPopover,
  removeAnnotation,
} from "./annotations";
import type { AnnotRec, Collections, DocStatus } from "./types";

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
    setCollections(doc: string, names: string[]): Promise<void>;
  } | null;
  onChanged(doc: string): void;
  onError(msg: string): void;
};

const $drawer = document.getElementById("reader-drawer")!;
const $toggle = document.getElementById("reader-meta")!;
let opts: Opts | null = null;
let openFor = ""; // doc id the drawer is showing; "" = closed

export function initDrawer(o: Opts) {
  opts = o;
  $toggle.addEventListener("click", () => {
    const doc = o.currentDoc();
    if (!doc) return;
    if (openFor === doc) closeDrawer();
    else openDrawer(doc);
  });
  // keep the marginalia section live while the drawer is open
  onAnnotationsChanged(() => {
    if (openFor && openFor === annotationsDoc()) openDrawer(openFor);
  });
}

export function closeDrawer() {
  $drawer.hidden = true;
  openFor = "";
}

async function openDrawer(doc: string) {
  if (!opts) return;
  openFor = doc;
  $drawer.hidden = false;
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

  const { el: checklist } = collectionsChecklist(
    Object.keys(cols),
    d.collections,
    async (names) => {
      try {
        await o.edit!.setCollections(d.id, names);
      } catch (e) {
        o.onError(`collections: ${e}`);
      }
      o.onChanged(d.id);
      openDrawer(d.id); // re-render with fresh membership
    },
    !o.edit,
  );
  const colRow = row("collections", checklist);
  if (o.edit) colRow.querySelector(".dval")!.append(newCollectionInput(d, checklist));
  rows.push(colRow);

  rows.push(row("file", text(d.id)));
  rows.push(row("pages", text(d.pages ? `${d.pages} pp.` : "—")));
  const s = d.status;
  rows.push(row("status", text(s ? s.state + (s.error ? ` — ${s.error}` : "") : "ready")));
  if (d.id === annotationsDoc()) rows.push(row("marginalia", marginaliaList()));

  $drawer.replaceChildren(...rows);
}

/** Every mark in the open doc, page order (annotations() is pre-sorted);
 * click jumps the scroll to the mark and opens its margin card. */
function marginaliaList(): HTMLElement {
  const el = document.createElement("div");
  el.className = "mnotes";
  const marks = annotations();
  if (!marks.length) {
    const none = document.createElement("span");
    none.className = "dnone";
    none.textContent = "none yet — select text, or press r";
    el.append(none);
    return el;
  }
  for (const a of marks) el.append(marginaliaRow(a));
  return el;
}

function marginaliaRow(a: AnnotRec): HTMLElement {
  const r = document.createElement("div");
  r.className = "mrow";
  const loc = document.createElement("span");
  loc.className = "mloc";
  loc.textContent = `p.${a.page}${a.kind === "region" ? " · region" : ""}`;
  const t = document.createElement("span");
  t.className = "mtext";
  t.textContent = a.note || (a.kind === "text" ? `“${a.text}”` : "—");
  const del = document.createElement("button");
  del.className = "mdel";
  del.title = "Remove mark";
  del.textContent = "✕";
  del.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      await removeAnnotation(a.id);
    } catch (err) {
      opts?.onError(`remove mark: ${err}`);
    }
  });
  r.append(loc, t, del);
  r.addEventListener("click", () => openPopover(a.id, true));
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

function newCollectionInput(d: DrawerDoc, checklist: HTMLElement): HTMLElement {
  const o = opts!;
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = "new collection…";
  input.addEventListener("keydown", async (e) => {
    e.stopPropagation();
    if (e.key !== "Enter" || !input.value.trim()) return;
    const names = [...checkedNames(checklist), input.value.trim()];
    try {
      await o.edit!.setCollections(d.id, names);
    } catch (err) {
      o.onError(`collections: ${err}`);
    }
    o.onChanged(d.id);
    openDrawer(d.id);
  });
  return input;
}

// ---------------------------------------------------------------------------
// collections checklist, shared with the book-card "⋯" menu
// ---------------------------------------------------------------------------

function checkedNames(el: HTMLElement): string[] {
  return [...el.querySelectorAll<HTMLInputElement>("input[type=checkbox]")]
    .filter((c) => c.checked)
    .map((c) => c.dataset.col!);
}

export function collectionsChecklist(
  all: string[],
  current: string[],
  apply: (names: string[]) => void,
  disabled = false,
): { el: HTMLElement; checked: () => string[] } {
  const el = document.createElement("div");
  el.className = "mcols";
  for (const name of all) {
    const label = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.dataset.col = name;
    box.checked = current.includes(name);
    box.disabled = disabled;
    box.addEventListener("change", () => apply(checkedNames(el)));
    label.append(box, name);
    el.append(label);
  }
  if (!all.length) {
    const none = document.createElement("span");
    none.className = "dnone";
    none.textContent = "none";
    el.append(none);
  }
  return { el, checked: () => checkedNames(el) };
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
