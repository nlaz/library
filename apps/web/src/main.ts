import { pageImg, pageUrl } from "./assets";
import { initChat } from "./chat";
import { closeDrawer, collectionsChecklist, initDrawer } from "./drawer";
import { hlBoxes } from "./highlights";
import {
  clearReaderHits,
  closeReader,
  onHitStep,
  openReader,
  readerDoc,
  readerOpen,
  setReaderHits,
  stepHit,
} from "./reader";
import { perfOpen, togglePerf } from "./perf";
import { isTauri, makeTransport, type Transport } from "./transport";
import type { Collections, DocInfo, IngestEvent, WireHit, WireResponse } from "./types";

const $q = document.getElementById("q") as HTMLInputElement;
const $cols = document.getElementById("cols")!;
let col = ""; // "" = everything, else a collection name
const $home = document.getElementById("home")!;
const $search = document.getElementById("search")!;
const $stats = document.getElementById("stats")!;
const $results = document.getElementById("results")!;
const $more = document.getElementById("more")!;
const $main = document.querySelector("main")!;
const $overlay = document.getElementById("overlay")!;
const $viewerLabel = document.getElementById("viewer-label")!;
const $viewerRead = document.getElementById("viewer-read")!;
const $viewerClose = document.getElementById("viewer-close")!;
const $pageImg = document.getElementById("page-img") as HTMLImageElement;
const $pageHl = document.getElementById("page-hl")!;
const $dropzone = document.getElementById("dropzone")!;
const $searchPop = document.getElementById("search-pop")!;
const $ac = document.getElementById("ac") as HTMLUListElement;
const $searchNav = document.getElementById("search-nav")!;
const $searchCount = document.getElementById("search-count")!;
const $searchPrev = document.getElementById("search-prev")!;
const $searchNext = document.getElementById("search-next")!;
const $toast = document.getElementById("toast")!;

const FULL_DEBOUNCE_MS = 60;
// once the query first reaches this many characters, fire the hybrid/image
// query immediately instead of waiting out the debounce — the first
// meaningful result set shows up sooner, and typing further still debounces
// normally off this point
const FULL_FLUSH_CHARS = 3;
const PREVIEW_H = 190; // px, keep in sync with .preview height in style.css
// one page of the blended result order; keep in sync with K in
// library-server/src/main.rs and library-app/src/lib.rs
const PAGE = 20;

let seq = 0;
let rendered = 0; // highest seq drawn; anything older is stale
let instantSeq = 0; // seq of the in-flight instant query; 0 = none
let instantDirty = false; // input changed while an instant was in flight
// dev-only: seq -> performance.now() at send, for round-trip logging
const sentAt = new Map<number, number>();
// seqs sent doc-scoped (reader find) and to which doc — their responses
// feed the reader's hit ticks, never the library results grid
const sentDoc = new Map<number, string>();
// infinite scroll: continuation seqs and what they were for, so a late
// page-N can be dropped if the query/col/pagination state moved on
const sentMore = new Map<number, { q: string; col: string; offset: number }>();
let moreOffset = 0; // next offset to request = blended hits consumed so far
let endReached = false;
// cross-page dedup: each continuation is its own fjall snapshot, so a
// mid-ingest index change can drift the slices slightly
const seen = new Set<string>();
const hitKey = (h: WireHit) => `${h.kind}|${h.doc}|${h.page}|${h.idx}`;
// send() closes over main()'s state; route() (top-level) needs it too
let sendQuery: (mode: "instant" | "full") => void = () => {};
let transport: Transport;
let desktop: typeof import("./tauri") | null = null;
let docList: DocInfo[] = [];
const ingesting = new Map<string, IngestEvent>();

// ---------------------------------------------------------------------------
// routing: #/ = home (shelves) or search results; #/read/<doc>?p=N = reader
// ---------------------------------------------------------------------------

