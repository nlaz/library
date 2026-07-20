// The pen: persistent marks on reader pages. Text highlights come from
// selecting words in the OCR text layer (a floating toolbar offers
// Highlight / + Note); region marks from an `r`-key drag. Marks render in
// a per-page .alayer with the same attach-at-image-load / discard-at-
// unload lifecycle as the find-hit layer, and carry an optional margin
// note in a popover card. Persistence rides marginalia-api (Tauri or
// HTTP — both land in the same sidecars + search index).

import { deleteAnnotation, listAnnotations, saveAnnotation } from "./marginalia-api";
import { dragBox, lineBoxes, negligible, selectionText } from "./selection";
import { notify } from "./toast";
import type { AnnotRec, Box, OcrWord } from "./types";

const $reader = document.getElementById("reader")!;
const $scroll = document.getElementById("reader-scroll")!;
const $pagesEl = document.getElementById("reader-pages")!;

let currentDoc = "";
let annots: AnnotRec[] = [];
let marksVisible = true;
let onChange: (() => void) | null = null;

/** Reader-side hook: fires after any create/edit/delete so dependent
 * surfaces (drawer list, ticks) can refresh. */
export function onAnnotationsChanged(cb: () => void) {
  onChange = cb;
}

export function annotations(): AnnotRec[] {
  return annots;
}

export function annotationsDoc(): string {
  return currentDoc;
}

/** Load a doc's marks and (re)decorate whatever pages are rendered. */
export async function setAnnotationsDoc(doc: string) {
  currentDoc = doc;
  annots = [];
  redecorate();
  try {
    const loaded = await listAnnotations(doc);
    if (currentDoc !== doc) return; // doc changed mid-fetch
    annots = loaded;
    redecorate();
    onChange?.();
  } catch {
    // offline host or cold engine — the reader still reads
  }
}

function redecorate() {
  for (const el of $pagesEl.children) attachAnnotLayer(el as HTMLElement);
}

function changed() {
  redecorate();
  onChange?.();
}

// ---------------------------------------------------------------------------
// mark rendering: one .alayer per page, boxes from creation-time snapshots
// ---------------------------------------------------------------------------

function markBoxes(a: AnnotRec): Box[] {
  return a.kind === "text" ? a.boxes : [a.bbox];
}

/** Same contract as reader.ts attachHitLayer: call at image load and after
 * any annotation change; unloading a page discards the layer with it. */
export function attachAnnotLayer(el: HTMLElement) {
  el.querySelector(".alayer")?.remove();
  if (!el.firstElementChild) return; // unloaded placeholder
  const page = Number(el.dataset.page);
  const marks = annots.filter((a) => a.page === page);
  if (!marks.length) return;
  const layer = document.createElement("div");
  layer.className = "alayer";
  if (!marksVisible) layer.classList.add("off");
  for (const a of marks) {
    for (const [x, y, w, h] of markBoxes(a)) {
      const b = document.createElement("div");
      b.className = a.kind === "region" ? "mark region" : "mark";
      if (a.note) b.classList.add("noted");
      b.dataset.annot = a.id;
      b.style.left = `${x * 100}%`;
      b.style.top = `${y * 100}%`;
      b.style.width = `${w * 100}%`;
      b.style.height = `${h * 100}%`;
      layer.append(b);
    }
  }
  el.append(layer);
}

/** `m`: a clean page for pure reading; marks stay loaded, just hidden. */
export function toggleMarks() {
  marksVisible = !marksVisible;
  for (const l of $pagesEl.querySelectorAll(".alayer")) l.classList.toggle("off", !marksVisible);
}

// ---------------------------------------------------------------------------
// selection toolbar: select words → Highlight / + Note
// ---------------------------------------------------------------------------

const toolbar = document.createElement("div");
toolbar.id = "sel-toolbar";
toolbar.hidden = true;
$reader.append(toolbar);

const tbHighlight = tbButton("highlight", true);
const tbNote = tbButton("+ note", false);
toolbar.append(tbHighlight, tbNote);

function tbButton(label: string, prime: boolean): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = label;
  if (prime) b.classList.add("prime");
  return b;
}

type Pending = { page: number; w0: number; w1: number; words: OcrWord[] };
let pending: Pending | null = null;

/** Map the live selection to a word range in one page's text layer.
 * Span order in a .tlayer IS word order (attachTextLayer appends in OCR
 * sequence), so indices come straight from the DOM. */
