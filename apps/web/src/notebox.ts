// Notes: a third top-level surface beside the shelves and the reader
// (#/notes) — the ledger. One flat reverse-chronological journal of
// reading under the app's one header: entries hang in whitespace instead
// of boxes, a floating document rail filters by source (multi-select),
// and the header's collection tabs scope both. The selected entry opens
// in place: body, evidence, links, and the related-but-unlinked rail
// folded in beneath it.

import { ensureDocTitles } from "./doc-titles";
import { $main } from "./dom";
import { evidenceEl } from "./evidence-el";
import { docTitle } from "./format";
import { getCol } from "./home";
import { cardNeighbors, listCards, updateCard } from "./marginalia-api";
import {
  backlinks,
  cardShown,
  docCounts,
  evidenceDocs,
  fmtStamp,
  pruneSelection,
  timeline,
  wikiTokens,
} from "./notebox-model";
import { sheetOpen, startCreate, startEdit } from "./sheet";
import { transport } from "./state";
import { notify } from "./toast";
import type { CardRec, Collections } from "./types";

const $notes = document.getElementById("notes")!;
const $body = document.getElementById("notes-body")!;
const $toggle = document.getElementById("notes-toggle")!;

let cards: CardRec[] = [];
let selected: string | null = null;
let sel = new Set<string>(); // the rail's multi-select
let colMap: Collections = {};
let railHidden = localStorage.getItem("notes:rail") === "hidden";
/** The ledger as last rendered (filters applied) — what j/k walk over. */
let visibleLive: CardRec[] = [];

export function notesOpen(): boolean {
  return !$notes.hidden;
}

/** Route entry: #/notes, optionally ?card=<id> to land selected. */
export async function openNotes(cardId: string | null) {
  $notes.hidden = false;
  $main.hidden = true;
  $toggle.classList.add("on");
  if (cardId) selected = cardId;
  await reload();
  if (cardId) {
    document
      .querySelector(`.lentry[data-id="${CSS.escape(cardId)}"]`)
      ?.scrollIntoView({ block: "center" });
  }
}

export function closeNotes() {
  $notes.hidden = true;
  $main.hidden = false;
  $toggle.classList.remove("on");
}

/** The header's collection tabs re-scope the ledger without a refetch. */
export function rerenderNotes() {
  render();
}

async function reload() {
  try {
    cards = await listCards();
  } catch {
    cards = [];
  }
  try {
    colMap = await transport.collections();
  } catch {
    colMap = {};
  }
  await ensureDocTitles(cards.flatMap(evidenceDocs));
  render();
}

function render() {
  const col = getCol();
  const colDocs = col ? new Set(colMap[col] ?? []) : null;
  const { live, filed } = timeline(cards);
  const counts = docCounts(live);
  const railDocs = [...counts.keys()].filter((d) => !colDocs || colDocs.has(d));
  sel = pruneSelection(sel, new Set(railDocs));
  if (selected && !cards.some((c) => c.id === selected)) selected = null;

  // breadcrumbs belong to the content, not the chrome
  const crumbs = document.createElement("div");
  crumbs.className = "nb-crumbs";
  const back = document.createElement("button");
  back.className = "nb-crumb";
  back.innerHTML = `← library <span class="here">/ notes</span>`;
  back.addEventListener("click", () => {
    location.hash = "#/";
  });
  const filterToggle = document.createElement("button");
  filterToggle.className = "nb-crumb";
  filterToggle.textContent = railHidden ? "show filter" : "hide filter";
  filterToggle.setAttribute("aria-pressed", String(!railHidden));
  filterToggle.addEventListener("click", () => {
    railHidden = !railHidden;
    localStorage.setItem("notes:rail", railHidden ? "hidden" : "shown");
    render();
  });
  const newBtn = document.createElement("button");
  newBtn.className = "nb-new";
  newBtn.textContent = "+ note";
  newBtn.title = "New note (c)";
  newBtn.addEventListener("click", () => startCreate());
  const tools = document.createElement("div");
  tools.className = "nb-tools";
  tools.append(filterToggle, newBtn);
  crumbs.append(back, tools);

  const content = document.createElement("div");
  content.className = "nb-content";
  if (railHidden) content.classList.add("railless");
  else content.append(docRailEl(railDocs, counts));

  const ledger = document.createElement("div");
  ledger.className = "ledger";
  visibleLive = live.filter((c) => cardShown(c, colDocs, sel));

  if (!live.length && !filed.length) {
    const empty = document.createElement("div");
    empty.className = "nb-empty";
    empty.textContent = "no notes yet — press c, or mark up a page in the reader (⌘U)";
    ledger.append(empty);
  } else if (!visibleLive.length) {
    const none = document.createElement("div");
    none.className = "nb-empty";
    none.textContent = "nothing here — clear a filter";
    ledger.append(none);
  }

  let day = "";
  for (const c of visibleLive) {
    const d = new Date(c.created * 1000).toDateString();
    if (d !== day) {
      day = d;
      const rule = document.createElement("div");
      rule.className = "nb-day";
      rule.textContent = fmtStamp(c.created);
      ledger.append(rule);
    }
    ledger.append(entryEl(c));
  }

  const filedShown = filed.filter((c) => cardShown(c, colDocs, sel));
  if (filedShown.length) {
    const rule = document.createElement("div");
    rule.className = "nb-filedrule";
    rule.textContent = `filed away · ${filedShown.length}`;
    ledger.append(rule);
    for (const c of filedShown) ledger.append(entryEl(c));
  }

  content.append(ledger);
  $body.replaceChildren(crumbs, content);
}

