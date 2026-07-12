// Continuous-scroll page reader. Pages are lazy in BOTH directions: an
// IntersectionObserver with a generous margin sets img.src coming into range
// and clears it going out, so a 296-page scan never holds more than a couple
// dozen 1600px JPEGs in the webview at once. Each visible page also gets a
// transparent OCR text layer so scanned text can be selected and copied.

import { ocrUrl, pageImg } from "./assets";
import type { OcrWord } from "./types";

const $reader = document.getElementById("reader")!;
const $scroll = document.getElementById("reader-scroll")!;
const $pagesEl = document.getElementById("reader-pages")!;
const $title = document.getElementById("reader-title")!;
const $pageNo = document.getElementById("reader-pageno")!;
const $back = document.getElementById("reader-back")!;

let currentDoc = "";
let loader: IntersectionObserver | null = null;
let tracker: IntersectionObserver | null = null;
/** Per-page OCR words for the current doc; null = fetch failed, don't retry. */
const ocrCache = new Map<number, OcrWord[] | null>();

export function readerOpen(): boolean {
  return !$reader.hidden;
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
            img.src = pageImg(doc, Number(el.dataset.page));
            img.addEventListener("load", () => {
              // real aspect ratio, so placeholder heights stop drifting
              el.style.aspectRatio = `${img.naturalWidth} / ${img.naturalHeight}`;
              fitTextLayer(el);
            });
            el.append(img);
            attachTextLayer(el, doc, Number(el.dataset.page));
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
// the printed words. One layout pass per page, once the true size is known.
function fitTextLayer(el: HTMLElement) {
  const spans = el.querySelectorAll<HTMLElement>(".tw");
  for (const s of spans) {
    s.style.transform = "";
    const k = s.offsetWidth / (s.scrollWidth || 1);
    if (k < 1 || k > 1.15) {
      s.style.transform = `scaleX(${k})`;
      s.style.transformOrigin = "0 0";
    }
  }
}

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