async function route() {
  const m = location.hash.match(/^#\/read\/([^?]+)(?:\?p=(\d+))?$/);
  closeDrawer(); // drawer is per-doc; any navigation invalidates it
  if (m) {
    const doc = decodeURIComponent(m[1]);
    // no explicit ?p= -> the reader resumes the remembered position
    openReader(doc, await pagesOf(doc), m[2] ? Number(m[2]) : undefined, docTitle(doc));
  } else {
    closeReader();
  }
  // crossing the library/reader boundary re-scopes the query: in-flight
  // answers for the old scope are dropped by seq, this refreshes the new one
  sentDoc.clear();
  if ($q.value.trim()) sendQuery("full");
  $searchNav.hidden = !readerOpen();
}
window.addEventListener("hashchange", route);

/** Total page count for a doc, so the reader knows how far it can scroll.
 * The desktop build already has this in `docList`; the web build has no
 * such command, so it asks the server directly. */
async function pagesOf(id: string): Promise<number> {
  const info = docList.find((d) => d.id === id);
  if (info) return info.pages;
  try {
    const res = await fetch(`/api/pages/${encodeURIComponent(id)}`);
    if (res.ok) return (await res.json()).pages;
  } catch {
    // offline/unreachable — fall through to the generous fallback below
  }
  return 9999;
}

function prettify(id: string): string {
  return id
    .split("-")
    .map((w) => (w.length > 2 ? w[0].toUpperCase() + w.slice(1) : w))
    .join(" ");
}

function displayTitle(d: DocInfo): string {
  return d.title ?? prettify(d.id);
}

/** The doc's display name (its title, or a prettified id), for any doc id
 * shown in the UI — search results never show the raw file name. */
function docTitle(id: string): string {
  const d = docList.find((x) => x.id === id);
  return d ? displayTitle(d) : prettify(id);
}

// ---------------------------------------------------------------------------
// collections tabs
// ---------------------------------------------------------------------------

async function loadCollections() {
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
}

// ---------------------------------------------------------------------------
// home: shelves of books (desktop only — needs the docs command)
// ---------------------------------------------------------------------------

async function renderHome() {
  if (!desktop) return;
  const [ds, cols] = await Promise.all([desktop.docs(), transport.collections()]);
  docList = ds;

  const byId = new Map(ds.map((d) => [d.id, d]));
  const shelves: [string, DocInfo[]][] = Object.entries(cols)
    .map(([name, ids]) => [name, ids.map((id) => byId.get(id)).filter(Boolean)] as [string, DocInfo[]])
    .filter(([, docs]) => docs.length > 0);
  const sorted = new Set(Object.values(cols).flat());
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
      row.append(...docs.map((d) => bookCard(d, cols)));
      shelf.append(h, row);
      return shelf;
    }),
  );
  if (!visible.length) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "the library is empty — drop a PDF anywhere";
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
    img.src = pageImg(d.id, 1);
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
    await loadCollections();
    renderHome();
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
    await loadCollections();
    renderHome();
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

const STAGE_LABEL: Record<string, string> = {
  ocr: "reading pages",
  clean: "cleaning text",
  embed: "indexing text",
  figures: "finding figures",
  clip: "indexing figures",
  indexing: "committing",
  queued: "queued",
  staged: "waiting to index",
};

/** Progress-shaped view of a persisted status, for cards with no live
 * event yet (e.g. right after launch while the doc is mid-ingest). */
function statusEvent(d: DocInfo): IngestEvent | undefined {
  const s = d.status;
  if (!s) return undefined;
  const stage = s.state === "preparing" ? (s.stage ?? "queued") : s.state;
  return { doc: d.id, stage: stage as IngestEvent["stage"], done: s.done, total: s.total, message: "" };
}

