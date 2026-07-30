// ---------------------------------------------------------------------------
// search core: query state + single-flight dispatch, result cards, the
// render/append paths, infinite-scroll paging, and the inline completion.
// All query/paging state lives here — main.ts only calls initSearch() and
// sendQuery().
// ---------------------------------------------------------------------------

import { pageUrl } from "./assets";
import {
  $home,
  $main,
  $more,
  $q,
  $qTail,
  $qTyped,
  $results,
  $search,
  $searchCount,
  $stats,
} from "./dom";
import { docTitle, hitKey } from "./format";
import { getCol, renderHome } from "./home";
import { hlBoxes } from "./highlights";
import { notesOpen } from "./notebox";
import { clearReaderHits, readerDoc, readerOpen, setReaderHits } from "./reader";
import { completionPrefix, ghostTail } from "./search-ghost";
import { DEFAULT_KIND, type Kind, nextKind } from "./search-kinds";
import { transport } from "./state";
import type { WireHit, WireResponse } from "./types";
import { openViewer } from "./viewer";

const PREVIEW_H = 190; // px, keep in sync with .preview height in style.css
// one page of the blended result order; keep in sync with K in
// library-server/src/main.rs and library-app/src/lib.rs
const PAGE = 20;

let seq = 0;
let rendered = 0; // highest seq drawn; anything older is stale
let pendingSeq = 0; // seq of the in-flight keystroke query; 0 = none
let pendingDirty = false; // input/state changed while a query was in flight
// dev-only: seq -> performance.now() at send, for round-trip logging
const sentAt = new Map<number, number>();
// seqs sent doc-scoped (reader find) and to which doc — their responses
// feed the reader's hit ticks, never the library results grid (exported so
// route() can drop the old scope's in-flight finds on navigation)
export const sentDoc = new Map<number, string>();
// infinite scroll: continuation seqs and what they were for, so a late
// page-N can be dropped if the query/col/kind/pagination state moved on
const sentMore = new Map<number, { q: string; col: string; kind: Kind; offset: number }>();
let moreOffset = 0; // next offset to request = blended hits consumed so far
let endReached = false;
// cross-page dedup: each continuation is its own fjall snapshot, so a
// mid-ingest index change can drift the slices slightly
const seen = new Set<string>();
// send() closes over initSearch()'s state; route() (in main.ts) needs it too
export let sendQuery: () => void = () => {};
/** Drop any painted completion — the popover opening or closing (viewer.ts).
 * Escape cannot do this from a $q listener: the document-capture handler
 * that ends the search calls stopImmediatePropagation() first. */
export let resetGhost: () => void = () => {};

/** Keys that take the suggestion. Tab is the affordance; → and End are free,
 * because the ghost only paints with the caret already at the end, where
 * both would otherwise do nothing. */
const ACCEPT_KEYS = ["Tab", "ArrowRight", "End"];

// ---------------------------------------------------------------------------
// result kinds: which face of the library the query answers from. Cmd+F
// steps the cycle and nothing labels it, so every session starts from the
// same place — everything — and the filter is reached by pressing again,
// not inherited from whatever the last session was left on. The stats line
// ("N hits · phase · ms") is the only tell: "hybrid+img" for everything,
// "hybrid" for text/notes, "img" for figures.
// ---------------------------------------------------------------------------

let kind: Kind = DEFAULT_KIND;

/** Cmd+F on an already-open popover: step to the next kind. */
export function cycleKind() {
  setKind(nextKind(kind));
}

/** Opening the popover: back to everything, so the next press is figures. */
export function resetKind() {
  setKind(DEFAULT_KIND);
}

function setKind(next: Kind) {
  if (next === kind) return;
  kind = next;
  // the grid is answering the old kind — re-ask (the empty-query and shelf
  // paths need nothing: they show no hits to be wrong about)
  if ($q.value.trim()) sendQuery();
}

// ---------------------------------------------------------------------------
// search results (unchanged card/viewer logic, URLs via pageUrl)
// ---------------------------------------------------------------------------

/** Note hits have no page scan — a text card face (title/snippet)
 * replaces the crop preview. The click goes to the notes timeline; a card
 * anchored to a page (a mark) also offers the marked page itself. */
