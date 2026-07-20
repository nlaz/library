// The card composer: always the same small card, never a full-screen
// editor — a card should feel like a 3×5. Create mode carries its birth
// context (parent branch / thread append / fresh thread); edit mode
// carries the card. The only decision the writer makes is the claim.

import { createCard, listCards, updateCard } from "./marginalia-api";
import { SPLIT_WORDS, displayAddr, splitPoint } from "./notebox-model";
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
    <textarea class="cp-body" rows="5" placeholder="a few sentences of your own reasoning… ([[ links a card)"></textarea>
    <ul class="cp-ac" hidden></ul>
    <div class="cp-evidence"></div>
    <button class="cp-whisper" hidden>this is becoming an essay · split?</button>
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
const $ac = root.querySelector<HTMLUListElement>(".cp-ac")!;
const $evidence = root.querySelector<HTMLElement>(".cp-evidence")!;
const $whisper = root.querySelector<HTMLButtonElement>(".cp-whisper")!;
const $place = root.querySelector<HTMLElement>(".cp-place")!;
const $save = root.querySelector<HTMLButtonElement>(".cp-save")!;
const $discard = root.querySelector<HTMLButtonElement>(".cp-discard")!;

/** Live card titles for [[ completion; refreshed on every open. */
let titles: string[] = [];

export function composerOpen(): boolean {
  return !root.hidden;
}

export function openComposer(m: Mode, done: (saved: CardRec | null) => void) {
  mode = m;
  onDone = done;
  root.hidden = false;
  $ac.hidden = true;
  listCards()
    .then((cs) => {
      titles = cs.filter((c) => !c.filed).map((c) => c.title);
    })
    .catch(() => {
      titles = [];
    });
  updateWhisper();
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

/** Persist the composer's state; null = validation stopped it. Doesn't
 * close — save() and the split flow share it. */
async function doSave(extra?: { filed?: boolean; splitHinted?: boolean }): Promise<CardRec | null> {
  if (!mode) return null;
  const title = $title.value.trim();
  if (!title && !extra?.filed) {
    $title.focus();
    return null;
  }
  try {
    if (mode.kind === "edit") {
      return await updateCard({
        ...mode.card,
        title: title || mode.card.title,
        body: $body.value,
        filed: extra?.filed ?? mode.card.filed,
        split_hinted: extra?.splitHinted ?? mode.card.split_hinted,
      });
    }
    const s = mode.seed;
    const input: NewCard = {
      title,
      body: $body.value,
      evidence: s.evidence ?? [],
      links: [],
      parent: s.parent?.id ?? null,
      thread: s.parent ? null : (s.thread ?? null),
    };
    return await createCard(input);
  } catch (e) {
    notify(`couldn't save card: ${e instanceof Error ? e.message : e}`);
    return null;
  }
}

async function save(extra?: { filed?: boolean }) {
  // an accepted-or-visible whisper never nags the same card twice
  const hinted = mode?.kind === "edit" && !$whisper.hidden ? { splitHinted: true } : {};
  const saved = await doSave({ ...extra, ...hinted });
  if (saved) close(saved);
}

// ---------------------------------------------------------------------------
// the split whisper: past ~150 words a card is becoming an essay
// ---------------------------------------------------------------------------

function wordCount(s: string): number {
  return (s.match(/\S+/g) ?? []).length;
}

function updateWhisper() {
  const over = wordCount($body.value) > SPLIT_WORDS;
  const nagged = mode?.kind === "edit" && mode.card.split_hinted;
  $whisper.hidden = !over || !!nagged;
}

$whisper.addEventListener("click", async () => {
  const cut = splitPoint($body.value);
  if (cut == null || !mode) return;
  const done = onDone ?? (() => {}); // the branch card reports through it too
  const rest = $body.value.slice(cut).trim();
  $body.value = $body.value.slice(0, cut).trimEnd();
  const saved = await doSave({ splitHinted: true });
  if (!saved) return;
  close(saved);
  // the overflow becomes a branch card — same composer, new birth
  openComposer(
    {
      kind: "create",
      seed: {
        body: rest,
        parent: {
          id: saved.id,
          address: displayAddr(saved.thread, saved.addr),
          title: saved.title,
        },
      },
    },
    done,
  );
});

// ---------------------------------------------------------------------------
// [[ autocomplete over live card titles
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

$body.addEventListener("input", () => {
  updateAc();
  updateWhisper();
});
$body.addEventListener("blur", () => {
  // let a mousedown on the list run first
  setTimeout(() => {
    $ac.hidden = true;
  }, 0);
});

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