function updateBookProgress(el: Element, e?: IngestEvent) {
  const sub = el.querySelector(".bsub");
  const fill = el.querySelector<HTMLElement>(".progress .fill");
  if (!e || !sub || !fill) return;
  const frac = e.total > 0 ? e.done / e.total : 0;
  fill.style.width = `${Math.round(frac * 100)}%`;
  const n = e.total > 0 ? ` ${e.done}/${e.total}` : "";
  sub.textContent = `${STAGE_LABEL[e.stage] ?? e.stage}${n}`;
}

// ---------------------------------------------------------------------------
// ingest wiring (desktop only)
// ---------------------------------------------------------------------------

let libraryDir = ""; // <data>/pdfs, for the move-confirm dialog

async function queuePdfs(paths: string[]) {
  if (!desktop) return;
  const pdfs = paths.filter((p) => p.toLowerCase().endsWith(".pdf"));
  if (!pdfs.length) return;
  // the library owns its documents: adding a PDF MOVES it into the
  // library folder, and that never happens without the user saying so
  const names = pdfs.map((p) => p.split("/").pop() ?? p);
  if (!(await desktop.confirmMove(names, libraryDir))) return;
  try {
    const queued = await desktop.ingestPaths(pdfs, col || null, "move");
    // queued docs show up on the shelves; only silence needs explaining
    if (!queued.length) notify("already in the queue");
  } catch (e) {
    notify(`add failed: ${e}`, { sticky: true });
  }
  renderHome();
}

function wireDesktop() {
  if (!desktop) return;
  desktop
    .getSettings()
    .then((s) => (libraryDir = `${s.data}/pdfs`))
    .catch(() => {});
  desktop.onDragDrop(
    () => ($dropzone.hidden = false),
    () => ($dropzone.hidden = true),
    (paths) => queuePdfs(paths),
  );
  desktop.onIngestProgress((e) => {
    if (e.stage === "log") return;
    if (e.stage === "done" || e.stage === "error") {
      ingesting.delete(e.doc);
      // "done" needs no announcement — the book appears on the shelf
      if (e.stage === "error") notify(`ingest failed: ${e.message}`, { sticky: true });
      loadCollections();
      renderHome();
      return;
    }
    ingesting.set(e.doc, e);
    const elCard = $home.querySelector(`.book[data-doc="${CSS.escape(e.doc)}"]`);
    if (elCard) updateBookProgress(elCard, e);
    else renderHome(); // first event for a doc we haven't drawn yet
  });
  desktop.onAppError((msg) => {
    notify(msg, { sticky: true });
  });
  desktop.onAppWaiting((msg) => {
    notify(msg, { sticky: true });
  });
}

// ---------------------------------------------------------------------------
// toast: transient notices bottom-left; sticky (errors) stay until clicked
// ---------------------------------------------------------------------------

let toastTimer: ReturnType<typeof setTimeout> | undefined;

function notify(msg: string, opts?: { sticky?: boolean }) {
  $toast.textContent = msg;
  $toast.classList.toggle("error", !!opts?.sticky);
  $toast.hidden = false;
  clearTimeout(toastTimer);
  if (!opts?.sticky) toastTimer = setTimeout(() => ($toast.hidden = true), 4000);
}
$toast.addEventListener("click", () => ($toast.hidden = true));

// ---------------------------------------------------------------------------
// search results (unchanged card/viewer logic, URLs via pageUrl)
// ---------------------------------------------------------------------------

