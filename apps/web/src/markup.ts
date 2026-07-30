// The pen, one verb: in markup mode (⌘U) a single drag makes a mark. The
// gesture starts as text selection when it lands on the OCR words and
// converts to a region when it escapes them (selection.ts owns that
// math); at mouseup the mark is pending until a note is typed — a mark
// IS a notebox card (title = the note, evidence = the anchor), so an
// empty note discards it and nothing persists. Marks render in a
// per-page .alayer with the same attach-at-image-load / discard-at-
// unload lifecycle as the find-hit layer. Persistence rides
// marginalia-api (Tauri or HTTP — both land in the same cards.json +
// search index).

import { createCard, listCards, updateCard } from "./marginalia-api";
import {
  type DragUpdate,
  SNAP_DIST,
  dragBox,
  medianLineHeight,
  nearestWord,
  negligible,
  selectionText,
  updateDrag,
} from "./selection";
import { notify } from "./toast";
import type { Box, CardRec, OcrWord, QuoteAnchor } from "./types";

const $reader = document.getElementById("reader")!;
const $scroll = document.getElementById("reader-scroll")!;
const $pagesEl = document.getElementById("reader-pages")!;
const $aticks = document.getElementById("reader-aticks")!;
const $pen = document.getElementById("reader-pen")!;

/** A card's anchor on the open doc — one page presence of one card. */
export type Mark = { card: CardRec; anchor: QuoteAnchor };

type WordSource = (page: number) => OcrWord[] | null | undefined;

let currentDoc = "";
let wordsFor: WordSource = () => undefined;
let cards: CardRec[] = [];
let marks: Mark[] = [];
let marksVisible = true;
let onChange: (() => void) | null = null;
let onEnter: ((doc: string) => void) | null = null;

/** Reader-side hook: fires after any create/edit/file-away so dependent
 * surfaces (drawer list, ticks) can refresh. */
export function onMarksChanged(cb: () => void) {
  onChange = cb;
}

/** Fires on entering markup mode — the drawer opens itself with it. */
export function onMarkupEnter(cb: (doc: string) => void) {
  onEnter = cb;
}

/** The open doc's marks, reading order (page, then top edge). */
export function docMarks(): Mark[] {
  return marks;
}

export function markupDoc(): string {
  return currentDoc;
}

function anchorBoxes(a: QuoteAnchor): Box[] {
  return a.kind === "text" ? a.boxes : [a.bbox];
}

function anchorY(a: QuoteAnchor): number {
  return anchorBoxes(a)[0]?.[1] ?? 0;
}

function rebuildMarks() {
  marks = cards
    .filter((c) => !c.filed)
    .flatMap((card) =>
      card.evidence.filter((a) => a.doc === currentDoc).map((anchor) => ({ card, anchor })),
    )
    .sort((a, b) => a.anchor.page - b.anchor.page || anchorY(a.anchor) - anchorY(b.anchor));
}

/** Load the box and (re)decorate whatever pages are rendered. `words`
 * hands the driver the reader's per-page OCR cache without a cycle. */
export async function setMarkupDoc(doc: string, words: WordSource) {
  currentDoc = doc;
  wordsFor = words;
  marks = [];
  redecorate();
  try {
    const loaded = await listCards();
    if (currentDoc !== doc) return; // doc changed mid-fetch
    cards = loaded;
    rebuildMarks();
    redecorate();
    onChange?.();
  } catch {
    // offline host or cold engine — the reader still reads
  }
}

function redecorate() {
  for (const el of $pagesEl.children) attachMarkLayer(el as HTMLElement);
  scheduleMarkTicks();
}

function changed() {
  rebuildMarks();
  redecorate();
  onChange?.();
}

// ---------------------------------------------------------------------------
// mark rendering: one .alayer per page, boxes from creation-time snapshots
// ---------------------------------------------------------------------------

/** Same contract as reader.ts attachHitLayer: call at image load and after
 * any mark change; unloading a page discards the layer with it. */
