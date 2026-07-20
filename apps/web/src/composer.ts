// The card composer: always the same small card, never a full-screen
// editor — a card should feel like a 3×5. Create mode carries its birth
// context (parent branch / thread append / fresh thread); edit mode
// carries the card. The only decision the writer makes is the claim.

import { createCard, updateCard } from "./marginalia-api";
import { displayAddr } from "./notebox-model";
import { notify } from "./toast";
import type { CardRec, NewCard, QuoteAnchor } from "./types";

export type ComposerSeed = {
  title?: string;
  body?: string;
  evidence?: QuoteAnchor[];
  parent?: { id: string; address: string; title: string } | null;
  thread?: number | null;
};

type Mode = { kind: "create"; seed: ComposerSeed } | { kind: "edit"; card: CardRec };

let mode: Mode | null = null;
let onDone: ((saved: CardRec | null) => void) | null = null;

const root = document.createElement("div");
root.id = "composer";
root.hidden = true;
root.innerHTML = `
  <div class="cp-card">
    <div class="cp-head"><span class="cp-addr"></span><button class="cp-file"></button></div>
    <input class="cp-title" type="text" placeholder="the claim, as a sentence…" />
    <textarea class="cp-body" rows="5" placeholder="a few sentences of your own reasoning…"></textarea>
    <div class="cp-evidence"></div>
    <div class="cp-foot">
      <span class="cp-place"></span>
      <span class="cp-actions">
        <button class="cp-discard">esc discards</button>
        <button class="cp-save">save</button>
      </span>
    </div>
  </div>
`;
document.body.append(root);

const $addr = root.querySelector<HTMLElement>(".cp-addr")!;
const $file = root.querySelector<HTMLButtonElement>(".cp-file")!;
const $title = root.querySelector<HTMLInputElement>(".cp-title")!;
const $body = root.querySelector<HTMLTextAreaElement>(".cp-body")!;
const $evidence = root.querySelector<HTMLElement>(".cp-evidence")!;
const $place = root.querySelector<HTMLElement>(".cp-place")!;
const $save = root.querySelector<HTMLButtonElement>(".cp-save")!;
const $discard = root.querySelector<HTMLButtonElement>(".cp-discard")!;

export function composerOpen(): boolean {
  return !root.hidden;
}

export function openComposer(m: Mode, done: (saved: CardRec | null) => void) {
  mode = m;
  onDone = done;
  root.hidden = false;
  if (m.kind === "edit") {
    $addr.textContent = displayAddr(m.card.thread, m.card.addr);
    $title.value = m.card.title;
    $body.value = m.card.body;
    renderEvidence(m.card.evidence);
    $place.textContent = "";
    $file.hidden = false;
    $file.textContent = m.card.filed ? "restore" : "file away";
  } else {
    const s = m.seed;
    $addr.textContent = "new card";
    $title.value = s.title ?? "";
    $body.value = s.body ?? "";
    renderEvidence(s.evidence ?? []);
    $place.textContent = s.parent
      ? `files after ${s.parent.address} ${s.parent.title}`
      : s.thread != null
        ? `files in thread ${s.thread}`
        : "starts a new thread";
    $file.hidden = true;
  }
  $title.focus();
}

function renderEvidence(evs: QuoteAnchor[]) {
  $evidence.replaceChildren();
  for (const q of evs) {
    const chip = document.createElement("div");
    chip.className = "cp-quote";
    const t = document.createElement("div");
    t.className = "cp-quote-text";
    t.textContent = `“${q.text}”`;
    const src = document.createElement("button");
    src.className = "cp-quote-src";
    src.textContent = `${q.doc} · p.${q.page} ↗`;
    src.addEventListener("click", () => {
      close(null);
      location.hash = `#/read/${q.doc}?p=${q.page}`;
    });
    chip.append(t, src);
    $evidence.append(chip);
  }
}

function close(saved: CardRec | null) {
  root.hidden = true;
  const cb = onDone;
  mode = null;
  onDone = null;
  cb?.(saved);
}

async function save(extra?: { filed?: boolean }) {
  if (!mode) return;
  const title = $title.value.trim();
  if (!title && !extra) {
    $title.focus();
    return;
  }
  try {
    if (mode.kind === "edit") {
      const saved = await updateCard({
        ...mode.card,
        title: title || mode.card.title,
        body: $body.value,
        filed: extra?.filed ?? mode.card.filed,
      });
      close(saved);
    } else {
      const s = mode.seed;
      const input: NewCard = {
        title,
        body: $body.value,
        evidence: s.evidence ?? [],
        links: [],
        parent: s.parent?.id ?? null,
        thread: s.parent ? null : (s.thread ?? null),
      };
      close(await createCard(input));
    }
  } catch (e) {
    notify(`couldn't save card: ${e instanceof Error ? e.message : e}`);
  }
}

$save.addEventListener("click", () => save());
$discard.addEventListener("click", () => close(null));
$file.addEventListener("click", () => {
  if (mode?.kind === "edit") save({ filed: !mode.card.filed });
});

for (const el of [$title, $body] as HTMLElement[]) {
  el.addEventListener("keydown", (e: KeyboardEvent) => {
    e.stopPropagation(); // notebox/reader hotkeys must not fire while typing
    if (e.key === "Escape") close(null);
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) save();
    if (e.key === "Enter" && el === $title && !e.shiftKey) {
      e.preventDefault();
      $body.focus();
    }
  });
}

root.addEventListener("mousedown", (e) => {
  if (e.target === root) close(null); // click the scrim = discard
});