function card(hit: WireHit): { el: HTMLElement; place: () => void } {
  const el = document.createElement("div");
  el.className = "card";

  const isImage = hit.kind === "image";
  if (isImage) el.classList.add("img");

  const pv = document.createElement("div");
  pv.className = "preview";
  const inner = document.createElement("div");
  inner.className = "pv-inner";
  const img = document.createElement("img");
  img.src = pageUrl(hit.img);
  img.loading = "lazy";
  // an image card's crop IS the figure — an amber wash over the whole
  // preview would just be noise, so boxes only decorate text cards
  inner.append(img, ...(isImage ? [] : hlBoxes(hit.boxes)));
  pv.append(inner);

  const loc = document.createElement("div");
  loc.className = "loc";
  loc.textContent = `${docTitle(hit.doc)} · p.${hit.page}${isImage ? " · figure" : ""}`;
  const meta = document.createElement("div");
  meta.className = "meta";
  meta.append(loc);
  if (!isImage) {
    const snip = document.createElement("div");
    snip.className = "snip";
    for (const w of hit.snippet) {
      const node = w.m ? document.createElement("mark") : document.createElement("span");
      node.textContent = w.t;
      snip.append(node, " ");
    }
    meta.append(snip);
  }
  el.append(pv, meta);
  el.addEventListener("click", () => openViewer(hit));

  // zoom so the chunk's text block (server-provided crop, which excludes the
  // scan's white margins) fills the card width, then CENTER the first match
  // in the window — clamped first to the content crop (so an edge match
  // zooms/pans instead of dragging margin white into view), then to the
  // page. Needs the card in the DOM and the image loaded.
  const place = () => {
    const w = pv.clientWidth;
    const pvH = pv.clientHeight || PREVIEW_H;
    if (!w || !img.naturalWidth) return;
    const [cx, cy, cw, ch] = hit.crop;
    const box = hit.boxes[0];
    // fill the card with the crop, but never so tight the match box (plus
    // some context) would overflow the window
    let z = Math.min(Math.max(cw, 0.05), 1);
    if (box) z = Math.min(Math.max(z, box[2] * 1.3), 1);
    const dispW = w / z;
    const dispH = dispW * (img.naturalHeight / img.naturalWidth);
    inner.style.width = `${dispW}px`;
    inner.style.height = `${dispH}px`;
    // want: match centered; lo..hi: window stays inside the crop (when the
    // crop is smaller than the window, pin to its leading edge); 0..max:
    // and always inside the page
    const win = (want: number, lo: number, hi: number, max: number) =>
      Math.min(Math.max(Math.min(Math.max(want, lo), Math.max(lo, hi)), 0), max);
    const xc = box ? box[0] + box[2] / 2 : cx + cw / 2;
    const yc = box ? box[1] + box[3] / 2 : cy + 0.1;
    const offX = win(xc * dispW - w / 2, cx * dispW, (cx + cw) * dispW - w, dispW - w);
    const offY = win(yc * dispH - pvH / 2, cy * dispH, (cy + ch) * dispH - pvH, dispH - pvH);
    inner.style.transform = `translate(${-offX}px, ${-offY}px)`;
  };
  if (!img.complete) img.addEventListener("load", place);
  return { el, place };
}

// ---------------------------------------------------------------------------
// infinite scroll: a sentinel under the grid requests the next PAGE-sized
// slice of the blended order; the server ends the stream when relevance
// drops below its MIN_REL cutoff (a short page = end)
// ---------------------------------------------------------------------------

let moreObserver: IntersectionObserver | null = null;

function setMoreState(s: "hidden" | "idle" | "loading" | "end") {
  $more.hidden = s === "hidden";
  $more.textContent = s === "loading" ? "· · ·" : s === "end" ? "· end ·" : "";
}

function resetPaging() {
  moreOffset = 0;
  endReached = false;
  seen.clear();
  sentMore.clear();
  setMoreState("hidden");
}

// IntersectionObserver fires on threshold *crossings* — a sentinel that is
// still visible after an append (thin page) needs a fresh observation to
// get another callback
function rearmSentinel() {
  moreObserver?.unobserve($more);
  moreObserver?.observe($more);
}

