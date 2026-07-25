// The Note Box: a third top-level surface beside the shelves and the
// reader (#/notes). The box list is deliberately boring — threads by
// recency, a place you pass through — and the thread view is where cards
// read as an argument: address order, branch indentation, j/k walking.

import { composerOpen, openComposer } from "./composer";
import { cardNeighbors, listCards, updateCard } from "./marginalia-api";
import {
  backlinks,
  compareCards,
  displayAddr,
  fmtStamp,
  threads,
  wikiTokens,
} from "./notebox-model";
import { notify } from "./toast";
import type { CardRec } from "./types";

const $notes = document.getElementById("notes")!;
const $back = document.getElementById("notes-back")!;
const $title = document.getElementById("notes-title")!;
const $newCard = document.getElementById("notes-new")!;
const $body = document.getElementById("notes-body")!;

let cards: CardRec[] = [];
let openThread: number | null = null;
let selected: string | null = null;

export function notesOpen(): boolean {
  return !$notes.hidden;
}

/** Route entry: #/notes → box list; #/notes/<t>?card=<id> → thread view. */
export async function openNotes(thread: number | null, cardId: string | null) {
  $notes.hidden = false;
  openThread = thread;
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
  if (openThread == null) renderBox();
  else renderThread(openThread);
}

// ---------------------------------------------------------------------------
// box list: threads by recency
// ---------------------------------------------------------------------------

function renderBox() {
  $title.textContent = "note box";
  const rows = threads(cards);
  const list = document.createElement("div");
  list.className = "nb-drawer";
  if (!rows.length) {
    const empty = document.createElement("div");
    empty.className = "nb-empty";
    empty.textContent = "no cards yet — press c, or quote a passage in the reader";
    list.append(empty);
  }
  for (const r of rows) {
    const row = document.createElement("div");
    row.className = "nb-row";
    const no = document.createElement("span");
    no.className = "nb-no";
    no.textContent = String(r.thread);
    const name = document.createElement("span");
    name.className = "nb-name";
    name.textContent = r.name;
    const meta = document.createElement("span");
    meta.className = "nb-meta";
    meta.textContent =
      `${r.cards.length} card${r.cards.length === 1 ? "" : "s"}` +
      (r.filed ? ` · ${r.filed} filed` : "");
    const when = document.createElement("span");
    when.className = "nb-when";
    when.textContent = fmtStamp(r.lastTouched);
    row.append(no, name, meta, when);
    row.addEventListener("click", () => {
      location.hash = `#/notes/${r.thread}`;
    });
    list.append(row);
  }
  $body.replaceChildren(list);
}

// ---------------------------------------------------------------------------
// thread view: the argument, in address order
// ---------------------------------------------------------------------------

function renderThread(thread: number) {
  const all = cards.filter((c) => c.thread === thread).sort(compareCards);
  const live = all.filter((c) => !c.filed);
  const filed = all.filter((c) => c.filed);
  const trunk = all.find((c) => c.addr.length === 1);
  $title.textContent = `thread ${thread} · ${trunk?.title ?? ""}`;

  if (selected && !all.some((c) => c.id === selected)) selected = null;
  selected ??= live[0]?.id ?? null;

  const wrap = document.createElement("div");
  wrap.className = "nb-thread";
  for (const c of live) wrap.append(cardEl(c));
  if (filed.length) {
    const rule = document.createElement("div");
    rule.className = "nb-filedrule";
    rule.textContent = `filed · ${filed.length}`;
    wrap.append(rule);
    for (const c of filed) wrap.append(cardEl(c));
  }
  const grid = document.createElement("div");
  grid.className = "nb-threadwrap";
  grid.append(wrap, railEl());
  $body.replaceChildren(grid);
}

// ---------------------------------------------------------------------------
// the rail: what the box suggests — near-but-unlinked cards
// ---------------------------------------------------------------------------

let railToken = 0;

function railEl(): HTMLElement {
  const rail = document.createElement("aside");
  rail.id = "notes-rail";
  const me = selectedCard();
  if (!me || me.filed) return rail;

  const box = document.createElement("div");
  box.className = "railbox";
  const lab = document.createElement("div");
  lab.className = "rail-lab";
  lab.textContent = "near this card · unlinked";
  const list = document.createElement("div");
  list.className = "rail-list";
  list.textContent = "…";
  box.append(lab, list);
  rail.append(box);

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
        const a = document.createElement("span");
        a.className = "rail-addr";
        a.textContent = n.address;
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
        row.append(a, t, add);
        list.append(row);
      }
    })
    .catch(() => {
      if (token === railToken) list.textContent = "";
    });
  return rail;
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
  if (c.id === selected) el.classList.add("active");
  if (c.filed) el.classList.add("filed");
  el.style.marginLeft = `${Math.min(c.addr.length - 1, 4) * 22}px`;

  const addr = document.createElement("span");
  addr.className = "ncard-addr";
  addr.textContent = displayAddr(c.thread, c.addr);
  const title = document.createElement("div");
  title.className = "ncard-title";
  title.textContent = c.title;
  el.append(addr, title);

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
    ev.textContent = `“${trunc(q.text, 90)}” — ${q.doc} · p.${q.page}`;
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

  el.addEventListener("click", () => {
    selected = c.id;
    render();
  });
  el.addEventListener("dblclick", () => editCard(c));
  return el;
}

function linkChip(glyph: string, target: CardRec): HTMLElement {
  const s = document.createElement("span");
  s.className = "nlink";
  s.textContent = `${glyph} ${displayAddr(target.thread, target.addr)} ${trunc(target.title, 40)}`;
  s.addEventListener("click", (e) => {
    e.stopPropagation();
    jumpToCard(target);
  });
  return s;
}

const trunc = (s: string, n: number) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);

function jumpToCard(c: CardRec) {
  selected = c.id;
  location.hash = `#/notes/${c.thread}?card=${c.id}`;
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
  const parent = openThread != null ? selectedCard() : null;
  openComposer(
    {
      kind: "create",
      seed: parent
        ? {
            parent: {
              id: parent.id,
              address: displayAddr(parent.thread, parent.addr),
              title: parent.title,
            },
          }
        : { thread: openThread },
    },
    (saved) => {
      if (!saved) return;
      selected = saved.id;
      if (openThread == null) location.hash = `#/notes/${saved.thread}?card=${saved.id}`;
      else void reload();
    },
  );
}

function editCard(c: CardRec) {
  openComposer({ kind: "edit", card: c }, (saved) => {
    if (saved) void reload();
  });
}

$newCard.addEventListener("click", newCard);
$back.addEventListener("click", () => {
  location.hash = openThread == null ? "#/" : "#/notes";
});
document.getElementById("notes-toggle")!.addEventListener("click", () => {
  location.hash = notesOpen() ? "#/" : "#/notes";
});

document.addEventListener("keydown", (e) => {
  if ($notes.hidden || composerOpen()) return;
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
  switch (e.key) {
    case "Escape":
      location.hash = openThread == null ? "#/" : "#/notes";
      break;
    case "c":
      newCard();
      e.preventDefault();
      break;
    case "j":
    case "k": {
      if (openThread == null) return;
      const live = cards
        .filter((c) => c.thread === openThread && !c.filed)
        .sort(compareCards);
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
