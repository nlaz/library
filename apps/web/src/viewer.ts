// ---------------------------------------------------------------------------
// page viewer overlay (opened from a result card) + the search popover /
// in-book find bar: Cmd+F opens it (right side, clear of the results),
// Escape closes it — and only it. In the library views query text and
// results survive a close; in the reader, dismissing the find bar clears
// the filter too, browser-find style.
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
import { onHitStep, readerOpen, stepHit } from "./reader";
import { sendQuery } from "./search";
import type { WireHit } from "./types";

let viewerHit: WireHit | null = null;

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
  $q.focus();
  $q.select(); // retyping replaces, browser-find style
}

function closeSearchPop() {
  $searchPop.hidden = true;
}

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
      // in the reader the query is a find filter — dismissing the bar
      // clears the highlights/ticks through the normal empty-query path
      // (which also seq-guards any answer still in flight)
      if (readerOpen() && $q.value) {
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