// ---------------------------------------------------------------------------
// the document rail: a floating accompaniment, not a panel — quiet text,
// selection marked by the pen's settle-line, never a fill
// ---------------------------------------------------------------------------

function docRailEl(docs: string[], counts: Map<string, number>): HTMLElement {
  const rail = document.createElement("aside");
  rail.className = "docrail";
  rail.setAttribute("aria-label", "filter by document");

  const head = document.createElement("div");
  head.className = "rail-head";
  const label = document.createElement("span");
  label.className = "rail-label";
  label.textContent = "documents";
  head.append(label);
  if (sel.size) {
    const clear = document.createElement("button");
    clear.className = "rail-clear";
    clear.textContent = "clear ×";
    clear.addEventListener("click", () => {
      sel.clear();
      render();
    });
    head.append(clear);
  }
  rail.append(head);

  for (const d of docs) {
    const item = document.createElement("button");
    item.className = "docitem";
    if (sel.has(d)) item.classList.add("sel");
    item.setAttribute("aria-pressed", String(sel.has(d)));
    const t = document.createElement("span");
    t.className = "t";
    t.textContent = docTitle(d);
    const n = document.createElement("span");
    n.className = "n";
    n.textContent = String(counts.get(d) ?? 0);
    item.append(t, n);
    item.addEventListener("click", () => {
      if (sel.has(d)) sel.delete(d);
      else sel.add(d);
      render();
    });
    rail.append(item);
  }
  return rail;
}

// ---------------------------------------------------------------------------
// the related rail: near-but-unlinked cards, folded into the open entry
// ---------------------------------------------------------------------------

let railToken = 0;

function relatedEl(me: CardRec): HTMLElement {
  const box = document.createElement("div");
  box.className = "railbox";
  const lab = document.createElement("div");
  lab.className = "rail-lab";
  lab.textContent = "related · unlinked";
  const list = document.createElement("div");
  list.className = "rail-list";
  list.textContent = "…";
  box.append(lab, list);

  const token = ++railToken;
  cardNeighbors(me.id, 6)
    .then((ns) => {
      if (token !== railToken) return;
      list.replaceChildren();
      if (!ns.length) {
        list.textContent = "nothing near yet";
        return;
      }
      for (const n of ns) {
        const row = document.createElement("div");
        row.className = "rail-row";
        const t = document.createElement("span");
        t.className = "rail-title";
        t.textContent = n.title;
        t.addEventListener("click", () => {
          const c = cards.find((x) => x.id === n.id);
          if (c) jumpToCard(c);
        });
        const add = document.createElement("button");
        add.className = "rail-add";
        add.textContent = "link";
        add.addEventListener("click", () => void linkTo(n.id));
        row.append(t, add);
        list.append(row);
      }
    })
    .catch(() => {
      if (token === railToken) list.textContent = "";
    });
  return box;
}