export function attachMarkLayer(el: HTMLElement) {
  el.querySelector(".alayer")?.remove();
  if (!el.firstElementChild) return; // unloaded placeholder
  const page = Number(el.dataset.page);
  const here = marks.filter((m) => m.anchor.page === page);
  if (!here.length) return;
  const layer = document.createElement("div");
  layer.className = "alayer";
  if (!marksVisible) layer.classList.add("off");
  for (const m of here) {
    for (const [x, y, w, h] of anchorBoxes(m.anchor)) {
      const b = document.createElement("div");
      b.className = m.anchor.kind === "region" ? "mark region" : "mark";
      b.dataset.card = m.card.id;
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
  $aticks.classList.toggle("off", !marksVisible);
}

// ---------------------------------------------------------------------------
// scroll-rail ticks: one square per mark, settle-line colored — the find
// rail's brass slivers stay ephemeral, these are the kept marks
// ---------------------------------------------------------------------------

function layoutMarkTicks() {
  $aticks.replaceChildren();
  if (!marks.length) return;
  const sh = $scroll.scrollHeight;
  const th = $aticks.clientHeight;
  if (!sh || !th) return;
  for (const m of marks) {
    const el = $pagesEl.children[m.anchor.page - 1] as HTMLElement | undefined;
    if (!el) continue;
    const t = document.createElement("div");
    t.className = "atick";
    t.title = m.card.title;
    t.style.top = `${((el.offsetTop + anchorY(m.anchor) * el.offsetHeight) / sh) * th}px`;
    t.addEventListener("click", () => openMarkPopover(m.card.id, true));
    $aticks.append(t);
  }
}

let atickRaf = 0;
/** rAF-coalesced relayout; page-image loads shift every offset below. */
export function scheduleMarkTicks() {
  if (atickRaf || !marks.length) return;
  atickRaf = requestAnimationFrame(() => {
    atickRaf = 0;
    layoutMarkTicks();
  });
}
window.addEventListener("resize", scheduleMarkTicks);

// ---------------------------------------------------------------------------
// markup mode: ⌘U (or the pen button) arms the gesture, Escape backs out
// ---------------------------------------------------------------------------

let markupMode = false;

function setMarkupMode(on: boolean) {
  if (markupMode === on) return;
  markupMode = on;
  $scroll.classList.toggle("markup-mode", on);
  // .active, not .on — markup mode is a document-wide mode, and the pen's
  // own rule keys off it. The attribute is for screen readers.
  $pen.classList.toggle("active", on);
  $pen.setAttribute("aria-pressed", String(on));
  if (on) {
    onEnter?.(currentDoc);
  } else {
    cancelGesture();
  }
}

$pen.addEventListener("click", () => setMarkupMode(!markupMode));

// ---------------------------------------------------------------------------
// the gesture: mousedown anchors, mousemove tracks text↔region, mouseup
// freezes a pending mark and asks for its note
// ---------------------------------------------------------------------------

/** Pointer travel below this is a click, not a drag. */
const CLICK_SLOP = 0.005;

type Drag = {
  pageEl: HTMLElement;
  page: number;
  words: OcrWord[] | null;
  anchor: { x: number; y: number; word: number | null };
  lineH: number;
  layer: HTMLElement;
  upd: DragUpdate | null;
  moved: boolean;
};
let drag: Drag | null = null;

const pageXY = (pageEl: HTMLElement, e: MouseEvent): [number, number] => {
  const r = pageEl.getBoundingClientRect();
  return [(e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height];
};

function cancelGesture() {
  drag?.layer.remove();
  drag = null;
  $scroll.classList.remove("gesture-text");
}

$pagesEl.addEventListener("mousedown", (e) => {
  if (!markupMode || e.button !== 0 || !popover.hidden) return;
  const pageEl = (e.target as HTMLElement).closest<HTMLElement>(".rpage");
  if (!pageEl?.firstElementChild) return;
  e.preventDefault(); // no native selection under the pen
  const page = Number(pageEl.dataset.page);
  const [x, y] = pageXY(pageEl, e);
  // undefined = OCR fetch still in flight: degrade this gesture to region
  const words = wordsFor(page) ?? null;
  const near = words ? nearestWord(words, x, y) : null;
  const layer = document.createElement("div");
  layer.className = "glayer";
  pageEl.append(layer);
  drag = {
    pageEl,
    page,
    words,
    anchor: { x, y, word: near && near.dist <= SNAP_DIST ? near.idx : null },
    lineH: medianLineHeight(words ?? []),
    layer,
    upd: null,
    moved: false,
  };
});

function renderPreview(d: Drag, upd: DragUpdate) {
  const boxes = upd.mode === "text" ? upd.boxes : [upd.bbox];
  d.layer.replaceChildren();
  for (const [x, y, w, h] of boxes) {
    const b = document.createElement("div");
    b.className = upd.mode === "region" ? "mark region" : "mark";
    b.style.left = `${x * 100}%`;
    b.style.top = `${y * 100}%`;
    b.style.width = `${w * 100}%`;
    b.style.height = `${h * 100}%`;
    d.layer.append(b);
  }
}

document.addEventListener("mousemove", (e) => {
  if (!drag) return;
  const [x, y] = pageXY(drag.pageEl, e);
  if (Math.hypot(x - drag.anchor.x, y - drag.anchor.y) > CLICK_SLOP) drag.moved = true;
  const prev = drag.upd?.mode ?? (drag.anchor.word != null ? "text" : "region");
  drag.upd = updateDrag(drag.words, drag.anchor, { x, y }, prev, drag.lineH);
  // the cursor is the mode indicator: I-beam over a text run, crosshair
  // the moment the gesture becomes a region
  $scroll.classList.toggle("gesture-text", drag.upd.mode === "text");
  renderPreview(drag, drag.upd);
});

document.addEventListener("mouseup", (e) => {
  if (!drag) return;
  const d = drag;
  drag = null;
  $scroll.classList.remove("gesture-text");
  // the loader may have unloaded the page mid-drag (layers go with it)
  if (!d.pageEl.isConnected || !d.pageEl.firstElementChild) {
    d.layer.remove();
    return;
  }
  const [x, y] = pageXY(d.pageEl, e);
  if (!d.moved) {
    // a click under the pen: open the mark it landed on, if any
    d.layer.remove();
    const hit = hitMark(d.page, x, y);
    if (hit) openMarkPopover(hit.card.id);
    else void closePopover();
    return;
  }
  const prev = d.upd?.mode ?? (d.anchor.word != null ? "text" : "region");
  const upd = updateDrag(d.words, d.anchor, { x, y }, prev, d.lineH);
  if (upd.mode === "region" && negligible(upd.bbox)) {
    d.layer.remove();
    return;
  }
  renderPreview(d, upd);
  for (const b of d.layer.children) b.classList.add("pending");
  const anchor: QuoteAnchor =
    upd.mode === "text"
      ? {
          doc: currentDoc,
          page: d.page,
          kind: "text",
          w0: upd.w0,
          w1: upd.w1,
          text: selectionText((d.words ?? []).slice(upd.w0, upd.w1)),
          boxes: upd.boxes,
        }
      : { doc: currentDoc, page: d.page, kind: "region", bbox: dragBox(d.anchor.x, d.anchor.y, x, y) };
  openPendingPopover(anchor, d.layer, d.pageEl);
});

function hitMark(page: number, x: number, y: number): Mark | undefined {
  if (!marksVisible) return undefined;
  return marks.find(
    (m) =>
      m.anchor.page === page &&
      anchorBoxes(m.anchor).some(
        ([bx, by, bw, bh]) => x >= bx && x <= bx + bw && y >= by && y <= by + bh,
      ),
  );
}

// ---------------------------------------------------------------------------
// the margin card: a pending mark asks for its note (the note IS the
// card); clicking an existing mark edits it
// ---------------------------------------------------------------------------

const popover = document.createElement("div");
popover.id = "mark-pop";
popover.hidden = true;
popover.innerHTML = `
  <div class="mp-head">
    <span class="mp-addr"></span>
    <span class="mp-actions">
      <button class="mp-more divot quiet" aria-haspopup="menu" aria-expanded="false" aria-label="mark actions">⋯</button>
      <div class="mp-menu" role="menu" hidden>
        <button class="mp-open divot quiet" role="menuitem">open in notes ↗</button>
        <button class="mp-file-away divot quiet" role="menuitem">file away</button>
      </div>
    </span>
  </div>
  <textarea class="mp-note" rows="3" placeholder="note…"></textarea>
  <div class="mp-foot">
    <button class="mp-cancel divot quiet">cancel</button>
    <button class="mp-comment divot raise accent">comment</button>
  </div>
`;
$reader.append(popover);
const mpHead = popover.querySelector<HTMLElement>(".mp-head")!;
const mpAddr = popover.querySelector<HTMLElement>(".mp-addr")!;
const mpActions = popover.querySelector<HTMLElement>(".mp-actions")!;
const mpMore = popover.querySelector<HTMLButtonElement>(".mp-more")!;
const mpMenu = popover.querySelector<HTMLElement>(".mp-menu")!;
const mpOpen = popover.querySelector<HTMLButtonElement>(".mp-open")!;
const mpFileAway = popover.querySelector<HTMLButtonElement>(".mp-file-away")!;
const mpNote = popover.querySelector<HTMLTextAreaElement>(".mp-note")!;
const mpFoot = popover.querySelector<HTMLElement>(".mp-foot")!;
const mpCancel = popover.querySelector<HTMLButtonElement>(".mp-cancel")!;
const mpComment = popover.querySelector<HTMLButtonElement>(".mp-comment")!;

type PopState =
  | { kind: "pending"; anchor: QuoteAnchor; layer: HTMLElement }
  | { kind: "edit"; cardId: string };
let pop: PopState | null = null;

/** The header holds only the date; both verbs wait behind ⋯ so the note
 * stays the whole surface and filing away takes a deliberate second click. */
function setMenu(open: boolean) {
  mpMenu.hidden = !open;
  mpMore.setAttribute("aria-expanded", String(open));
}

/** Anchor the popover beside a mark's first box, clamped to the reading
 * pane — the scroll pane's right edge, not the reader's, so it never
 * floats over an open drawer. */
function placePopover(pageEl: HTMLElement, box: Box) {
  const pr = pageEl.getBoundingClientRect();
  const host = $reader.getBoundingClientRect();
  const paneRight = $scroll.getBoundingClientRect().right - host.left;
  popover.hidden = false;
  const left = Math.min(
    pr.left - host.left + box[0] * pr.width,
    paneRight - popover.offsetWidth - 12,
  );
  const top = Math.min(
    pr.top - host.top + box[1] * pr.height + box[3] * pr.height + 8,
    host.height - popover.offsetHeight - 12,
  );
  popover.style.left = `${Math.max(8, left)}px`;
  popover.style.top = `${Math.max(8, top)}px`;
}

function fmtWhen(created: number): string {
  return new Date(created * 1000)
    .toLocaleDateString(undefined, { month: "short", day: "numeric" })
    .toLowerCase();
}

/** A new mark is only the note field and its two verbs — the highlight
 * above it is the header. */
function openPendingPopover(anchor: QuoteAnchor, layer: HTMLElement, pageEl: HTMLElement) {
  pop = { kind: "pending", anchor, layer };
  setMenu(false);
  mpHead.hidden = true;
  mpNote.value = "";
  mpFoot.hidden = false;
  mpComment.disabled = true;
  placePopover(pageEl, anchorBoxes(anchor)[0]);
  mpNote.focus();
}

export function openMarkPopover(cardId: string, scrollTo = false) {
  const m = marks.find((x) => x.card.id === cardId);
  if (!m) return;
  const pageEl = $pagesEl.children[m.anchor.page - 1] as HTMLElement | undefined;
  if (!pageEl) return;
  if (scrollTo) {
    const y =
      pageEl.offsetTop + anchorY(m.anchor) * pageEl.offsetHeight - $scroll.clientHeight * 0.3;
    $scroll.scrollTo({ top: Math.max(y, 0) });
  }
  pop = { kind: "edit", cardId };
  setMenu(false);
  mpHead.hidden = false;
  mpAddr.textContent = fmtWhen(m.card.created);
  mpActions.hidden = false;
  mpNote.value = m.card.title;
  mpFoot.hidden = true;
  placePopover(pageEl, anchorBoxes(m.anchor)[0]);
  mpNote.focus();
}

/** Commit a pending mark: the note becomes a card on the timeline. */
async function commitPending(state: Extract<PopState, { kind: "pending" }>) {
  const title = mpNote.value.trim();
  try {
    const saved = await createCard({
      title,
      body: "",
      evidence: [state.anchor],
      links: [],
    });
    cards.push(saved);
    state.layer.remove();
    popover.hidden = true;
    pop = null;
    changed();
    // "noted", not "note filed" — "file" is reserved for the archive verb
    notify("noted");
  } catch (e) {
    pop = state; // keep the pending mark; the note is not lost
    popover.hidden = false;
    notify(`couldn't file mark: ${e instanceof Error ? e.message : e}`);
  }
}

async function saveEdit(state: Extract<PopState, { kind: "edit" }>) {
  const card = cards.find((c) => c.id === state.cardId);
  const title = mpNote.value.trim();
  if (!card || !title || title === card.title) return;
  try {
    const saved = await updateCard({ ...card, title });
    cards = cards.map((c) => (c.id === saved.id ? saved : c));
    changed();
  } catch (e) {
    notify(`couldn't save note: ${e instanceof Error ? e.message : e}`);
  }
}

/** Close commits: a pending mark with a note files, without one it
 * discards; an edited title saves. Escape is the only way out that
 * throws ink away (discardPopover). */
async function closePopover() {
  setMenu(false);
  if (popover.hidden || !pop) {
    popover.hidden = true;
    return;
  }
  const state = pop;
  if (state.kind === "pending") {
    if (mpNote.value.trim()) {
      await commitPending(state);
      return;
    }
    state.layer.remove();
  } else {
    void saveEdit(state);
  }
  pop = null;
  popover.hidden = true;
}

function discardPopover() {
  setMenu(false);
  if (pop?.kind === "pending") pop.layer.remove();
  pop = null;
  popover.hidden = true;
}

/** File the mark's card away: off the pages, out of search, address kept. */
export async function fileAwayMark(cardId: string) {
  const card = cards.find((c) => c.id === cardId);
  if (!card) return;
  const saved = await updateCard({ ...card, filed: true });
  cards = cards.map((c) => (c.id === saved.id ? saved : c));
  if (pop?.kind === "edit" && pop.cardId === cardId) {
    pop = null;
    popover.hidden = true;
  }
  changed();
}

mpMore.addEventListener("click", () => setMenu(mpMenu.hidden));

mpOpen.addEventListener("click", () => {
  const state = pop;
  if (state?.kind !== "edit") return;
  const card = cards.find((c) => c.id === state.cardId);
  if (!card) return;
  location.hash = `#/notes?card=${card.id}`;
});

mpFileAway.addEventListener("click", async () => {
  if (pop?.kind !== "edit") return;
  try {
    await fileAwayMark(pop.cardId);
  } catch (e) {
    notify(`couldn't file away: ${e instanceof Error ? e.message : e}`);
  }
});

// reader hotkeys must not fire while writing a note
mpNote.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Escape") {
    discardPopover();
  } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    // browsers do nothing on meta+Enter in a textarea — insert the
    // newline ourselves (shift+Enter keeps its native one)
    e.preventDefault();
    mpNote.setRangeText("\n", mpNote.selectionStart, mpNote.selectionEnd, "end");
    mpNote.dispatchEvent(new Event("input"));
  } else if (e.key === "Enter" && !e.shiftKey) {
    // Enter files, like sending a comment
    e.preventDefault();
    void closePopover();
  }
});
mpNote.addEventListener("input", () => {
  mpComment.disabled = !mpNote.value.trim();
});
// the foot buttons: mousedown keeps focus in the textarea so the blur
// handler below never races the click
for (const b of [mpCancel, mpComment]) {
  b.addEventListener("mousedown", (e) => e.preventDefault());
}
mpCancel.addEventListener("click", () => discardPopover());
mpComment.addEventListener("click", () => void closePopover());
// wherever focus goes, the note is not lost — blur commits (clicks that
// land outside the pages container never reach the close handler). The
// action buttons live inside the popover, so their mousedown must not
// re-enter close.
mpNote.addEventListener("blur", (e) => {
  if (e.relatedTarget instanceof Node && popover.contains(e.relatedTarget)) return;
  void closePopover();
});

