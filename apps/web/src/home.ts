// ---------------------------------------------------------------------------
// home: collections tabs + shelves of books (desktop only — needs the docs
// command), including the book-card "…" menus and ingest-progress rendering.
// ---------------------------------------------------------------------------

import { coverImg } from "./assets";
import { $cols, $home } from "./dom";
import { collectionsChecklist } from "./drawer";
import { displayTitle, prettify, setDocList, STAGE_LABEL, statusEvent } from "./format";
import { desktop, transport } from "./state";
import { notify } from "./toast";
import type { Collections, DocInfo, IngestEvent } from "./types";

let col = ""; // "" = everything, else a collection name

export function getCol(): string {
  return col;
}

export function setCol(v: string) {
  col = v;
}

/** Live ingest events by doc id — written by the ingest wiring, read here to
 * seed progress bars on cards drawn mid-ingest. */
export const ingesting = new Map<string, IngestEvent>();

// ---------------------------------------------------------------------------
// collections tabs
// ---------------------------------------------------------------------------

/** Redraw the tabs and return the map, so callers that re-render the shelves
 * next can pass it along instead of fetching collections a second time. */
export async function loadCollections(): Promise<Collections> {
  const cols = await transport.collections();
  // the active collection can vanish (last doc removed) — fall back to all
  if (col && !(col in cols)) col = "";
  // no "Everything" tab: nothing selected IS everything
  $cols.replaceChildren(
    ...Object.keys(cols).map((name) => {
      const btn = document.createElement("button");
      btn.dataset.col = name;
      btn.textContent = name;
      const n = document.createElement("span");
      n.className = "n";
      n.textContent = String(cols[name].length);
      btn.append(n);
      if (name === col) btn.classList.add("on");
      return btn;
    }),
  );
  return cols;
}

export async function renderHome(cols?: Collections) {
  if (!desktop) return;
  const [ds, colMap] = await Promise.all([desktop.docs(), cols ?? transport.collections()]);
  setDocList(ds);

  const byId = new Map(ds.map((d) => [d.id, d]));
  const shelves: [string, DocInfo[]][] = Object.entries(colMap)
    .map(([name, ids]) => [name, ids.map((id) => byId.get(id)).filter(Boolean)] as [string, DocInfo[]])
    .filter(([, docs]) => docs.length > 0);
  const sorted = new Set(Object.values(colMap).flat());
  const unsorted = ds.filter((d) => !sorted.has(d.id));
  if (unsorted.length) shelves.push(["Unsorted", unsorted]);

  const visible = col ? shelves.filter(([name]) => name === col) : shelves;
  $home.replaceChildren(
    ...visible.map(([name, docs]) => {
      const shelf = document.createElement("section");
      shelf.className = "shelf";
      const h = document.createElement("h2");
      h.textContent = name;
      const row = document.createElement("div");
      row.className = "books";
      row.append(...docs.map((d) => bookCard(d, colMap)));
      shelf.append(h, row);
      return shelf;
    }),
  );
  if (!visible.length) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "the library is empty — drop a PDF or image anywhere";
    $home.replaceChildren(empty);
  }
}

function bookCard(d: DocInfo, cols: Collections): HTMLElement {
  const el = document.createElement("div");
  el.className = "book";
  el.dataset.doc = d.id;

  const cover = document.createElement("div");
  cover.className = "cover";
  if (d.pages > 0) {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.decoding = "async";
    img.src = coverImg(d.id);
    cover.append(img);
  }
  const title = document.createElement("div");
  title.className = "btitle";
  title.textContent = displayTitle(d);
  const sub = document.createElement("div");
  sub.className = "bsub";
  sub.textContent = d.pages ? `${d.pages} pp.` : "queued";

  el.append(cover, title, sub);

  const state = d.status?.state;
  if (state === "failed") {
    // error badge + retry; the card stays otherwise inert
    el.classList.add("failed");
    sub.textContent = "indexing failed";
    if (d.status?.error) el.title = d.status.error;
    const retry = document.createElement("button");
    retry.className = "bretry";
    retry.textContent = "Retry";
    retry.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        await desktop!.retryDoc(d.id);
      } catch (err) {
        notify(`retry: ${err}`, { sticky: true });
      }
      renderHome();
    });
    cover.append(retry);
    if (desktop) el.append(bookMenu(el, d, cols));
    return el;
  }

  if (d.processing) {
    el.classList.add("processing");
    const bar = document.createElement("div");
    bar.className = "progress";
    const fill = document.createElement("div");
    fill.className = "fill";
    bar.append(fill);
    cover.append(bar);
    // a live event wins; the persisted status seeds the initial render
    updateBookProgress(el, ingesting.get(d.id) ?? statusEvent(d));
  } else {
    // text_ready reads and searches fine — figures are still indexing, so
    // keep a quiet badge but leave the card fully usable
    if (state === "text_ready") {
      el.classList.add("finishing");
      sub.textContent = `${d.pages} pp. · indexing figures…`;
    }
    el.addEventListener("click", () => {
      location.hash = `#/read/${encodeURIComponent(d.id)}`;
    });
    if (desktop) el.append(bookMenu(el, d, cols));
  }
  return el;
}

