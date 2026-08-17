// ---------------------------------------------------------------------------
// The card catalog (⌘K): one box over everything, for reaching any book,
// shelf, note or command by name.
//
// It is not a second search box, and the distinction is the whole design.
// ⌘F searches *inside* the books — page text and figures, through the engine,
// streamed. This searches the furniture: the names of things and of verbs,
// matched locally against lists the app already holds. A query it cannot
// answer is handed to ⌘F rather than dead-ending in an empty list.
//
// Two rules hold the key layering together and both are load-bearing:
//
//   1. ⌘K is checked before every other chord (main.ts, window capture), so
//      it opens from inside a text field and from over the perf view, which
//      owns the top of the key stack everywhere else.
//   2. Escape closes the catalog and nothing else. It never unwinds a layer
//      underneath, and whatever had focus gets it back.
//
// See docs/command-palette.md for why, and for the surfaces still to come.
// ---------------------------------------------------------------------------

import { commands, go } from "./commands";
import { $q as $searchBox } from "./dom";
import { displayTitle, docTitle, getDocList, STAGE_LABEL, stageCount, statusEvent } from "./format";
import { ingesting } from "./home";
import { listCards } from "./marginalia-api";
import { forgetDoc } from "./nav";
import { fmtStamp, impliedTitle } from "./notebox-model";
import {
  crumbs,
  flatten,
  GROUP_CAP,
  type Item,
  rank,
  type Row,
  type Span,
  type Stage,
  step,
} from "./palette-model";
import { sendQuery } from "./search";
import { desktop, transport } from "./state";
import { notify } from "./toast";
import type { CardRec, Collections, DocInfo } from "./types";
import { openSearchPop } from "./viewer";

const $palette = document.getElementById("palette")!;
const $card = document.getElementById("pal-card")!;
const $palQ = document.getElementById("pal-q") as HTMLInputElement;
const $list = document.getElementById("pal-list")!;
const $crumb = document.getElementById("pal-crumb")!;

const PLACEHOLDER = "Find a book or note…";

/** The desktop module, once it is known to be there. */
type Host = NonNullable<typeof desktop>;

type Opts = {
  /** Apply a collection filter, exactly as the header tabs do. Passed in
   * rather than imported: putting every surface that scopes by collection
   * back in agreement is main.ts's job, and the catalog must not grow a
   * second opinion about it. */
  chooseCol: (col: string) => void;
  /** A book was renamed, reshelved or removed — the same repaint the drawer
   * asks for after the same edits, and the same function. */
  onDocChanged: (doc: string) => void | Promise<void>;
};

let opts: Opts | null = null;

export function initPalette(o: Opts) {
  opts = o;
}

/** The flattened rows, in the order the arrow keys walk them. */
let rows: Row[] = [];
let sel = 0;
/** What had focus when the catalog opened, so closing can give it back. */
let restore: HTMLElement | null = null;
/** Read once per open. Both are small in-memory lists on the host side, and
 * a catalog that is one book stale is worse than one that refetches. */
let cols: Collections = {};
let cards: CardRec[] = [];
/** The drill-down. Empty = the root catalog. */
let stack: Stage[] = [];

export function paletteOpen(): boolean {
  return !$palette.hidden;
}

export function togglePalette() {
  if (paletteOpen()) closePalette();
  else openPalette();
}

export function openPalette() {
  if (paletteOpen()) return;
  restore = document.activeElement as HTMLElement | null;
  $palette.hidden = false;
  $palQ.value = "";
  render();
  $palQ.focus();
  void refresh();
}

export function closePalette() {
  if (!paletteOpen()) return;
  $palette.hidden = true;
  $palQ.value = "";
  $palQ.placeholder = PLACEHOLDER;
  $palQ.removeAttribute("aria-activedescendant");
  $list.replaceChildren();
  rows = [];
  sel = 0;
  stack = [];
  // The caret goes back where it was: opening the catalog over the search
  // box and closing it again must not cost the user their place in it.
  restore?.focus();
  restore = null;
}

/** Pull the two lists the catalog does not already hold. Failures are
 * swallowed to what they cost: a catalog missing its shelves is still a
 * working command list, and an error toast over a box the user opened half a
 * second ago helps nobody. */
async function refresh() {
  const [c, k] = await Promise.all([
    transport.collections().catch(() => ({}) as Collections),
    listCards().catch(() => [] as CardRec[]),
  ]);
  cols = c;
  cards = k;
  if (paletteOpen()) render();
}

