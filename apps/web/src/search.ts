// ---------------------------------------------------------------------------
// search core: query state + debounce, result cards, the render/append paths,
// infinite-scroll paging, and the type-ahead dropdown. All query/paging state
// lives here — main.ts only calls initSearch() and sendQuery().
// ---------------------------------------------------------------------------

import { pageUrl } from "./assets";
import { $ac, $home, $main, $more, $q, $results, $search, $searchCount, $stats } from "./dom";
import { docTitle, hitKey } from "./format";
import { getCol, renderHome } from "./home";
import { hlBoxes } from "./highlights";
import { clearReaderHits, readerDoc, readerOpen, setReaderHits } from "./reader";
import { transport } from "./state";
import type { WireHit, WireResponse } from "./types";
import { openViewer } from "./viewer";

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
// feed the reader's hit ticks, never the library results grid (exported so
// route() can drop the old scope's in-flight finds on navigation)
export const sentDoc = new Map<number, string>();
// infinite scroll: continuation seqs and what they were for, so a late
// page-N can be dropped if the query/col/pagination state moved on
const sentMore = new Map<number, { q: string; col: string; offset: number }>();
let moreOffset = 0; // next offset to request = blended hits consumed so far
let endReached = false;
// cross-page dedup: each continuation is its own fjall snapshot, so a
// mid-ingest index change can drift the slices slightly
const seen = new Set<string>();
// send() closes over initSearch()'s state; route() (in main.ts) needs it too
export let sendQuery: (mode: "instant" | "full") => void = () => {};

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

// ---------------------------------------------------------------------------
// mode switching: home shelves when the query is empty, results otherwise
// ---------------------------------------------------------------------------

export function showSearch(active: boolean) {
  $search.hidden = !active;
  $home.hidden = active;
}

/** Wire query dispatch, the response handler, infinite scroll, and the
 * type-ahead dropdown. Call once at startup, after the transport is ready. */
export function initSearch() {
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
    transport.send({ seq: ++seq, q, mode, col: getCol(), kind: "", doc });
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
    sentMore.set(seq + 1, { q, col: getCol(), offset: moreOffset });
    if (import.meta.env.DEV) sentAt.set(seq + 1, performance.now());
    transport.send({ seq: ++seq, q, mode: "full", col: getCol(), kind: "", doc: "", offset: moreOffset });
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
        more.col !== getCol() ||
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
}