function marginaliaCard(hit: WireHit): { el: HTMLElement; place: () => void } {
  const el = document.createElement("div");
  el.className = "card notehit";

  const face = document.createElement("div");
  face.className = "preview note-face";
  if (hit.card) {
    const title = document.createElement("div");
    title.className = "nf-title";
    title.textContent = hit.card.title;
    face.append(title);
  }
  const body = document.createElement("div");
  body.className = "nf-snip";
  for (const w of hit.snippet) {
    const node = w.m ? document.createElement("mark") : document.createElement("span");
    node.textContent = w.t;
    body.append(node, " ");
  }
  face.append(body);

  // the hit is the destination — end the search session first, or the
  // still-live query would bounce the target view straight back to the
  // grid (showSearch leaves the box whenever results render)
  const settle = () => {
    $q.value = "";
    $q.dispatchEvent(new Event("input", { bubbles: true }));
  };

  const loc = document.createElement("div");
  loc.className = "loc";
  loc.textContent = hit.card?.doc ? `note · ${docTitle(hit.card.doc)}` : "note";
  const meta = document.createElement("div");
  meta.className = "meta";
  meta.append(loc);
  // a mark-card carries its page: a second affordance jumps into the
  // reader instead of the notebox
  const c = hit.card;
  if (c?.doc && c.page != null) {
    const jump = document.createElement("button");
    jump.className = "nf-jump";
    jump.textContent = `${docTitle(c.doc)} · p.${c.page} ↗`;
    jump.addEventListener("click", (e) => {
      e.stopPropagation();
      settle();
      location.hash = `#/read/${c.doc}?p=${c.page}`;
    });
    meta.append(jump);
  }
  el.append(face, meta);

  el.addEventListener("click", () => {
    settle();
    if (hit.card) location.hash = `#/notes?card=${hit.card.id}`;
  });
  return { el, place: () => {} };
}