/** One click in the rail = a relates-link from the active card. */
async function linkTo(neighborId: string) {
  const me = selectedCard();
  if (!me) return;
  try {
    await updateCard({ ...me, links: [...me.links, { to: neighborId, kind: "relates" }] });
    await reload();
  } catch (e) {
    notify(`couldn't link: ${e instanceof Error ? e.message : e}`);
  }
}

// ---------------------------------------------------------------------------
// one ledger entry: time in the margin, whitespace instead of a box
// ---------------------------------------------------------------------------

function entryEl(c: CardRec): HTMLElement {
  const el = document.createElement("div");
  el.className = "lentry";
  el.dataset.id = c.id;
  const active = c.id === selected;
  if (active) el.classList.add("active");
  if (c.filed) el.classList.add("filed");

  const when = document.createElement("div");
  when.className = "lwhen";
  when.textContent = fmtWhen(c.created);

  const content = document.createElement("div");
  content.className = "lcontent";
  const title = document.createElement("div");
  title.className = "ltitle";
  title.textContent = c.title;
  content.append(title);

  if (c.body) {
    const body = document.createElement("div");
    body.className = "lbody";
    for (const tok of wikiTokens(c.body)) {
      if (tok.kind === "text") {
        body.append(tok.text);
      } else {
        const a = document.createElement("span");
        a.className = "wl";
        a.textContent = tok.title;
        a.addEventListener("click", (e) => {
          e.stopPropagation();
          jumpToTitle(tok.title);
        });
        body.append(a);
      }
    }
    content.append(body);
  }

  for (const q of c.evidence) content.append(evidenceEl(q));

  const back = backlinks(cards, c);
  const outs = c.links
    .map((l) => cards.find((x) => x.id === l.to))
    .filter((x): x is CardRec => !!x);
  if (outs.length || back.length) {
    const foot = document.createElement("div");
    foot.className = "llinks";
    for (const o of outs) foot.append(linkChip("↔", o));
    for (const b of back) foot.append(linkChip("←", b));
    content.append(foot);
  }

  // the open entry carries the suggestion rail
  if (active && !c.filed) content.append(relatedEl(c));

  el.append(when, content);
  el.addEventListener("click", () => {
    selected = active ? null : c.id;
    render();
  });
  el.addEventListener("dblclick", () => startEdit(c.id));
  return el;
}

function linkChip(glyph: string, target: CardRec): HTMLElement {
  const s = document.createElement("span");
  s.className = "nlink";
  s.textContent = `${glyph} ${trunc(target.title, 40)}`;
  s.addEventListener("click", (e) => {
    e.stopPropagation();
    jumpToCard(target);
  });
  return s;
}

const trunc = (s: string, n: number) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);

/** Time within the day rule: `5:31 pm`. */
function fmtWhen(secs: number): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  return d
    .toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })
    .toLowerCase();
}

function jumpToCard(c: CardRec) {
  selected = c.id;
  location.hash = `#/notes?card=${c.id}`;
  // hash may already match (same card re-selected) — render regardless
  render();
}

function jumpToTitle(title: string) {
  const c = cards.find((x) => x.title === title);
  if (c) jumpToCard(c);
}

function selectedCard(): CardRec | null {
  return cards.find((c) => c.id === selected) ?? null;
}

// ---------------------------------------------------------------------------
// chrome + keys
// ---------------------------------------------------------------------------

$toggle.addEventListener("click", () => {
  location.hash = notesOpen() ? "#/" : "#/notes";
});

document.addEventListener("keydown", (e) => {
  if ($notes.hidden || sheetOpen()) return;
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
  switch (e.key) {
    case "Escape":
      location.hash = "#/";
      break;
    case "j":
    case "k": {
      // walk the ledger as filtered, not the raw card list
      if (!visibleLive.length) return;
      const i = visibleLive.findIndex((c) => c.id === selected);
      const next =
        visibleLive[Math.min(Math.max(i + (e.key === "j" ? 1 : -1), 0), visibleLive.length - 1)];
      selected = next.id;
      render();
      document
        .querySelector(`.lentry[data-id="${CSS.escape(next.id)}"]`)
        ?.scrollIntoView({ block: "nearest" });
      e.preventDefault();
      break;
    }
    case "Enter": {
      const c = selectedCard();
      if (c) {
        startEdit(c.id);
        e.preventDefault();
      }
      break;
    }
  }
});