function render(msg: WireResponse) {
  const q = $q.value.trim();
  if (!q) return;

  // "settled" = this response answers what's in the box right now; an older
  // query's empty answer must not flash "no matches" over a pending one
  const settled = msg.seq >= seq;
  if (!settled && msg.hits.length === 0) return;

  $stats.textContent = settled
    ? `${msg.hits.length} hits · ${msg.phase} · ${(msg.us / 1000).toFixed(1)}ms`
    : "searching…";

  const t0 = import.meta.env.DEV ? performance.now() : 0;
  const cards = msg.hits.map(card);
  $results.replaceChildren(...cards.map((c) => c.el));
  if (import.meta.env.DEV) {
    console.debug(`[perf] seq=${msg.seq} dom_build=${(performance.now() - t0).toFixed(1)}ms (${cards.length} cards)`);
  }
  requestAnimationFrame(() => {
    const t1 = import.meta.env.DEV ? performance.now() : 0;
    cards.forEach((c) => c.place());
    if (import.meta.env.DEV) {
      console.debug(`[perf] seq=${msg.seq} place=${(performance.now() - t1).toFixed(1)}ms`);
    }
  });

  if (settled && msg.hits.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "no matches";
    $results.replaceChildren(empty);
  }

  // a replace render restarts pagination from this page-1
  seen.clear();
  for (const h of msg.hits) seen.add(hitKey(h));
  moreOffset = msg.hits.length;
  // an instant response can be short of PAGE and flag "end" prematurely;
  // the debounced full re-render 60ms later recomputes it
  endReached = settled && msg.hits.length < PAGE;
  setMoreState(!msg.hits.length ? "hidden" : endReached ? "end" : "idle");
  $main.scrollTop = 0;
  rearmSentinel();
}

function appendResults(msg: WireResponse) {
  const fresh = msg.hits.filter((h) => !seen.has(hitKey(h)));
  for (const h of fresh) seen.add(hitKey(h));
  const cards = fresh.map(card);
  $results.append(...cards.map((c) => c.el));
  requestAnimationFrame(() => cards.forEach((c) => c.place()));
  // raw server count, not `fresh.length` — offsets are server-side positions
  moreOffset += msg.hits.length;
  endReached = msg.hits.length < PAGE;
  setMoreState(endReached ? "end" : "idle");
  if (!endReached) rearmSentinel();
  $stats.textContent = `${$results.querySelectorAll(".card").length} hits · ${msg.phase} · ${(msg.us / 1000).toFixed(1)}ms`;
}

let viewerHit: WireHit | null = null;

function openViewer(hit: WireHit) {
  viewerHit = hit;
  $viewerLabel.textContent = `${docTitle(hit.doc)} · p. ${hit.page}`;
  $pageHl.replaceChildren(...hlBoxes(hit.boxes));
  const src = pageUrl(hit.img);
  const reveal = () => {
    const first = $pageHl.firstElementChild;
    if (first) first.scrollIntoView({ block: "center" });
  };
  if ($pageImg.src.endsWith(src)) {
    reveal();
  } else {
    $pageImg.src = src;
    $pageImg.addEventListener("load", reveal, { once: true });
  }
  $overlay.hidden = false;
}

function closeViewer() {
  $overlay.hidden = true;
}

$viewerRead.addEventListener("click", () => {
  if (!viewerHit) return;
  closeViewer();
  location.hash = `#/read/${encodeURIComponent(viewerHit.doc)}?p=${viewerHit.page}`;
});
$viewerClose.addEventListener("click", closeViewer);
$overlay.addEventListener("click", (e) => {
  if (e.target === $overlay) closeViewer();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !$overlay.hidden) closeViewer();
});

// ---------------------------------------------------------------------------
// search popover: Cmd+F opens it (right side, clear of the results), Escape
// closes it — and only it. Query text and results survive a close.
// ---------------------------------------------------------------------------

function openSearchPop() {
  $searchPop.hidden = false;
  $searchNav.hidden = !readerOpen();
  $q.focus();
  $q.select(); // retyping replaces, browser-find style
}

function closeSearchPop() {
  $searchPop.hidden = true;
}

