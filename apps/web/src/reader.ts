// Continuous-scroll page reader. Pages are lazy in BOTH directions: an
// IntersectionObserver with a generous margin sets img.src coming into range
// and clears it going out, so a 296-page scan never holds more than a couple
// dozen 1600px JPEGs in the webview at once. Each visible page also gets a
// transparent OCR text layer so scanned text can be selected and copied.

import { attachAnnotLayer, scheduleAnnotTicks, setAnnotationsDoc } from "./annotations";
import { ocrUrl, pageImg } from "./assets";
import { hlBoxes } from "./highlights";
import type { OcrWord, WireHit } from "./types";

const $reader = document.getElementById("reader")!;
const $scroll = document.getElementById("reader-scroll")!;
const $pagesEl = document.getElementById("reader-pages")!;
const $title = document.getElementById("reader-title")!;
const $pageNo = document.getElementById("reader-pageno")!;
const $back = document.getElementById("reader-back")!;
const $ticks = document.getElementById("reader-ticks")!;

let currentDoc = "";
let totalPages = 0;
let loader: IntersectionObserver | null = null;
let tracker: IntersectionObserver | null = null;
/** Per-page OCR words for the current doc; null = fetch failed, don't retry. */
const ocrCache = new Map<number, OcrWord[] | null>();

export function readerOpen(): boolean {
  return !$reader.hidden;
}

/** The open doc's id when the reader is visible — scopes find queries. */
export function readerDoc(): string {
  return $reader.hidden ? "" : currentDoc;
}

/** Open at `page`; when omitted, resume the last remembered position. */
export function openReader(doc: string, pages: number, page?: number, title?: string) {
  if (currentDoc !== doc) {
    buildPages(doc, pages);
    currentDoc = doc;
  }
  page ??= Number(localStorage.getItem(`pos:${doc}`)) || 1;
  $title.textContent = title ?? doc;
  $reader.hidden = false;
  const target = $pagesEl.children[Math.min(Math.max(page, 1), pages) - 1];
  // placeholder aspect ratios keep offsets stable enough to land on the page
  requestAnimationFrame(() => target?.scrollIntoView());
}

export function closeReader() {
  $reader.hidden = true;
}

function buildPages(doc: string, pages: number) {
  loader?.disconnect();
  tracker?.disconnect();
  ocrCache.clear();
  clearReaderHits(); // hits belong to the previous doc
  void setAnnotationsDoc(doc); // marks load async and decorate as pages render
  totalPages = pages;

  const els: HTMLElement[] = [];
  for (let p = 1; p <= pages; p++) {
    const el = document.createElement("div");
    el.className = "rpage";
    el.dataset.page = String(p);
    els.push(el);
  }
  $pagesEl.replaceChildren(...els);
  $pageNo.textContent = `p. 1 / ${pages}`;

  loader = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        const el = e.target as HTMLElement;
        if (e.isIntersecting) {
          if (!el.firstElementChild) {
            const img = document.createElement("img");
            img.decoding = "async"; // keep JPEG decode off the scroll path
            img.src = pageImg(doc, Number(el.dataset.page));
            img.addEventListener("load", () => {
              // real aspect ratio, so placeholder heights stop drifting
              el.style.aspectRatio = `${img.naturalWidth} / ${img.naturalHeight}`;
              fitTextLayer(el);
              scheduleTicks(); // page heights shifted — tick offsets moved
              scheduleAnnotTicks();
            });
            el.append(img);
            attachTextLayer(el, doc, Number(el.dataset.page));
            attachHitLayer(el);
            attachAnnotLayer(el);
          }
        } else {
          el.replaceChildren(); // out of range: give the memory back
        }
      }
    },
    { root: $scroll, rootMargin: "300% 0px 300% 0px" },
  );
  tracker = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) {
          const page = (e.target as HTMLElement).dataset.page!;
          $pageNo.textContent = `p. ${page} / ${pages}`;
          localStorage.setItem(`pos:${doc}`, page);
        }
      }
    },
    { root: $scroll, rootMargin: "-45% 0px -45% 0px" },
  );
  for (const el of els) {
    loader.observe(el);
    tracker.observe(el);
  }
}

// ---------------------------------------------------------------------------
// text layer: transparent positioned spans over the page image, so scanned
// text selects and copies like real text (same idea as PDF.js)
// ---------------------------------------------------------------------------

