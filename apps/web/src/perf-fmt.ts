// Formatting helpers for the perf view. DOM-free on purpose: perf.ts and
// perf-agent.ts both need them, and keeping them in a leaf module means the
// unit tests can import them without standing up the overlay's markup.

import type { MemoryBreakdown } from "./types";

/** The total the memory tab's line items are measured against.
 *
 * phys_footprint, falling back to resident_size when the host had no
 * footprint probe. The order matters: rss omits pages the memory compressor
 * has taken, so on an idle app it can land far below the estimates that are
 * trying to explain it — which is how the panel once showed rss=23.5MiB
 * against accounted=205.1MiB and an unaccounted remainder of -181.6MiB.
 *
 * Mirrors `HostMem::total` in library-core's perf.rs; both sides pick
 * footprint first, and the Rust test pins the same ordering. */
export function memTotal(m: MemoryBreakdown): number | null {
  return m.footprint_bytes ?? m.rss_bytes;
}

export function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/** Search stages are measured in µs; roll over to ms past 1000. */
export function us(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}ms` : `${n}µs`;
}

/** Agent durations are measured in ms; roll over to seconds past 1000 — a
 * turn runs seconds, a tool call runs hundreds of ms. */
export function ms(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(2)}s` : `${Math.round(n)}ms`;
}

export function hhmmss(ts: number): string {
  const d = new Date(ts);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

/** Local wall-clock with UTC offset, matching the row timestamps (a mixed
 * local/ISO header sends screenshot readers down the wrong hour). */
export function localStamp(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  const off = -d.getTimezoneOffset();
  const sign = off >= 0 ? "+" : "-";
  const [oh, om] = [Math.floor(Math.abs(off) / 60), Math.abs(off) % 60];
  return (
    `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}` +
    ` ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}` +
    ` UTC${sign}${oh}${om ? `:${p(om)}` : ""}`
  );
}

/** Human-readable bytes in binary units, one decimal past KiB. Negative
 * values keep their sign (the unaccounted remainder can be negative). */
export function bytes(n: number): string {
  if (n < 0) return `-${bytes(-n)}`;
  if (n < 1024) return `${n}B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)}${units[i]}`;
}

/** "—" for not-recorded, distinct from a real 0. */
export function opt(v: number | undefined | null, f: (n: number) => string = String): string {
  return v === undefined || v === null ? "—" : f(v);
}

/** A glossed header term. The raw constant name stays literal — a screenshot
 * must still carry it — and the popover adds the plain-English reading, keyed
 * off `name` in perf-gloss.ts's registry. */
export function term(name: string, text: string): string {
  return `<b data-term="${esc(name)}" tabindex="0">${esc(text)}</b>`;
}

/** Median of a list, or null when there's nothing to summarize. Used by the
 * header's live-evidence lines, which must say nothing rather than NaN. */
export function median(xs: number[]): number | null {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}
