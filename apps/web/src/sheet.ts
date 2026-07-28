// The sheet: writing a note is a full page — dateline, title, prose —
// with chrome only at the edges. Route-driven (#/notes/new,
// #/notes/edit?card=<id>) so it stands beside the surfaces it belongs to
// instead of floating over them; the modal composer is retired. Nothing
// is asked of the writer: the title is optional (the first line stands
// in), saving is automatic, esc walks back to the ledger.

import { ensureDocTitles } from "./doc-titles";
import { $main } from "./dom";
import { evidenceEl } from "./evidence-el";
import { createCard, listCards, updateCard } from "./marginalia-api";
import { impliedTitle, SPLIT_WORDS, splitPoint } from "./notebox-model";
import { notify } from "./toast";
import type { CardLink, CardRec, QuoteAnchor } from "./types";

export type SheetSeed = {
  body?: string;
  evidence?: QuoteAnchor[];
  links?: CardLink[];
};

const $sheet = document.getElementById("sheet")!;
const $back = document.getElementById("sheet-back")!;
const $state = document.getElementById("sheet-state")!;
const $date = document.getElementById("sheet-date")!;
const $title = document.getElementById("sheet-title") as HTMLInputElement;
const $body = document.getElementById("sheet-body") as HTMLTextAreaElement;
const $ac = document.getElementById("sheet-ac") as HTMLUListElement;
const $whisper = document.getElementById("sheet-whisper") as HTMLButtonElement;
const $evidence = document.getElementById("sheet-evidence")!;
const $file = document.getElementById("sheet-file") as HTMLButtonElement;
const $keep = document.getElementById("sheet-keep") as HTMLButtonElement;
const $toggle = document.getElementById("notes-toggle")!;

let card: CardRec | null = null; // the saved record; null until first save
let evidence: QuoteAnchor[] = [];
let links: CardLink[] = [];
let baseline = ""; // last-saved snapshot, for dirty checks
let pendingSeed: SheetSeed | null = null;
let titles: string[] = []; // live card titles for [[ completion
let saveTimer: ReturnType<typeof setTimeout> | null = null;
let opening = 0; // guards a stale open finishing after another began

export function sheetOpen(): boolean {
  return !$sheet.hidden;
}

/** Enter the sheet with a seed (split overflow, future quote flows). A
 * plain "+ note" passes nothing. */
export function startCreate(seed: SheetSeed = {}) {
  pendingSeed = seed;
  if (location.hash === "#/notes/new") {
    void openSheet("new", null); // already routed — re-open in place
  } else {
    location.hash = "#/notes/new";
  }
}

export function startEdit(id: string) {
  location.hash = `#/notes/edit?card=${id}`;
}

export async function openSheet(kind: "new" | "edit", cardId: string | null) {
  const token = ++opening;
  $sheet.hidden = false;
  $main.hidden = true;
  $toggle.classList.add("on");
  $ac.hidden = true;

  if (kind === "edit" && cardId) {
    let cs: CardRec[] = [];
    try {
      cs = await listCards();
    } catch {
      // offline — fall through to the not-found path
    }
    if (token !== opening) return;
    titles = cs.filter((c) => !c.filed && c.id !== cardId).map((c) => c.title);
    const found = cs.find((c) => c.id === cardId);
    if (!found) {
      notify("that note is gone");
      location.hash = "#/notes";
      return;
    }
    card = found;
    evidence = found.evidence;
    links = found.links;
    fill(found.title, found.body, found.created);
    setState("saved");
  } else {
    const s = pendingSeed ?? {};
    pendingSeed = null;
    card = null;
    evidence = s.evidence ?? [];
    links = s.links ?? [];
    fill("", s.body ?? "", Date.now() / 1000);
    setState("");
    listCards()
      .then((cs) => {
        if (token === opening) titles = cs.filter((c) => !c.filed).map((c) => c.title);
      })
      .catch(() => {
        titles = [];
      });
  }
  baseline = snap();
  updateChrome();
  void ensureDocTitles(evidence.map((q) => q.doc)).then(() => {
    if (token === opening) renderEvidence();
  });
  renderEvidence();
  $body.focus();
  $body.setSelectionRange($body.value.length, $body.value.length);
}

/** Route away: flush unsaved words on the way out, never block the nav. */
export function closeSheet() {
  if ($sheet.hidden) return;
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = null;
  void save();
  $sheet.hidden = true;
  $main.hidden = false;
  $toggle.classList.remove("on");
}

function fill(title: string, body: string, createdSecs: number) {
  $date.textContent = dateline(createdSecs);
  $title.value = title;
  $body.value = body;
  grow();
}

function dateline(secs: number): string {
  const d = new Date(secs * 1000);
  const day = d.toLocaleDateString(undefined, { weekday: "long" });
  const date = d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  const time = d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  return `${day} · ${date} · ${time}`.toLowerCase();
}

function renderEvidence() {
  $evidence.replaceChildren(...evidence.map(evidenceEl));
}

function snap(): string {
  return JSON.stringify([$title.value, $body.value]);
}

function setState(s: string) {
  $state.textContent = s;
}