// window + capture phase: the perf view sits above every other layer, so its
// keys must run before all the document-level handlers below (and before
// inputs' stopPropagation). Cmd+. toggles; Escape closes exactly this layer.
window.addEventListener(
  "keydown",
  (e) => {
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === ".") {
      e.preventDefault();
      e.stopPropagation();
      togglePerf();
    } else if (e.key === "Escape" && perfOpen()) {
      e.preventDefault();
      e.stopPropagation();
      togglePerf();
    }
  },
  true,
);

document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === "f") {
    e.preventDefault(); // the popover replaces native find in the web build
    openSearchPop();
  }
});

// capture phase: the reader's own Escape (registered earlier, bubble phase)
// would exit the reader before the popover saw the key — each press must
// close exactly one layer: viewer modal, then popover, then reader
document.addEventListener(
  "keydown",
  (e) => {
    if (e.key === "Escape" && !$searchPop.hidden && $overlay.hidden) {
      e.preventDefault();
      e.stopImmediatePropagation();
      closeSearchPop();
    }
  },
  true,
);

const showStep = (i: number, n: number) => {
  $searchCount.textContent = `${i + 1}/${n}`;
};
onHitStep(showStep);
$searchNext.addEventListener("click", () => stepHit(1));
$searchPrev.addEventListener("click", () => stepHit(-1));

$q.addEventListener("keydown", (e) => {
  // the reader's document-level hotkeys (Space/arrows scroll) must not see
  // keys typed into the box — same pattern as the book-menu inputs
  e.stopPropagation();
  if (e.key === "Enter" && readerOpen()) stepHit(e.shiftKey ? -1 : 1);
});

// ---------------------------------------------------------------------------
// mode switching: home shelves when the query is empty, results otherwise
// ---------------------------------------------------------------------------

function showSearch(active: boolean) {
  $search.hidden = !active;
  $home.hidden = active;
}