// ---------------------------------------------------------------------------
// clicks outside markup mode still open marks. Hit-testing is geometric,
// not through the DOM: the OCR text layer attaches async above the mark
// layer, and raising marks above the text would break selecting inside a
// highlighted passage.
// ---------------------------------------------------------------------------

$pagesEl.addEventListener("click", (e) => {
  if (markupMode) return; // the gesture's mouseup owns pen clicks
  const sel = window.getSelection();
  if (sel && !sel.isCollapsed) return; // a selection drag, not a click
  const pageEl = (e.target as HTMLElement).closest<HTMLElement>(".rpage");
  if (!pageEl) {
    void closePopover();
    return;
  }
  const [x, y] = pageXY(pageEl, e);
  const hit = hitMark(Number(pageEl.dataset.page), x, y);
  if (hit) {
    e.preventDefault();
    openMarkPopover(hit.card.id);
  } else if (!popover.contains(e.target as Node)) {
    void closePopover();
  }
});

// NOTE: this listener must register before reader.ts's (import order does
// that — reader.ts imports this module), so the Escape branches here can
// stopImmediatePropagation before the reader's Escape closes the doc.
document.addEventListener("keydown", (e) => {
  if ($reader.hidden) return;
  if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "u") {
    setMarkupMode(!markupMode);
    e.preventDefault();
    return;
  }
  if (e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLInputElement) return;
  if (e.key === "Escape" && (drag || !popover.hidden || markupMode)) {
    if (drag) cancelGesture();
    else if (!popover.hidden) void closePopover();
    else setMarkupMode(false);
    e.stopImmediatePropagation(); // don't also close the reader
    return;
  }
  if (e.key === "m") {
    toggleMarks();
    e.preventDefault();
  }
});
