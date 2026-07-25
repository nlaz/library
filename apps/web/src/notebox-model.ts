// Pure note-box logic: display addresses (a mirror of the Rust rules —
// minting itself is Rust-only, single authority), thread grouping,
// backlink derivation, wiki-link tokenizing, and the split-whisper
// boundary. No DOM; vitest pins all of it.

import type { CardRec } from "./types";

/** `21/3a2` for thread 21, addr [3,1,2] — bijective base-26 letters on
 * odd segments, mirroring notes.rs display_addr. */
export function displayAddr(thread: number, addr: number[]): string {
  let out = `${thread}/`;
  addr.forEach((seg, i) => {
    out += i % 2 === 0 ? String(seg) : letters(seg);
  });
  return out;
}

function letters(n: number): string {
  let s = "";
  while (n > 0) {
    n -= 1;
    s = String.fromCharCode(97 + (n % 26)) + s;
    n = Math.floor(n / 26);
  }
  return s;
}

/** Thread-view order: lexicographic (thread, addr) — branches read
 * directly after their parent. */
export function compareCards(a: CardRec, b: CardRec): number {
  if (a.thread !== b.thread) return a.thread - b.thread;
  const n = Math.min(a.addr.length, b.addr.length);
  for (let i = 0; i < n; i++) {
    if (a.addr[i] !== b.addr[i]) return a.addr[i] - b.addr[i];
  }
  return a.addr.length - b.addr.length;
}

export type ThreadRow = {
  thread: number;
  /** The thread's name: its first trunk card's title. */
  name: string;
  /** Live (unfiled) cards, thread-view order. */
  cards: CardRec[];
  filed: number;
  lastTouched: number;
};

/** Group cards into threads, live threads sorted by recency (the box you
 * actually use floats up). A thread whose every card is filed drops out —
 * its cards still count in the caller's filed total. */
export function threads(cards: CardRec[]): ThreadRow[] {
  const byThread = new Map<number, CardRec[]>();
  for (const c of cards) {
    const arr = byThread.get(c.thread) ?? [];
    arr.push(c);
    byThread.set(c.thread, arr);
  }
  const rows: ThreadRow[] = [];
  for (const [thread, all] of byThread) {
    all.sort(compareCards);
    const live = all.filter((c) => !c.filed);
    if (!live.length) continue;
    const trunk = all.find((c) => c.addr.length === 1);
    rows.push({
      thread,
      name: (trunk ?? all[0]).title,
      cards: live,
      filed: all.length - live.length,
      lastTouched: Math.max(...live.map((c) => c.modified)),
    });
  }
  rows.sort((a, b) => b.lastTouched - a.lastTouched);
  return rows;
}

/** Cards that point at `target`: typed links plus [[Title]] mentions. */
export function backlinks(cards: CardRec[], target: CardRec): CardRec[] {
  const mention = `[[${target.title}]]`;
  return cards.filter(
    (c) =>
      c.id !== target.id &&
      !c.filed &&
      (c.links.some((l) => l.to === target.id) || c.body.includes(mention)),
  );
}

export type WikiToken = { kind: "text"; text: string } | { kind: "link"; title: string };

/** Split a body into text and [[wiki-link]] tokens for rendering. */
export function wikiTokens(body: string): WikiToken[] {
  const out: WikiToken[] = [];
  const re = /\[\[([^\][]+)\]\]/g;
  let last = 0;
  for (let m = re.exec(body); m; m = re.exec(body)) {
    if (m.index > last) out.push({ kind: "text", text: body.slice(last, m.index) });
    out.push({ kind: "link", title: m[1] });
    last = m.index + m[0].length;
  }
  if (last < body.length) out.push({ kind: "text", text: body.slice(last) });
  return out;
}

/** The split whisper's threshold: past this many words a card is
 * becoming an essay. */
export const SPLIT_WORDS = 150;

/** Where to cut an overlong body: the first sentence boundary at or after
 * SPLIT_WORDS words (falling back to the word boundary). Returns the char
 * index of the cut, or null when the body is still card-sized. */
export function splitPoint(body: string, limit = SPLIT_WORDS): number | null {
  const words = [...body.matchAll(/\S+/g)];
  if (words.length <= limit) return null;
  const from = words[limit - 1].index + words[limit - 1][0].length;
  const rest = body.slice(from);
  const m = rest.match(/[.!?]["')\]]?\s/);
  return m && m.index !== undefined ? from + m.index + m[0].length : from;
}

export function fmtStamp(secs: number): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  return d
    .toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: sameYear ? undefined : "numeric",
    })
    .toLowerCase();
}