function selectionToPending(): Pending | null {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || !sel.rangeCount) return null;
  const range = sel.getRangeAt(0);
  const spanOf = (n: Node) =>
    (n instanceof Element ? n : n.parentElement)?.closest<HTMLElement>(".tw") ?? null;
  const start = spanOf(range.startContainer);
  const end = spanOf(range.endContainer);
  if (!start || !end) return null;
  const layer = start.parentElement;
  if (!layer || layer !== end.parentElement) return null; // cross-page: v1 declines
  const pageEl = layer.closest<HTMLElement>(".rpage");
  if (!pageEl) return null;
  const spans = Array.from(layer.children) as HTMLElement[];
  let w0 = spans.indexOf(start);
  let w1 = spans.indexOf(end);
  if (w0 < 0 || w1 < 0) return null;
  if (w0 > w1) [w0, w1] = [w1, w0];
  const words = spans.slice(w0, w1 + 1).map((s) => ({
    t: (s.textContent ?? "").trim(),
    x: parseFloat(s.style.left) / 100,
    y: parseFloat(s.style.top) / 100,
    w: parseFloat(s.style.width) / 100,
    h: parseFloat(s.style.height) / 100,
  }));
  return { page: Number(pageEl.dataset.page), w0, w1: w1 + 1, words };
}

function showToolbar() {
  pending = selectionToPending();
  if (!pending) {
    toolbar.hidden = true;
    return;
  }
  const sel = window.getSelection()!;
  const r = sel.getRangeAt(0).getBoundingClientRect();
  const host = $reader.getBoundingClientRect();
  toolbar.hidden = false;
  const left = Math.max(8, r.left - host.left + r.width / 2 - toolbar.offsetWidth / 2);
  toolbar.style.left = `${left}px`;
  toolbar.style.top = `${Math.max(8, r.top - host.top - toolbar.offsetHeight - 8)}px`;
}

function hideToolbar() {
  toolbar.hidden = true;
  pending = null;
}

async function commitHighlight(openNote: boolean) {
  if (!pending) return;
  const { page, w0, w1, words } = pending;
  hideToolbar();
  window.getSelection()?.removeAllRanges();
  try {
    const saved = await saveAnnotation({
      id: "",
      doc: currentDoc,
      page,
      kind: "text",
      w0,
      w1,
      text: selectionText(words),
      boxes: lineBoxes(words),
      note: "",
      created: 0,
    });
    annots.push(saved);
    changed();
    if (openNote) openPopover(saved.id);
  } catch (e) {
    notify(`couldn't save highlight: ${e instanceof Error ? e.message : e}`);
  }
}

tbHighlight.addEventListener("click", () => commitHighlight(false));
tbNote.addEventListener("click", () => commitHighlight(true));

document.addEventListener("mouseup", (e) => {
  if ($reader.hidden) return;
  if (toolbar.contains(e.target as Node) || popover.contains(e.target as Node)) return;
  // let the click settle the selection first
  requestAnimationFrame(showToolbar);
});

// ---------------------------------------------------------------------------
// region marks: `r`, then drag a rectangle on a page
// ---------------------------------------------------------------------------

let regionMode = false;
let drag: { pageEl: HTMLElement; x: number; y: number; preview: HTMLElement } | null = null;

export function startRegionMode() {
  regionMode = true;
  $scroll.classList.add("region-mode");
}

function endRegionMode() {
  regionMode = false;
  $scroll.classList.remove("region-mode");
  drag?.preview.remove();
  drag = null;
}

const pageXY = (pageEl: HTMLElement, e: MouseEvent): [number, number] => {
  const r = pageEl.getBoundingClientRect();
  return [(e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height];
};

$pagesEl.addEventListener("mousedown", (e) => {
  if (!regionMode) return;
  const pageEl = (e.target as HTMLElement).closest<HTMLElement>(".rpage");
  if (!pageEl) return;
  e.preventDefault();
  const [x, y] = pageXY(pageEl, e);
  const preview = document.createElement("div");
  preview.className = "region-preview";
  pageEl.append(preview);
  drag = { pageEl, x, y, preview };
});

document.addEventListener("mousemove", (e) => {
  if (!drag) return;
  const [x, y] = pageXY(drag.pageEl, e);
  const [bx, by, bw, bh] = dragBox(drag.x, drag.y, x, y);
  Object.assign(drag.preview.style, {
    left: `${bx * 100}%`,
    top: `${by * 100}%`,
    width: `${bw * 100}%`,
    height: `${bh * 100}%`,
  });
});

document.addEventListener("mouseup", async (e) => {
  if (!drag) return;
  const { pageEl } = drag;
  const [x, y] = pageXY(pageEl, e);
  const bbox = dragBox(drag.x, drag.y, x, y);
  endRegionMode();
  if (negligible(bbox)) return;
  try {
    const saved = await saveAnnotation({
      id: "",
      doc: currentDoc,
      page: Number(pageEl.dataset.page),
      kind: "region",
      bbox,
      note: "",
      created: 0,
    });
    annots.push(saved);
    changed();
    openPopover(saved.id);
  } catch (err) {
    notify(`couldn't save region: ${err instanceof Error ? err.message : err}`);
  }
});

// ---------------------------------------------------------------------------
// the margin card: click a mark → note popover
// ---------------------------------------------------------------------------

const popover = document.createElement("div");
popover.id = "annot-pop";
popover.hidden = true;
popover.innerHTML = `
  <div class="ap-head"><span class="ap-when"></span><button class="ap-delete">remove</button></div>
  <textarea class="ap-note" rows="3" placeholder="margin note…"></textarea>
`;
$reader.append(popover);
const apWhen = popover.querySelector<HTMLElement>(".ap-when")!;
const apNote = popover.querySelector<HTMLTextAreaElement>(".ap-note")!;
const apDelete = popover.querySelector<HTMLButtonElement>(".ap-delete")!;
let popId: string | null = null;

// reader hotkeys must not fire while writing a note
apNote.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Escape") closePopover();
});