async function fetchWords(doc: string, page: number): Promise<OcrWord[] | null> {
  if (ocrCache.has(page)) return ocrCache.get(page)!;
  let words: OcrWord[] | null = null;
  try {
    const res = await fetch(ocrUrl(doc, page));
    if (res.ok) words = (await res.json()).words;
  } catch {
    // no OCR for this page (or protocol hiccup) — reader still works
  }
  ocrCache.set(page, words);
  return words;
}

async function attachTextLayer(el: HTMLElement, doc: string, page: number) {
  const words = await fetchWords(doc, page);
  // the leave branch may have unloaded (or the doc changed) mid-fetch;
  // appending now would orphan the layer on an empty placeholder
  if (!words?.length || currentDoc !== doc || !el.firstElementChild) return;
  if (el.querySelector(".tlayer")) return;

  const layer = document.createElement("div");
  layer.className = "tlayer";
  for (const w of words) {
    const s = document.createElement("span");
    s.className = "tw";
    s.textContent = w.t + " "; // trailing space so copied text keeps word gaps
    s.style.left = `${w.x * 100}%`;
    s.style.top = `${w.y * 100}%`;
    s.style.width = `${w.w * 100}%`;
    s.style.height = `${w.h * 100}%`;
    s.style.fontSize = `${w.h * 100}cqh`;
    layer.append(s);
  }
  el.append(layer);
  const img = el.querySelector("img");
  if (img?.complete) fitTextLayer(el);
}

// Squeeze each span's glyphs into its box so selection targets line up with
// the printed words. Reads and writes are batched into separate passes: a
// dense catalog page carries ~2400 spans, and interleaving a measure with a
// style write forces a full reflow per span — seconds of jank per page.
// Batched, it's one reflow (transform writes are compositor-only).
function fitTextLayer(el: HTMLElement) {
  const spans = el.querySelectorAll<HTMLElement>(".tw");
  for (const s of spans) s.style.transform = "";
  const ks = new Float64Array(spans.length);
  spans.forEach((s, i) => {
    ks[i] = s.offsetWidth / (s.scrollWidth || 1);
  });
  spans.forEach((s, i) => {
    const k = ks[i];
    if (k < 1 || k > 1.15) {
      s.style.transform = `scaleX(${k})`;
      s.style.transformOrigin = "0 0";
    }
  });
}

// ---------------------------------------------------------------------------
// find-in-document: hits from a doc-scoped search render as scrollbar tick
// marks + word-box highlights on the pages, and Enter steps through them
// ---------------------------------------------------------------------------

/** Blend weights for find-hit stepping order (must sum to 1): "next" is not
 * strictly the best match nor strictly the next one down the page, but a
 * weighted compromise between relevance rank and document position. */
const STEP_RELEVANCE_WEIGHT = 0.6;
const STEP_POSITION_WEIGHT = 0.4;

let hits: WireHit[] = [];
let stepOrder: number[] = []; // hit indices in blended stepping order
let cur = -1; // position in stepOrder; -1 = not stepping yet
let hitsByPage = new Map<number, { hit: number; boxes: [number, number, number, number][] }[]>();
let stepListener: ((i: number, n: number) => void) | null = null;

/** Called whenever stepping lands on a hit (Enter, buttons, tick click). */
export function onHitStep(cb: (i: number, n: number) => void) {
  stepListener = cb;
}

const clamp01 = (v: number) => Math.min(Math.max(v, 0), 1);

/** Fraction of the way through the document, by page + y on the page. */
function docPos(h: WireHit): number {
  return (h.page - 1 + clamp01(h.crop[1])) / Math.max(totalPages, 1);
}

export function setReaderHits(hs: WireHit[]): number {
  hits = hs;
  hitsByPage = new Map();
  hs.forEach((h, i) => {
    // semantic hits can match without any lexical word boxes — highlight
    // the whole chunk region instead of nothing
    const boxes = h.boxes.length ? h.boxes : [h.crop];
    const arr = hitsByPage.get(h.page) ?? [];
    arr.push({ hit: i, boxes });
    hitsByPage.set(h.page, arr);
  });
  // response order IS relevance rank; normalize it against list length
  const denom = Math.max(hs.length - 1, 1);
  const weight = new Map(
    hs.map((h, i) => [i, STEP_RELEVANCE_WEIGHT * (i / denom) + STEP_POSITION_WEIGHT * docPos(h)]),
  );
  stepOrder = hs.map((_, i) => i).sort((a, b) => weight.get(a)! - weight.get(b)!);
  cur = -1;
  for (const el of $pagesEl.children) attachHitLayer(el as HTMLElement);
  layoutTicks();
  return hits.length;
}