// ---------------------------------------------------------------------------
// book-card "…" menu: rename / collections / delete (desktop only)
// ---------------------------------------------------------------------------

function closeMenus() {
  for (const m of document.querySelectorAll(".book-menu")) m.remove();
}
document.addEventListener("click", closeMenus);

function bookMenu(card: HTMLElement, d: DocInfo, cols: Collections): HTMLElement {
  const btn = document.createElement("button");
  btn.className = "bmenu";
  btn.textContent = "⋯";
  btn.title = "Rename, collections, delete";
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    const open = card.querySelector(".book-menu");
    closeMenus();
    if (!open) card.append(menuPanel(card, d, cols));
  });
  return btn;
}

function menuPanel(card: HTMLElement, d: DocInfo, cols: Collections): HTMLElement {
  const panel = document.createElement("div");
  panel.className = "book-menu";
  panel.addEventListener("click", (e) => e.stopPropagation());

  const rename = document.createElement("button");
  rename.textContent = "Rename";
  rename.addEventListener("click", () => {
    panel.remove();
    renameInline(card, d);
  });

  const apply = async (names: string[]) => {
    try {
      await desktop!.setCollections(d.id, names);
    } catch (e) {
      notify(`collections: ${e}`, { sticky: true });
    }
    renderHome(await loadCollections());
  };
  const { el: colList, checked } = collectionsChecklist(Object.keys(cols), d.collections, apply);
  const newCol = document.createElement("input");
  newCol.type = "text";
  newCol.placeholder = "new collection…";
  newCol.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter" && newCol.value.trim()) {
      apply([...checked(), newCol.value.trim()]);
    }
    if (e.key === "Escape") closeMenus();
  });

  const del = document.createElement("button");
  del.className = "danger";
  del.textContent = "Delete…";
  del.addEventListener("click", async () => {
    closeMenus();
    if (!(await desktop!.confirmDelete(displayTitle(d)))) return;
    try {
      await desktop!.deleteDoc(d.id);
      localStorage.removeItem(`pos:${d.id}`);
      if (location.hash.startsWith(`#/read/${encodeURIComponent(d.id)}`)) location.hash = "#/";
    } catch (e) {
      notify(`delete failed: ${e}`, { sticky: true });
    }
    renderHome(await loadCollections());
  });

  panel.append(rename, colList, newCol, del);
  return panel;
}

function renameInline(card: HTMLElement, d: DocInfo) {
  const title = card.querySelector<HTMLElement>(".btitle");
  if (!title) return;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "brename";
  input.value = displayTitle(d);
  input.addEventListener("click", (e) => e.stopPropagation());
  let done = false;
  const commit = async () => {
    if (done) return;
    done = true;
    // storing the prettified id would freeze the fallback; treat it as "unset"
    const v = input.value.trim() === prettify(d.id) ? "" : input.value;
    try {
      await desktop!.setTitle(d.id, v);
    } catch (e) {
      notify(`rename: ${e}`, { sticky: true });
    }
    renderHome();
  };
  input.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter") commit();
    if (e.key === "Escape") {
      done = true;
      renderHome();
    }
  });
  input.addEventListener("blur", commit);
  title.replaceChildren(input);
  input.focus();
  input.select();
}

export function updateBookProgress(el: Element, e?: IngestEvent) {
  const sub = el.querySelector(".bsub");
  const fill = el.querySelector<HTMLElement>(".progress .fill");
  if (!e || !sub || !fill) return;
  const frac = e.total > 0 ? e.done / e.total : 0;
  fill.style.width = `${Math.round(frac * 100)}%`;
  const n = e.total > 0 ? ` ${e.done}/${e.total}` : "";
  sub.textContent = `${STAGE_LABEL[e.stage] ?? e.stage}${n}`;
}
