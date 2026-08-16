// ---------------------------------------------------------------------------
// page viewer overlay (opened from a result card) + the search popover /
// in-book find bar: Cmd+F opens it (right side, clear of the results) and
// every press after that steps the unlabelled result-kind cycle. Escape
// ends the search — it closes the popover and clears the query, which takes
// the library views back to the shelves and the reader back to an
// unfiltered page (browser-find style). It still unwinds exactly one layer
// per press.
// ---------------------------------------------------------------------------

import { pageUrl } from "./assets";
import {
  $overlay,
  $pageHl,
  $pageImg,
  $q,
  $searchCount,
  $searchNav,
  $searchNext,
  $searchPop,
  $searchPrev,
  $viewerClose,
  $viewerLabel,
  $viewerRead,
} from "./dom";
import { docTitle } from "./format";
import { hlBoxes } from "./highlights";
import { gridRetryDelay } from "./page-retry";
import { onHitStep, readerOpen, stepHit } from "./reader";
import { cycleKind, resetGhost, resetKind, sendQuery } from "./search";
import type { WireHit } from "./types";

let viewerHit: WireHit | null = null;

/** The page the overlay is showing, and how many times its render has been
 * asked for again. Same story as the results grid: the request can be shed
 * (library-app/src/render.rs), and the overlay used to sit blank when it
 * was — which is exactly where someone lands after clicking a card whose
 * preview didn't load. */
let viewerSrc = "";
let viewerAttempt = 0;

$pageImg.addEventListener("error", () => {
  const delay = gridRetryDelay(++viewerAttempt);
  if (delay === null || !viewerSrc) return;
  const src = viewerSrc;
  window.setTimeout(() => {
    // not if the overlay closed or moved on to another page in the meantime
    if (viewerSrc !== src || $overlay.hidden) return;
    $pageImg.src = `${src}?r=${viewerAttempt}`;
  }, delay);
});

export function openViewer(hit: WireHit) {
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
    viewerSrc = src;
    viewerAttempt = 0; // a fresh page gets the full budget of tries
    $pageImg.src = src;
    $pageImg.addEventListener("load", reveal, { once: true });
  }
  $overlay.hidden = false;
}

export function closeViewer() {
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

export function openSearchPop() {
  $searchPop.hidden = false;
  $searchNav.hidden = !readerOpen();
  resetKind(); // every session starts on everything; Cmd+F again narrows
  resetGhost(); // a completion painted for the old value must not survive
  $q.focus();
  $q.select(); // retyping replaces, browser-find style
}

function closeSearchPop() {
  $searchPop.hidden = true;
  resetGhost();
}

// capture phase, like the perf hotkey: opening the popover focuses $q, and
// $q swallows every keydown it sees (stopPropagation, so the reader's
// scroll keys can't fire while typing) — a bubble-phase listener here would
// therefore see the *first* Cmd+F and never another, leaving the cycle stuck
document.addEventListener(
  "keydown",
  (e) => {
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === "f") {
      e.preventDefault(); // the popover replaces native find in the web build
      e.stopPropagation();
      // the first press opens the bar on everything; each press after that
      // steps the cycle: everything → figures → text/notes.
      // The reader's find has only page text to offer, so it never cycles.
      if ($searchPop.hidden) {
        openSearchPop();
      } else if (!readerOpen()) {
        cycleKind();
        $q.focus();
      }
    }
  },
  true,
);

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
      // Escape ends the search session rather than just hiding the box: the
      // query clears and the empty-query path puts the library views back
      // on the shelves (and, in the reader, drops the find highlights and
      // ticks). It also seq-guards any answer still in flight.
      if ($q.value) {
        $q.value = "";
        sendQuery();
      }
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