function fmtWhen(a: AnnotRec): string {
  const d = new Date(a.created * 1000);
  const kind = a.kind === "region" ? " · region" : "";
  return (
    d.toLocaleDateString(undefined, { month: "short", day: "numeric" }).toLowerCase() + kind
  );
}

export function openPopover(id: string, scrollTo = false) {
  const a = annots.find((x) => x.id === id);
  if (!a) return;
  const pageEl = $pagesEl.children[a.page - 1] as HTMLElement | undefined;
  if (!pageEl) return;
  if (scrollTo) {
    const y = pageEl.offsetTop + markBoxes(a)[0][1] * pageEl.offsetHeight - $scroll.clientHeight * 0.3;
    $scroll.scrollTo({ top: Math.max(y, 0) });
  }
  popId = id;
  apWhen.textContent = fmtWhen(a);
  apNote.value = a.note;
  popover.hidden = false;
  // anchor beside the mark's first box, clamped to the reader
  const [bx, by] = markBoxes(a)[0];
  const pr = pageEl.getBoundingClientRect();
  const host = $reader.getBoundingClientRect();
  const left = Math.min(pr.left - host.left + bx * pr.width, host.width - popover.offsetWidth - 12);
  const top = Math.min(
    pr.top - host.top + by * pr.height + 18,
    host.height - popover.offsetHeight - 12,
  );
  popover.style.left = `${Math.max(8, left)}px`;
  popover.style.top = `${Math.max(8, top)}px`;
  apNote.focus();
}

async function closePopover() {
  if (popover.hidden || !popId) {
    popover.hidden = true;
    return;
  }
  const a = annots.find((x) => x.id === popId);
  popover.hidden = true;
  popId = null;
  if (!a || apNote.value === a.note) return;
  a.note = apNote.value;
  try {
    await saveAnnotation(a);
    changed();
  } catch (e) {
    notify(`couldn't save note: ${e instanceof Error ? e.message : e}`);
  }
}

apDelete.addEventListener("click", async () => {
  if (!popId) return;
  const id = popId;
  popId = null;
  popover.hidden = true;
  try {
    await deleteAnnotation(currentDoc, id);
    annots = annots.filter((a) => a.id !== id);
    changed();
  } catch (e) {
    notify(`couldn't remove mark: ${e instanceof Error ? e.message : e}`);
  }
});

export async function removeAnnotation(id: string) {
  await deleteAnnotation(currentDoc, id);
  annots = annots.filter((a) => a.id !== id);
  changed();
}

$pagesEl.addEventListener("click", (e) => {
  const mark = (e.target as HTMLElement).closest<HTMLElement>(".alayer .mark");
  if (mark?.dataset.annot) {
    e.preventDefault();
    openPopover(mark.dataset.annot);
  } else if (!popover.contains(e.target as Node)) {
    void closePopover();
  }
});

document.addEventListener("keydown", (e) => {
  if ($reader.hidden) return;
  if (e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLInputElement) return;
  if (e.key === "Escape" && (regionMode || !popover.hidden || !toolbar.hidden)) {
    endRegionMode();
    void closePopover();
    hideToolbar();
    e.stopImmediatePropagation(); // don't also close the reader
    return;
  }
  if (e.key === "r") {
    startRegionMode();
    e.preventDefault();
  } else if (e.key === "m") {
    toggleMarks();
    e.preventDefault();
  }
});