function card(hit: WireHit): { el: HTMLElement; place: () => void } {
  if (hit.kind === "card") return marginaliaCard(hit);

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
  // an image card's crop IS the figure — a brass wash over the whole
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

  // only the answer to the box's current text commits to the DOM. During a
  // fast burst a superseded answer lands every engine round-trip (the dirty
  // catch-up is already in flight), and rebuilding 20 cards per landing
  // starves the input of main-thread time — the box visibly stalls. Skipped
  // renders cost nothing: the settled answer arrives one round-trip after
  // the last keystroke and repaints everything.
  if (msg.seq < seq) return;

  $stats.textContent = `${msg.hits.length} hits · ${msg.phase} · ${(msg.us / 1000).toFixed(1)}ms`;

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

  if (msg.hits.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "no matches";
    $results.replaceChildren(empty);
  }

  // a replace render restarts pagination from this page-1
  seen.clear();
  for (const h of msg.hits) seen.add(hitKey(h));
  moreOffset = msg.hits.length;
  endReached = msg.hits.length < PAGE;
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

// ---------------------------------------------------------------------------
// mode switching: home shelves when the query is empty, results otherwise
// ---------------------------------------------------------------------------

export function showSearch(active: boolean) {
  // a library-wide query while the note box is open means "show me the
  // results" — leave the box by navigation, the way the reader is left
  // (the box sits over the grid and would swallow every click)
  if (active && notesOpen()) location.hash = "#/";
  $search.hidden = !active;
  $home.hidden = active;
}

/** Wire query dispatch, the response handler, infinite scroll, and the
 * inline completion. Call once at startup, after the transport is ready. */
export function initSearch() {
  const send = () => {
    const q = $q.value.trim();
    // reader open = the query is a find scoped to that doc; the library
    // views behind the reader must not churn
    const doc = readerDoc();
    if (!q) {
      rendered = ++seq;
      pendingSeq = 0;
      pendingDirty = false;
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
      pendingDirty = false;
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
    // at most one query in flight: a burst of keystrokes must not stack
    // concurrent hybrid scans on the engine — mark dirty and catch up when
    // the pending answer lands. Self-pacing replaces the old debounce: the
    // engine answers well under a keystroke interval, so every send is a
    // full (hybrid+image) query.
    if (pendingSeq > rendered) {
      pendingDirty = true;
      return;
    }
    pendingSeq = seq + 1;
    if (!doc) $stats.textContent = "searching…";
    if (import.meta.env.DEV) sentAt.set(seq + 1, performance.now());
    if (doc) sentDoc.set(seq + 1, doc);
    // doc-scoped find has exactly one kind of hit — lexical page text — so
    // it ignores the cycle rather than answering nothing in "figures"
    transport.send({ seq: ++seq, q, mode: "full", col: getCol(), kind: doc ? "" : kind, doc });
  };
  sendQuery = send;

  // infinite scroll continuation — deliberately NOT routed through send():
  // it must not touch the single-flight machinery
  const sendMore = () => {
    const q = $q.value.trim();
    if (readerOpen() || $search.hidden || endReached || q.length < 2) return;
    // settled-gate: only continue when nothing else is in flight, so a
    // continuation can never race an unanswered query
    if (seq !== rendered) return;
    if (sentMore.size) return; // one continuation at a time
    setMoreState("loading");
    sentMore.set(seq + 1, { q, col: getCol(), kind, offset: moreOffset });
    if (import.meta.env.DEV) sentAt.set(seq + 1, performance.now());
    transport.send({ seq: ++seq, q, mode: "full", col: getCol(), kind, doc: "", offset: moreOffset });
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
    if (msg.seq === pendingSeq) {
      pendingSeq = 0;
      if (pendingDirty) {
        pendingDirty = false;
        send(); // the box (or collection) changed while we were waiting
      }
    }
    const hitDoc = sentDoc.get(msg.seq);
    sentDoc.delete(msg.seq);
    const more = sentMore.get(msg.seq);
    sentMore.delete(msg.seq);
    if (msg.seq < rendered) return; // superseded while in flight
    rendered = msg.seq;
    if (hitDoc) {
      // reader find: hits become ticks/highlights, never result cards —
      // and, as with the grid, only the settled answer is applied
      if (readerDoc() === hitDoc && msg.seq >= seq) {
        const n = setReaderHits(msg.hits);
        $searchCount.textContent = n ? `${n} hits` : "no matches";
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
        more.col !== getCol() ||
        more.kind !== kind ||
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

  $q.addEventListener("input", () => send()); // hybrid, every keystroke

  // --- ghost text: the likeliest continuation of the word being typed ------
  // Additive and independent of the search grid + seq machinery. Stale
  // responses are dropped by a monotonic token (there is no request to
  // abort), and the candidate list is *kept* between keystrokes so typing on
  // inside one word repaints from cache instead of round-tripping.
  let ghostToken = 0;
  let ghostCands: string[] = [];
  let ghostTimer: ReturnType<typeof setTimeout>;
  let composing = false;

  const paintGhost = () => {
    const end = $q.selectionEnd;
    const tail = ghostTail({
      value: $q.value,
      candidates: ghostCands,
      // a null selection means the engine does not track one, in which case
      // it cannot observe mid-string editing either — treat it as at-end and
      // degrade to "always on" rather than silently never showing
      caretAtEnd: end === null || (end === $q.value.length && $q.selectionStart === end),
      composing,
      readerFind: !!readerDoc(),
      // the input scrolls its own text; the overlay does not, so a value
      // wider than the field would draw the tail in the wrong place.
      // scrollWidth can equal clientWidth in WebKit regardless of overflow —
      // with the caret pinned at the end, scrollLeft catches what it misses
      overflowing: $q.scrollWidth > $q.clientWidth || $q.scrollLeft > 0,
    });
    // both spans, unconditionally: a "did the tail change" guard would be
    // wrong, since candidates ["abzz", "abczz"] give tail "zz" for both "ab"
    // and "abc" — the transparent head would go stale under an equal tail
    $qTyped.textContent = tail ? $q.value : "";
    $qTail.textContent = tail;
  };

  const clearGhost = () => {
    clearTimeout(ghostTimer);
    ghostCands = [];
    $qTyped.textContent = "";
    $qTail.textContent = "";
  };
  resetGhost = clearGhost;

  const fetchGhost = (prefix: string) => {
    const tok = ++ghostToken;
    transport
      .complete(prefix)
      .then((items) => {
        if (tok !== ghostToken) return; // a newer keystroke superseded this
        ghostCands = items; // kept verbatim; ghostTail does the filtering
        paintGhost();
      })
      .catch(() => {});
  };

  const armGhost = () => {
    clearTimeout(ghostTimer);
    const prefix = completionPrefix($q.value);
    // no completions for reader-find (the dictionary is library-wide), for a
    // word too short to complete, or mid-composition
    if (!prefix || composing || readerDoc()) return clearGhost();
    paintGhost(); // synchronous, from cache — the ghost never blinks
    ghostTimer = setTimeout(() => fetchGhost(prefix), 80);
  };

  $q.addEventListener("input", armGhost);
  // a caret move fires no input event, and element-level "selectionchange" is
  // too new for the WKWebView floor — repaint on what every engine does fire
  for (const ev of ["keyup", "mouseup", "select"] as const) {
    $q.addEventListener(ev, paintGhost);
  }
  $q.addEventListener("compositionstart", () => {
    composing = true;
    clearGhost();
  });
  $q.addEventListener("compositionend", () => {
    composing = false;
    armGhost();
  });
  $q.addEventListener("blur", clearGhost);
  $q.addEventListener("keydown", (e) => {
    if (!ACCEPT_KEYS.includes(e.key) || e.shiftKey || e.metaKey || e.ctrlKey || e.altKey) return;
    // what is on screen is what Tab takes
    const tail = $qTail.textContent ?? "";
    if (!tail) return; // nothing to accept: the key keeps its normal job
    e.preventDefault();
    $q.value += tail;
    clearGhost(); // accepting finishes the word; the next keystroke re-arms
    send(); // ...and the finished word is a new query
  });
}