// --- the four drawers -------------------------------------------------------

/** What a book row says on the right: where it is shelved and how long it is,
 * or what is still happening to it. */
function bookMeta(d: DocInfo): string {
  const shelf = d.collections[0] ?? "";
  const s = d.status;
  const state =
    s && s.state !== "ready" && s.state !== "text_ready"
      ? (STAGE_LABEL[s.stage ?? s.state] ?? s.state)
      : d.pages
        ? `${d.pages}p`
        : "";
  return [shelf, state].filter(Boolean).join(" · ");
}

function bookItems(): Item[] {
  const docs = getDocList();
  if (docs.length) {
    return docs
      .filter((d) => d.status?.state !== "deleted")
      .map((d) => ({
        id: `doc:${d.id}`,
        label: displayTitle(d),
        group: "books",
        meta: bookMeta(d),
        run: () => openDoc(d.id),
        drill: () => bookStage(d.id),
      }));
  }
  // The web build has no `docs` command, so the shelves are the only census
  // of the library it can take; docTitle falls back to the prettified id.
  const ids = new Set<string>();
  for (const list of Object.values(cols)) for (const id of list) ids.add(id);
  return [...ids].map((id) => ({
    id: `doc:${id}`,
    label: docTitle(id),
    group: "books",
    run: () => openDoc(id),
    drill: () => bookStage(id),
  }));
}

function shelfItems(): Item[] {
  return Object.entries(cols).map(([name, ids]) => ({
    id: `shelf:${name}`,
    label: name,
    group: "shelves",
    meta: `${ids.length} book${ids.length === 1 ? "" : "s"}`,
    run: () => {
      go("#/"); // a shelf filter is a library view; the reader can't show one
      opts?.chooseCol(name);
    },
  }));
}

function noteItems(): Item[] {
  return cards
    .filter((c) => !c.filed)
    .map((c) => ({
      id: `card:${c.id}`,
      label: impliedTitle(c.title, c.body),
      group: "notes",
      meta: fmtStamp(c.modified),
      run: () => go(`#/notes?card=${encodeURIComponent(c.id)}`),
    }));
}

function cmdItems(): Item[] {
  return commands().map((c) => ({
    id: c.id,
    label: c.label,
    group: c.group,
    chord: c.chord,
    run: c.run,
  }));
}

const openDoc = (doc: string) => go(`#/read/${encodeURIComponent(doc)}`);
const report = (e: unknown) => notify(String(e), { sticky: true });

/** What the library is still chewing on, and what it gave up on.
 *
 * Ingest state exists per book but is only visible if you happen to be looking
 * at the right shelf card, which means a failed book can sit unnoticed
 * forever. An empty catalog is the one place the library gets to say so
 * unprompted — and, for a failure, the only place it can be retried without
 * first going to find it.
 *
 * Offered to an empty box only: once there is a name to match, the book's own
 * row carries the same state in its metadata. */
function attentionItems(): Item[] {
  const stuck: Item[] = [];
  const busy: Item[] = [];
  for (const d of getDocList()) {
    const state = d.status?.state;
    if (state === "failed") {
      stuck.push({
        id: `fix:${d.id}`,
        label: displayTitle(d),
        group: "needs attention",
        meta: desktop ? "failed · retry" : "failed",
        run: desktop ? () => void retry(d.id) : undefined,
      });
    } else if (state === "queued" || state === "preparing" || state === "staged") {
      busy.push({
        id: `busy:${d.id}`,
        label: displayTitle(d),
        group: "indexing",
        meta: progress(d),
        run: () => openDoc(d.id),
      });
    }
  }
  return [...stuck, ...busy];
}

/** The live event if the worker is talking to us, the persisted status if it
 * is not — a book mid-ingest at launch has the second and not the first. */
function progress(d: DocInfo): string {
  const e = ingesting.get(d.id) ?? statusEvent(d);
  if (!e) return "";
  return (STAGE_LABEL[e.stage] ?? e.stage) + stageCount(e.stage, e.done, e.total);
}

async function retry(doc: string) {
  if (!desktop) return;
  try {
    await desktop.retryDoc(doc);
    await opts?.onDocChanged(doc);
  } catch (e) {
    report(e);
  }
}

// --- the drill-down ---------------------------------------------------------