export function clearReaderHits() {
  hits = [];
  stepOrder = [];
  cur = -1;
  hitsByPage = new Map();
  $ticks.replaceChildren();
  for (const l of $pagesEl.querySelectorAll(".hlayer")) l.remove();
}

/** Advance through the blended order (wrapping) and scroll to the hit. */
export function stepHit(dir: 1 | -1) {
  const n = stepOrder.length;
  if (!n) return;
  cur = cur < 0 ? (dir > 0 ? 0 : n - 1) : (cur + dir + n) % n;
  gotoHit(stepOrder[cur]);
  stepListener?.(cur, n);
}

function gotoHit(i: number) {
  const h = hits[i];
  const el = $pagesEl.children[h.page - 1] as HTMLElement | undefined;
  if (!el) return;
  // placeholders always exist, so this works for unloaded pages too: the
  // scroll brings the page into the loader's range and the highlight layer
  // attaches when the image does
  const y = el.offsetTop + clamp01(h.crop[1]) * el.offsetHeight - $scroll.clientHeight * 0.3;
  $scroll.scrollTo({ top: Math.max(y, 0) });
  updateCur();
}

/** Re-decorate the active hit everywhere it's drawn (pages + ticks). */
function updateCur() {
  const active = cur >= 0 ? String(stepOrder[cur]) : "";
  for (const b of $pagesEl.querySelectorAll<HTMLElement>(".hlayer .hl")) {
    b.classList.toggle("cur", b.dataset.hit === active);
  }
  for (const t of $ticks.children) {
    t.classList.toggle("cur", (t as HTMLElement).dataset.hit === active);
  }
}

/** Word-box highlights for one page, attached at image-load time (the same
 * lifecycle as the OCR text layer — the unload branch discards both). */
function attachHitLayer(el: HTMLElement) {
  el.querySelector(".hlayer")?.remove();
  if (!el.firstElementChild) return; // unloaded placeholder
  const entries = hitsByPage.get(Number(el.dataset.page));
  if (!entries) return;
  const layer = document.createElement("div");
  layer.className = "hlayer";
  const active = cur >= 0 ? stepOrder[cur] : -1;
  for (const en of entries) {
    const divs = hlBoxes(en.boxes);
    for (const d of divs) {
      d.dataset.hit = String(en.hit);
      if (en.hit === active) d.classList.add("cur");
    }
    layer.append(...divs);
  }
  el.append(layer);
}

/** One tick per hit along the scrollbar. Positions must come from content
 * offsets, not page-count fractions — page aspect ratios vary, and they
 * refine as images load (scheduleTicks re-runs this on those loads). */
function layoutTicks() {
  $ticks.replaceChildren();
  if (!hits.length) return;
  const sh = $scroll.scrollHeight;
  const th = $ticks.clientHeight;
  if (!sh || !th) return;
  const active = cur >= 0 ? stepOrder[cur] : -1;
  hits.forEach((h, i) => {
    const el = $pagesEl.children[h.page - 1] as HTMLElement | undefined;
    if (!el) return;
    const t = document.createElement("div");
    t.className = "tick";
    t.dataset.hit = String(i);
    if (i === active) t.classList.add("cur");
    t.style.top = `${((el.offsetTop + clamp01(h.crop[1]) * el.offsetHeight) / sh) * th}px`;
    t.addEventListener("click", () => {
      cur = stepOrder.indexOf(i);
      gotoHit(i);
      stepListener?.(cur, stepOrder.length);
    });
    $ticks.append(t);
  });
}

let tickRaf = 0;
function scheduleTicks() {
  if (tickRaf || !hits.length) return;
  tickRaf = requestAnimationFrame(() => {
    tickRaf = 0;
    layoutTicks();
  });
}
window.addEventListener("resize", scheduleTicks);

$back.addEventListener("click", () => {
  location.hash = "#/";
});

document.addEventListener("keydown", (e) => {
  if ($reader.hidden) return;
  const vh = $scroll.clientHeight;
  switch (e.key) {
    case "Escape":
      location.hash = "#/";
      break;
    case "ArrowDown":
    case "PageDown":
    case " ":
      $scroll.scrollBy({ top: vh * 0.85 });
      e.preventDefault();
      break;
    case "ArrowUp":
    case "PageUp":
      $scroll.scrollBy({ top: -vh * 0.85 });
      e.preventDefault();
      break;
    case "Home":
      $scroll.scrollTo({ top: 0 });
      e.preventDefault();
      break;
    case "End":
      $scroll.scrollTo({ top: $scroll.scrollHeight });
      e.preventDefault();
      break;
  }
});