function updateChrome() {
  $file.hidden = !card;
  $file.textContent = card?.filed ? "restore" : "file away";
  const over = ($body.value.match(/\S+/g) ?? []).length > SPLIT_WORDS;
  $whisper.hidden = !over || !!card?.split_hinted;
}

// ---------------------------------------------------------------------------
// saving: debounced while writing, flushed on every way out
// ---------------------------------------------------------------------------

/** Persist the current text; a no-op while there's nothing to say (an
 * all-empty sheet never births a blank card). */
async function save(extra?: { filed?: boolean; splitHinted?: boolean }): Promise<CardRec | null> {
  const title = impliedTitle($title.value, $body.value);
  if (!card && !title) return null;
  if (card && snap() === baseline && !extra) return card;
  try {
    const saved = card
      ? await updateCard({
          ...card,
          title: title || card.title,
          body: $body.value,
          filed: extra?.filed ?? card.filed,
          split_hinted: extra?.splitHinted ?? card.split_hinted,
        })
      : await createCard({ title, body: $body.value, evidence, links });
    card = saved;
    baseline = snap();
    setState("saved just now");
    updateChrome();
    return saved;
  } catch (e) {
    setState("couldn't save");
    notify(`couldn't save note: ${e instanceof Error ? e.message : e}`);
    return null;
  }
}

function scheduleSave() {
  setState(snap() === baseline ? (card ? "saved" : "") : "unsaved");
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    void save();
  }, 1200);
}

async function leave() {
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = null;
  const saved = await save();
  location.hash = saved ? `#/notes?card=${saved.id}` : "#/notes";
}

// ---------------------------------------------------------------------------
// the split whisper: past ~150 words an entry is becoming an essay
// ---------------------------------------------------------------------------

$whisper.addEventListener("click", async () => {
  const cut = splitPoint($body.value);
  if (cut == null) return;
  const rest = $body.value.slice(cut).trim();
  $body.value = $body.value.slice(0, cut).trimEnd();
  const saved = await save({ splitHinted: true });
  if (!saved) return;
  // the overflow becomes its own entry, linked as a continuation
  startCreate({ body: rest, links: [{ to: saved.id, kind: "continues" }] });
});

// ---------------------------------------------------------------------------
// [[ autocomplete over live card titles (the composer's, unchanged)
// ---------------------------------------------------------------------------

function acContext(): { start: number; prefix: string } | null {
  const pos = $body.selectionStart ?? 0;
  const before = $body.value.slice(0, pos);
  const open = before.lastIndexOf("[[");
  if (open < 0) return null;
  const frag = before.slice(open + 2);
  if (frag.includes("]]") || frag.includes("\n")) return null;
  return { start: open + 2, prefix: frag };
}

function updateAc() {
  const ctx = acContext();
  if (!ctx) {
    $ac.hidden = true;
    return;
  }
  const q = ctx.prefix.toLowerCase();
  const matches = titles.filter((t) => t.toLowerCase().includes(q)).slice(0, 6);
  if (!matches.length) {
    $ac.hidden = true;
    return;
  }
  $ac.replaceChildren(
    ...matches.map((t) => {
      const li = document.createElement("li");
      li.textContent = t;
      // mousedown, not click — the textarea must not lose the caret first
      li.addEventListener("mousedown", (e) => {
        e.preventDefault();
        applyAc(t);
      });
      return li;
    }),
  );
  $ac.hidden = false;
}

function applyAc(title: string) {
  const ctx = acContext();
  if (!ctx) return;
  const pos = $body.selectionStart ?? 0;
  const after = $body.value.slice(pos);
  const closed = after.startsWith("]]") ? after.slice(2) : after;
  $body.value = `${$body.value.slice(0, ctx.start)}${title}]]${closed}`;
  const caret = ctx.start + title.length + 2;
  $body.setSelectionRange(caret, caret);
  $ac.hidden = true;
  $body.focus();
}

// ---------------------------------------------------------------------------
// wiring
// ---------------------------------------------------------------------------

/** The prose column grows with the entry; the page scrolls, not the box. */
function grow() {
  $body.style.height = "auto";
  $body.style.height = `${$body.scrollHeight}px`;
}

$body.addEventListener("input", () => {
  grow();
  updateAc();
  updateChrome();
  scheduleSave();
});
$title.addEventListener("input", scheduleSave);
$body.addEventListener("blur", () => {
  // let a mousedown on the completion list run first
  setTimeout(() => {
    $ac.hidden = true;
  }, 0);
});

$back.addEventListener("click", () => void leave());
$keep.addEventListener("click", () => void leave());
$file.addEventListener("click", async () => {
  if (!card) return;
  const saved = await save({ filed: !card.filed });
  if (saved) location.hash = "#/notes";
});

for (const el of [$title, $body] as HTMLElement[]) {
  el.addEventListener("keydown", (e: KeyboardEvent) => {
    e.stopPropagation(); // reader/notes hotkeys must not fire while writing
    if (e.key === "Escape") void leave();
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void leave();
    if (e.key === "Enter" && el === $title && !e.shiftKey) {
      e.preventDefault();
      $body.focus();
    }
  });
}

// esc with focus outside the fields still walks back
document.addEventListener("keydown", (e) => {
  if ($sheet.hidden || e.key !== "Escape") return;
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
  e.stopImmediatePropagation();
  void leave();
});