/** Everything that can be done to one book, reached with ⇥ from its row. */
function bookStage(doc: string): Stage {
  return {
    crumb: docTitle(doc),
    placeholder: "a verb…",
    items: () => bookVerbs(doc),
  };
}

function bookVerbs(doc: string): Item[] {
  const group = "this book";
  const out: Item[] = [
    { id: "v:open", label: "Open in the reader", group, chord: "⏎", run: () => openDoc(doc) },
  ];
  if (!desktop) return out; // the browser build has no file to rename or move
  const d = desktop;
  // a book still on its way in is cancelled rather than deleted — the same
  // removal (gone from the shelf, file kept), by the command that knows how
  // to stop a worker mid-pass instead of refusing while one is running
  const processing = getDocList().find((x) => x.id === doc)?.processing ?? false;
  out.push(
    { id: "v:rename", label: "Rename…", group, drill: () => renameStage(d, doc) },
    { id: "v:file", label: "File into a collection…", group, drill: () => shelfStage(d, doc) },
    {
      id: "v:reveal",
      label: "Show in Finder",
      group,
      run: () => void d.revealDoc(doc).catch(report),
    },
    processing
      ? { id: "v:cancel", label: "Cancel indexing…", group, drill: () => removeStage(d, doc, true) }
      : {
          id: "v:delete",
          label: "Remove from the library…",
          group,
          drill: () => removeStage(d, doc),
        },
  );
  return out;
}

function renameStage(d: Host, doc: string): Stage {
  return {
    crumb: `rename ${docTitle(doc)}`,
    placeholder: docTitle(doc),
    raw: true,
    items: (q) => {
      const name = q.trim();
      if (!name) return [];
      return [
        {
          id: "rename:commit",
          label: `Rename to “${name}”`,
          group: "rename",
          chord: "⏎",
          run: async () => {
            try {
              await d.setTitle(doc, name);
              await opts?.onDocChanged(doc);
            } catch (e) {
              report(e);
            }
          },
        },
      ];
    },
  };
}

function shelfStage(d: Host, doc: string): Stage {
  const move = async (shelf: string) => {
    try {
      await d.moveToShelf(doc, shelf);
      await opts?.onDocChanged(doc);
    } catch (e) {
      report(e);
    }
  };
  return {
    crumb: `file ${docTitle(doc)}`,
    placeholder: "a shelf…",
    items: (q) => {
      const group = "shelves";
      const out: Item[] = Object.entries(cols).map(([shelf, ids]) => ({
        id: `file:${shelf}`,
        label: shelf,
        group,
        meta: `${ids.length} book${ids.length === 1 ? "" : "s"}`,
        run: () => void move(shelf),
      }));
      out.push({
        id: "file:root",
        label: "No shelf — the top of the library",
        group,
        run: () => void move(""),
      });
      // The only place the app offers to make a collection. It is a real
      // folder either way — moveToShelf creates it — but everywhere else you
      // have to already know that a shelf is a depth-1 directory to guess so.
      // The label carries the query, which is also what keeps this row past
      // the matcher when nothing else survives it.
      const name = q.trim();
      if (name && !(name in cols)) {
        out.push({
          id: "file:new",
          label: `New collection “${name}”`,
          group,
          run: () => void move(name),
        });
      }
      return out;
    },
  };
}

/** Destructive, so it is a stage rather than a row: the confirmation is a
 * place you have to walk into, and ⌫ walks back out of it. With `cancel`
 * it is the same removal offered to a book still indexing, through the
 * command that stops the worker instead of refusing while it runs. */
function removeStage(d: Host, doc: string, cancel = false): Stage {
  const title = docTitle(doc);
  return {
    crumb: `${cancel ? "cancel" : "remove"} ${title}`,
    placeholder: "",
    raw: true,
    items: () => [
      {
        id: "remove:confirm",
        label: cancel ? `Stop indexing “${title}”` : `Remove “${title}” from the library`,
        group: "confirm",
        meta: "the file stays on disk",
        chord: "⏎",
        run: async () => {
          try {
            await (cancel ? d.cancelDoc(doc) : d.deleteDoc(doc));
            localStorage.removeItem(`pos:${doc}`);
            forgetDoc(doc); // and it must not stay on anyone's way back
            if (location.hash.startsWith(`#/read/${encodeURIComponent(doc)}`)) location.hash = "#/";
            await opts?.onDocChanged(doc);
          } catch (e) {
            report(e);
          }
        },
      },
    ],
  };
}

