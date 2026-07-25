// Notes: a third top-level surface beside the shelves and the reader
// (#/notes). One flat reverse-chronological timeline — a journal of
// reading — with day rules as the only grouping. The selected card opens
// in place: full body, marks, links, and the related-but-unlinked rail
// folded in beneath it.

import { composerOpen, openComposer } from "./composer";
import { cardNeighbors, listCards, updateCard } from "./marginalia-api";
import { backlinks, fmtStamp, timeline, wikiTokens } from "./notebox-model";
import { notify } from "./toast";
import type { CardRec } from "./types";

const $notes = document.getElementById("notes")!;
const $back = document.getElementById("notes-back")!;
const $title = document.getElementById("notes-title")!;
const $newCard = document.getElementById("notes-new")!;
const $body = document.getElementById("notes-body")!;

let cards: CardRec[] = [];
let selected: string | null = null;

export function notesOpen(): boolean {
  return !$notes.hidden;
}

/** Route entry: #/notes, optionally ?card=<id> to land selected. */
export async function openNotes(cardId: string | null) {
  $notes.hidden = false;
  if (cardId) selected = cardId;
  await reload();
  if (cardId) {
    document
      .querySelector(`.ncard[data-id="${CSS.escape(cardId)}"]`)
      ?.scrollIntoView({ block: "center" });
  }
}

export function closeNotes() {
  $notes.hidden = true;
}

async function reload() {
  try {
    cards = await listCards();
  } catch {
    cards = [];
  }
  render();
}

function render() {
  $title.textContent = "notes";
  const { live, filed } = timeline(cards);
  if (selected && !cards.some((c) => c.id === selected)) selected = null;

  const list = document.createElement("div");
  list.className = "nb-timeline";
  if (!live.length && !filed.length) {
    const empty = document.createElement("div");
    empty.className = "nb-empty";
    empty.textContent = "no notes yet — press c, or mark up a page in the reader (⌘U)";
    list.append(empty);
  }
  let day = "";
  for (const c of live) {
    const d = new Date(c.created * 1000).toDateString();
    if (d !== day) {
      day = d;
      const rule = document.createElement("div");
      rule.className = "nb-day";
      rule.textContent = fmtStamp(c.created);
      list.append(rule);
    }
    list.append(cardEl(c));
  }
  if (filed.length) {
    const rule = document.createElement("div");
    rule.className = "nb-filedrule";
    rule.textContent = `filed away · ${filed.length}`;
    list.append(rule);
    for (const c of filed) list.append(cardEl(c));
  }
  $body.replaceChildren(list);
}

// ---------------------------------------------------------------------------
// the rail: what the box suggests — near-but-unlinked cards, shown
// inside the selected card
// ---------------------------------------------------------------------------

let railToken = 0;

function railEl(me: CardRec): HTMLElement {
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

function cardEl(c: CardRec): HTMLElement {
  const el = document.createElement("div");
  el.className = "ncard";
  el.dataset.id = c.id;
  const active = c.id === selected;
  if (active) el.classList.add("active");
  if (c.filed) el.classList.add("filed");

  const when = document.createElement("span");
  when.className = "ncard-when";
  when.textContent = fmtWhen(c.created);
  const title = document.createElement("div");
  title.className = "ncard-title";
  title.textContent = c.title;
  el.append(when, title);

  if (c.body) {
    const body = document.createElement("div");
    body.className = "ncard-body";
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
    el.append(body);
  }

  for (const q of c.evidence) {
    const ev = document.createElement("button");
    ev.className = "ncard-ev";
    ev.textContent =
      q.kind === "text"
        ? `“${trunc(q.text, 90)}” — ${q.doc} · p.${q.page}`
        : `region — ${q.doc} · p.${q.page}`;
    ev.addEventListener("click", (e) => {
      e.stopPropagation();
      location.hash = `#/read/${q.doc}?p=${q.page}`;
    });
    el.append(ev);
  }

  const back = backlinks(cards, c);
  const outs = c.links
    .map((l) => cards.find((x) => x.id === l.to))
    .filter((x): x is CardRec => !!x);
  if (outs.length || back.length) {
    const foot = document.createElement("div");
    foot.className = "ncard-links";
    for (const o of outs) foot.append(linkChip("↔", o));
    for (const b of back) foot.append(linkChip("←", b));
    el.append(foot);
  }

  // the open card carries the suggestion rail
  if (active && !c.filed) el.append(railEl(c));

  el.addEventListener("click", () => {
    selected = active ? null : c.id;
    render();
  });
  el.addEventListener("dblclick", () => editCard(c));
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

/** Time within the day rule: `5:31 pm`; older years fall back to the date. */
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

// ---------------------------------------------------------------------------
// births + edits
// ---------------------------------------------------------------------------

function selectedCard(): CardRec | null {
  return cards.find((c) => c.id === selected) ?? null;
}

function newCard() {
  openComposer({ kind: "create", seed: {} }, (saved) => {
    if (!saved) return;
    selected = saved.id;
    void reload();
  });
}

function editCard(c: CardRec) {
  openComposer({ kind: "edit", card: c }, (saved) => {
    if (saved) void reload();
  });
}

$newCard.addEventListener("click", newCard);
$back.addEventListener("click", () => {
  location.hash = "#/";
});
document.getElementById("notes-toggle")!.addEventListener("click", () => {
  location.hash = notesOpen() ? "#/" : "#/notes";
});

document.addEventListener("keydown", (e) => {
  if ($notes.hidden || composerOpen()) return;
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
  switch (e.key) {
    case "Escape":
      location.hash = "#/";
      break;
    case "c":
      newCard();
      e.preventDefault();
      break;
    case "j":
    case "k": {
      const { live } = timeline(cards);
      if (!live.length) return;
      const i = live.findIndex((c) => c.id === selected);
      const next = live[Math.min(Math.max(i + (e.key === "j" ? 1 : -1), 0), live.length - 1)];
      selected = next.id;
      render();
      document
        .querySelector(`.ncard[data-id="${CSS.escape(next.id)}"]`)
        ?.scrollIntoView({ block: "nearest" });
      e.preventDefault();
      break;
    }
    case "Enter": {
      const c = selectedCard();
      if (c) {
        editCard(c);
        e.preventDefault();
      }
      break;
    }
  }
});