async function main() {
  if (isTauri()) desktop = await import("./tauri");
  transport = await makeTransport();
  wireDesktop();
  showSearch(false);
  renderHome();

  initChat({
    prettify,
    desktop: desktop ? { turn: desktop.chatTurn, cancel: desktop.chatCancel } : null,
  });

  initDrawer({
    currentDoc: readerDoc,
    getDoc: async (id) => {
      if (desktop) {
        docList = await desktop.docs();
        const d = docList.find((x) => x.id === id);
        if (d) return d;
      } else {
        try {
          const res = await fetch(`/api/doc/${encodeURIComponent(id)}`);
          if (res.ok) return await res.json();
        } catch {
          // offline/unreachable — fall through to the empty shape below
        }
      }
      return { id, title: null, pages: 0, collections: [], status: null };
    },
    getCollections: () => transport.collections(),
    prettify,
    // no write path in the web build — the drawer renders read-only
    edit: desktop ? { setTitle: desktop.setTitle, setCollections: desktop.setCollections } : null,
    onChanged: async (id) => {
      await loadCollections();
      await renderHome();
      // a rename must show in the reader chrome immediately
      if (readerDoc() === id) {
        document.getElementById("reader-title")!.textContent = docTitle(id);
      }
    },
    onError: (msg) => notify(msg, { sticky: true }),
  });

  await transport.ready();
  loadCollections();
  renderHome().then(route); // route needs page counts for #/read links

  // query text of the last "full" (hybrid+image) request sent, so a
  // catch-up "instant" — resent once a stale in-flight instant finally
  // resolves — doesn't fire (and later render over) a full response the
  // current text has already gotten
  let lastFullQuery = "";

  const send = (mode: "instant" | "full") => {
    const q = $q.value.trim();
    // reader open = the query is a find scoped to that doc; the library
    // views behind the reader must not churn
    const doc = readerDoc();
    if (!q) {
      rendered = ++seq;
      instantSeq = 0;
      instantDirty = false;
      sentDoc.clear();
      resetPaging();
      if (doc) {
        clearReaderHits();
        $searchCount.textContent = "";
      } else {
        $results.replaceChildren();
        showSearch(false);
        renderHome();
      }
      return;
    }
    if (!doc) showSearch(true);
    // single chars tokenize to nothing server-side — don't round-trip,
    // and don't let a stale in-flight answer render over this state
    if (q.length < 2) {
      rendered = ++seq;
      instantDirty = false;
      resetPaging();
      if (doc) {
        clearReaderHits();
        $searchCount.textContent = "";
      } else {
        $results.replaceChildren();
        $stats.textContent = "searching…";
      }
      return;
    }
    // at most one instant query in flight: a burst of keystrokes must not
    // stack concurrent scans on the engine — mark dirty and catch up when
    // the pending answer lands
    if (mode === "instant") {
      if (instantSeq > rendered) {
        instantDirty = true;
        return;
      }
      instantSeq = seq + 1;
    }
    if (mode === "full") lastFullQuery = q;
    if (!doc) $stats.textContent = "searching…";
    if (import.meta.env.DEV) sentAt.set(seq + 1, performance.now());
    if (doc) sentDoc.set(seq + 1, doc);
    // "" = search all kinds — the UI has no text/images toggle
    transport.send({ seq: ++seq, q, mode, col, kind: "", doc });
  };
  sendQuery = send;

  // infinite scroll continuation — deliberately NOT routed through send():
  // it must not touch lastFullQuery or the instant machinery
  const sendMore = () => {
    const q = $q.value.trim();
    if (readerOpen() || $search.hidden || endReached || q.length < 2) return;
    // settled-gate: only continue when nothing else is in flight, so a
    // continuation can never race an unanswered query
    if (seq !== rendered) return;
    if (sentMore.size) return; // one continuation at a time
    setMoreState("loading");
    sentMore.set(seq + 1, { q, col, offset: moreOffset });
    if (import.meta.env.DEV) sentAt.set(seq + 1, performance.now());
    transport.send({ seq: ++seq, q, mode: "full", col, kind: "", doc: "", offset: moreOffset });
  };
  moreObserver = new IntersectionObserver(
    (es) => {
      if (es.some((e) => e.isIntersecting)) sendMore();
    },
    { root: $main, rootMargin: "0px 0px 200% 0px" },
  );
  moreObserver.observe($more);

  transport.onResponse((msg) => {
    if (import.meta.env.DEV) {
      const t0 = sentAt.get(msg.seq);
      if (t0 !== undefined) {
        sentAt.delete(msg.seq);
        const roundtrip = performance.now() - t0;
        // roundtrip - server = time spent in IPC/transport + queuing, not
        // computing the answer — that's the "is the UI choked" signal
        console.debug(
          `[perf] seq=${msg.seq} roundtrip=${roundtrip.toFixed(1)}ms server=${(msg.us / 1000).toFixed(1)}ms transport=${(roundtrip - msg.us / 1000).toFixed(1)}ms`,
        );
      }
    }
    if (msg.seq === instantSeq) {
      instantSeq = 0;
      const dirty = instantDirty;
      instantDirty = false;
      // a full response for the current text is already in flight (or
      // rendered) — a lex-only catch-up would only regress what's on screen
      if (dirty && $q.value.trim() !== lastFullQuery) {
        send("instant"); // the box changed while we were waiting
      }
    }
    const hitDoc = sentDoc.get(msg.seq);
    sentDoc.delete(msg.seq);
    const more = sentMore.get(msg.seq);
    sentMore.delete(msg.seq);
    if (msg.seq < rendered) return; // superseded while in flight
    rendered = msg.seq;
    if (hitDoc) {
      // reader find: hits become ticks/highlights, never result cards
      if (readerDoc() === hitDoc) {
        const n = setReaderHits(msg.hits);
        const settled = msg.seq >= seq;
        $searchCount.textContent = settled && !n ? "no matches" : n ? `${n} hits` : "";
      }
      return;
    }
    if (more) {
      // sends are strictly seq-ordered, so a new query's page-1 always
      // outranks an outstanding continuation: if it rendered first, the
      // seq guard above dropped us; if it hasn't yet, these guards do —
      // append only when the grid still holds exactly `offset` hits of
      // this exact query/collection
      if (
        more.q !== $q.value.trim() ||
        more.col !== col ||
        more.offset !== moreOffset ||
        readerOpen()
      ) {
        setMoreState("idle");
        return;
      }
      appendResults(msg);
      return;
    }
    render(msg);
  });

  let fullTimer: ReturnType<typeof setTimeout>;
  let flushedLen = 0; // query length we last eagerly flushed at; 0 = not yet
  $q.addEventListener("input", () => {
    const q = $q.value.trim();
    send("instant"); // lexical, every keystroke
    clearTimeout(fullTimer);
    if (!q.length) {
      flushedLen = 0;
    } else if (!flushedLen && q.length >= FULL_FLUSH_CHARS) {
      flushedLen = q.length;
      send("full"); // enough to search meaningfully — don't wait for the pause
    }
    fullTimer = setTimeout(() => send("full"), FULL_DEBOUNCE_MS); // hybrid, on pause
  });

  // --- type-ahead: frequency-ranked word completions in a dropdown ---------
  // Additive and independent of the search grid + seq machinery. Stale
  // responses are dropped by a monotonic token (there is no request to abort).
  let acToken = 0;
  let acItems: string[] = [];
  let acActive = -1;
  let acTimer: ReturnType<typeof setTimeout>;

  const hideAc = () => {
    acItems = [];
    acActive = -1;
    $ac.hidden = true;
    $ac.replaceChildren();
  };
  const applyAc = (term: string) => {
    $q.value = term;
    hideAc();
    flushedLen = term.length;
    send("full");
    $q.focus();
  };
  const renderAc = () => {
    if (!acItems.length) return hideAc();
    $ac.replaceChildren(
      ...acItems.map((t, i) => {
        const li = document.createElement("li");
        li.textContent = t;
        if (i === acActive) li.className = "on";
        // mousedown (not click) so it fires before the input's blur hides us
        li.addEventListener("mousedown", (e) => {
          e.preventDefault();
          applyAc(t);
        });
        return li;
      }),
    );
    $ac.hidden = false;
  };
  const fetchAc = (q: string) => {
    const tok = ++acToken;
    transport
      .complete(q)
      .then((items) => {
        if (tok !== acToken) return; // a newer keystroke superseded this
        acItems = items.filter((t) => t !== q); // exact echo has nothing to add
        acActive = -1;
        renderAc();
      })
      .catch(() => {});
  };

  $q.addEventListener("input", () => {
    const q = $q.value.trim();
    clearTimeout(acTimer);
    // no completions for reader-find or sub-2-char queries
    if (q.length < 2 || readerDoc()) return hideAc();
    acTimer = setTimeout(() => fetchAc(q), 80);
  });
  $q.addEventListener("keydown", (e) => {
    if ($ac.hidden || !acItems.length) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      acActive = (acActive + 1) % acItems.length;
      renderAc();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      acActive = (acActive - 1 + acItems.length) % acItems.length;
      renderAc();
    } else if (e.key === "Enter") {
      if (acActive >= 0) {
        e.preventDefault();
        applyAc(acItems[acActive]);
      }
    } else if (e.key === "Escape") {
      hideAc();
    }
  });
  $q.addEventListener("blur", () => hideAc());

  $cols.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest("button");
    if (!btn) return;
    // no "Everything" tab: clicking the active collection again clears it
    col = col === btn.dataset.col ? "" : btn.dataset.col!;
    for (const b of $cols.children) b.classList.toggle("on", b === btn && col !== "");
    if ($q.value.trim()) send("full");
    else renderHome();
  });
}

main().catch((e) => {
  notify(`startup failed: ${e?.message ?? e}`, { sticky: true });
});