/** Everything the catalog can offer right now.
 *
 * An empty query offers what needs a person and then the three verbs. It does
 * not offer books: a library of a few hundred has no meaningful "first six",
 * and opening onto an arbitrary slice of one is a census nobody asked for —
 * they arrive the moment there is a name to match them against.
 *
 * Order matters only as a tiebreak, since rank() sorts groups by their best
 * row. Commands lead, so that a verb and a book of the same name go to the
 * verb. */
function items(): Item[] {
  if (!$palQ.value.trim()) return [...attentionItems(), ...cmdItems()];
  return [...cmdItems(), ...bookItems(), ...shelfItems(), ...noteItems()];
}

// --- the way out ------------------------------------------------------------

/** Hand the query to the thing that can actually answer it. The catalog
 * matches names; this one reads the books. */
function searchFor(text: string) {
  go("#/"); // a doc-scoped find would answer only from the book in front of you
  openSearchPop();
  $searchBox.value = text;
  sendQuery();
}

/** What to offer when nothing in the library is called this. The catalog
 * never shows an empty list with nothing to do in it. */
function handoffs(text: string): Row[] {
  const item: Item = {
    id: "handoff:search",
    label: `Search the library for “${text}”`,
    group: "elsewhere",
    chord: "⏎",
    run: () => searchFor(text),
  };
  return [{ item, spans: [], score: 0 }];
}

function pushStage(s: Stage) {
  stack.push(s);
  $palQ.value = "";
  render();
  $palQ.focus();
}

function popStage() {
  stack.pop();
  $palQ.value = "";
  render();
  $palQ.focus();
}

function render() {
  const stage = stack.at(-1);
  const typed = $palQ.value.trim();

  // The trail earns its line only once there is one: at the root it named the
  // box you had just opened, which taught nobody anything.
  $crumb.hidden = !stack.length;
  $crumb.textContent = stack.length ? crumbs(stack) : "";
  $palQ.placeholder = stage?.placeholder ?? PLACEHOLDER;

  // A raw stage's rows are built *from* the query, so matching them against
  // it again would be marking a string for containing itself. Ranking against
  // nothing groups and orders them without filtering any away — and a stage
  // is a short, deliberate list, so the group cap comes off too.
  const pool = stage ? stage.items($palQ.value) : items();
  let sections = rank(pool, stage?.raw ? "" : $palQ.value, stage ? pool.length : GROUP_CAP);
  if (!stage && typed && !flatten(sections).length) {
    sections = [{ title: "elsewhere", rows: handoffs(typed) }];
  }

  // One flat list. The groups still order the rows — rank() sorts them by
  // their best match — but they no longer announce themselves: with the verbs
  // capped at three, a heading over each kind labelled lists of one and two,
  // and the rows already say what they are (a chord is a command, a page
  // count is a book, a date is a note).
  rows = flatten(sections);
  sel = 0;

  $list.replaceChildren(...rows.map((r, i) => rowEl(r, i)));
  paint();
}

/** The label with the matched runs marked, so it is visible *why* a row is
 * in the list — a fuzzy match with nothing highlighted reads as a bug. */
function labelEl(label: string, spans: Span[]): HTMLElement {
  const el = document.createElement("span");
  el.className = "pal-label";
  let at = 0;
  for (const [a, b] of spans) {
    if (a > at) el.append(label.slice(at, a));
    const m = document.createElement("mark");
    m.textContent = label.slice(a, b);
    el.append(m);
    at = b;
  }
  if (at < label.length) el.append(label.slice(at));
  return el;
}

function rowEl(r: Row, i: number): HTMLElement {
  const li = document.createElement("li");
  li.className = "pal-row";
  li.id = `pal-row-${i}`;
  li.setAttribute("role", "option");
  li.append(labelEl(r.item.label, r.spans));

  if (r.item.meta) {
    const m = document.createElement("span");
    m.className = "pal-meta";
    m.textContent = r.item.meta;
    li.append(m);
  }
  if (r.item.chord) {
    const k = document.createElement("kbd");
    k.textContent = r.item.chord;
    li.append(k);
  }
  if (r.item.drill) {
    const k = document.createElement("kbd");
    k.className = "pal-drill";
    k.textContent = "⇥";
    li.append(k);
  }

  // mousedown rather than click: a click fires after the input has already
  // lost focus, and preventDefault here keeps the caret in the box for the
  // rows that don't close the catalog
  li.addEventListener("mousedown", (e) => {
    e.preventDefault();
    activate(i);
  });
  li.addEventListener("mousemove", () => {
    if (sel === i) return;
    sel = i;
    paint();
  });
  return li;
}

function paint() {
  const els = $list.querySelectorAll<HTMLElement>(".pal-row");
  els.forEach((el, i) => {
    el.classList.toggle("on", i === sel);
    el.setAttribute("aria-selected", String(i === sel));
  });
  const cur = els[sel];
  if (cur) {
    $palQ.setAttribute("aria-activedescendant", cur.id);
    cur.scrollIntoView({ block: "nearest" });
  } else {
    $palQ.removeAttribute("aria-activedescendant");
  }
}

/** Enter, or a click. `viaDrill` is Tab: a row with both a verb and a stage
 * runs the verb on Enter and opens the stage on Tab, and a row with only a
 * stage opens it either way — nothing should ever be a dead keypress. */
function activate(i: number, viaDrill = false) {
  const r = rows[i];
  if (!r) return;
  const drill = r.item.drill;
  if (drill && (viaDrill || !r.item.run)) {
    pushStage(drill());
    return;
  }
  if (!r.item.run) return;
  // Close before running: a command that opens another layer must not have
  // the catalog painted over it, and one that navigates must not be holding
  // focus when the new surface reaches for it.
  const act = r.item.run;
  closePalette();
  void act();
}

$palQ.addEventListener("input", render);

$palQ.addEventListener("keydown", (e) => {
  // The reader's scroll keys and the ledger's j/k are document-level; nothing
  // typed in here may reach them — same pattern as #q and the book menus.
  // (Escape never gets here: the capture-phase listener below takes it first.)
  e.stopPropagation();
  switch (e.key) {
    case "ArrowDown":
      e.preventDefault();
      sel = step(rows.length, sel, 1);
      paint();
      break;
    case "ArrowUp":
      e.preventDefault();
      sel = step(rows.length, sel, -1);
      paint();
      break;
    case "Enter":
      e.preventDefault();
      activate(sel);
      break;
    case "Tab":
      if (!rows[sel]?.item.drill) break; // let focus move as it normally would
      e.preventDefault();
      activate(sel, true);
      break;
    case "Backspace":
      // only on an empty box, so it never eats a character someone is deleting
      if ($palQ.value || !stack.length) break;
      e.preventDefault();
      popStage();
      break;
  }
});

// ---------------------------------------------------------------------------
// The chord, on window + capture, and it must be the first listener there.
//
// Registration order is what gives it that, and it is not accidental:
// main.ts imports this module, so this line runs before main.ts's own
// window-capture listener is added, and window capture runs ahead of the
// document-capture handlers in viewer.ts and every bubble-phase handler
// elsewhere. The result is the one property the catalog needs — there is no
// state in which ⌘K is unavailable: not inside the search box, not in the
// reader with markup mode on, not over the perf view, which outranks
// everything else.
//
// Escape is here for the mirror reason. It closes the catalog and stops, so
// the layer it was opened over is still open when it goes; every other
// Escape branch in the app is downstream of this one.
// ---------------------------------------------------------------------------

window.addEventListener(
  "keydown",
  (e) => {
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === "k") {
      e.preventDefault();
      e.stopPropagation();
      togglePalette();
    } else if (e.key === "Escape" && paletteOpen()) {
      e.preventDefault();
      e.stopPropagation();
      // innermost layer first, the way every other Escape in this app works:
      // a drill-down backs out one level before the catalog itself closes
      if (stack.length) popStage();
      else closePalette();
    }
  },
  true,
);

// The header's ⌘K button: the same act as the chord, for a hand already on
// the mouse. It is also the only place the catalog is advertised at all.
document.getElementById("pal-open")!.addEventListener("click", togglePalette);
$palette.addEventListener("mousedown", (e) => {
  if (e.target === $palette) closePalette(); // the scrim, not the card
});
// A click that starts on the card must not reach the scrim handler above.
// It must also not cost the box its focus: the document-level `?` key steps
// aside for an input, and only for an input, so a click on the header that
// dropped focus to the body would leave it live under an open catalog.
$card.addEventListener("mousedown", (e) => {
  e.stopPropagation();
  if (e.target !== $palQ && !(e.target as HTMLElement).closest("button")) {
    e.preventDefault();
    $palQ.focus();
  }
});
