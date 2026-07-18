// Hidden performance view (Cmd+.): recent searches with per-stage timings
// and per-hit ranker provenance, plus per-doc ingest metrics. Dense and
// labeled for human tuning — and self-describing enough (constants header,
// absolute numbers with units) that a screenshot alone carries the state an
// agent needs to troubleshoot. Data comes from the hosts' perf endpoints
// (web: /api/perf/*, desktop: perf_* commands); the view polls while open.

import { isTauri } from "./transport";
import type { IngestRow, PerfMeta, SearchRecord } from "./types";

const $perf = document.getElementById("perf")!;
const $head = document.getElementById("perf-head")!;
const $searchBody = document.getElementById("perf-search-body")!;
const $ingestBody = document.getElementById("perf-ingest-body")!;

const SEARCH_POLL_MS = 1500;
const INGEST_POLL_MS = 3000;

let searchTimer = 0;
let ingestTimer = 0;
// ts_ms of the manually expanded record; null = follow the newest
let pinned: number | null = null;
// last-rendered payloads, to skip DOM churn on unchanged polls
let lastSearches = "";
let lastIngest = "";
// ingest sort: column key + direction (numeric columns sort desc first)
let sortKey = "doc";
let sortDir = 1;

type SearchPayload = { meta: PerfMeta; searches: SearchRecord[] };

async function fetchSearches(): Promise<SearchPayload> {
  if (isTauri()) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<SearchPayload>("perf_searches");
  }
  return (await fetch("/api/perf/searches")).json();
}

async function fetchIngest(): Promise<IngestRow[]> {
  if (isTauri()) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<IngestRow[]>("perf_ingest");
  }
  return (await fetch("/api/perf/ingest")).json();
}

export function perfOpen(): boolean {
  return !$perf.hidden;
}

export function togglePerf() {
  if (perfOpen()) closePerf();
  else openPerf();
}

function openPerf() {
  $perf.hidden = false;
  $searchBody.textContent = "loading…";
  $ingestBody.textContent = "computing… (first open scores legibility for older docs)";
  lastSearches = "";
  lastIngest = "";
  void pollSearches();
  void pollIngest();
  searchTimer = window.setInterval(() => void pollSearches(), SEARCH_POLL_MS);
  ingestTimer = window.setInterval(() => void pollIngest(), INGEST_POLL_MS);
}

function closePerf() {
  $perf.hidden = true;
  window.clearInterval(searchTimer);
  window.clearInterval(ingestTimer);
}

async function pollSearches() {
  let p: SearchPayload;
  try {
    p = await fetchSearches();
  } catch (e) {
    $searchBody.textContent = `perf endpoint unreachable: ${e}`;
    return;
  }
  if (!perfOpen()) return;
  // head is cheap and carries the clock; the table only re-renders on real
  // change (meta.now_ms moves every poll and would churn away text selection)
  renderHead(p.meta);
  const json = JSON.stringify(p.searches);
  if (json === lastSearches) return;
  lastSearches = json;
  renderSearches(p.searches);
}

async function pollIngest() {
  let rows: IngestRow[];
  try {
    rows = await fetchIngest();
  } catch (e) {
    $ingestBody.textContent = `perf endpoint unreachable: ${e}`;
    return;
  }
  if (!perfOpen()) return;
  const json = JSON.stringify(rows);
  if (json === lastIngest) return;
  lastIngest = json;
  renderIngest(rows);
}

// --- rendering --------------------------------------------------------------

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function us(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}ms` : `${n}µs`;
}

function hhmmss(ts: number): string {
  const d = new Date(ts);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

/** Local wall-clock with UTC offset, matching the row timestamps (a mixed
 * local/ISO header sends screenshot readers down the wrong hour). */
function localStamp(ts: number): string {
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

/** "—" for not-recorded, distinct from a real 0. */
function opt(v: number | undefined | null, f: (n: number) => string = String): string {
  return v === undefined || v === null ? "—" : f(v);
}

function renderHead(m: PerfMeta) {
  const host = isTauri() ? "tauri" : "web";
  // f32 constants arrive with f64 noise (0.3499999940395355) — trim for display
  const f = (n: number) => parseFloat(n.toPrecision(4));
  $head.innerHTML =
    `<b>PERF</b> · ${localStamp(m.now_ms)} · host=${host} · debug=${m.debug}` +
    ` · emb_dim=${m.emb_dim} clip_dim=${m.clip_dim}` +
    ` · K=${m.k} K_DOC=${m.k_doc} LEX_FETCH=${m.lex_fetch} IMG_FETCH=${m.img_fetch}` +
    ` · MIN_REL=${f(m.min_rel)} IMG_MIN_REL=${f(m.img_min_rel)} RRF_K=${m.rrf_k}` +
    ` MMR=${f(m.mmr_lambda)}/${m.mmr_pool}` +
    ` · chunks=${m.chunks} figs=${m.figures} docs=${m.docs}` +
    ` · ring=${m.search_log_cap} · <kbd>Cmd+.</kbd> or <kbd>Esc</kbd> to close`;
}

function scopeLabel(r: SearchRecord): string {
  let s = `${r.mode || "?"}/${r.kind || "all"}`;
  if (r.doc) s += `/doc:${r.doc}`;
  if (r.col) s += `/col:${r.col}`;
  if (r.offset) s += `/off=${r.offset}`;
  return s;
}

function renderSearches(recs: SearchRecord[]) {
  if (!recs.length) {
    $searchBody.textContent = "no searches recorded yet — type a query in the main view, then reopen";
    return;
  }
  // auto-expand the newest *primary* search — not an instant keystroke or an
  // infinite-scroll continuation (offset > 0)
  const primary = recs.find((r) => r.mode !== "instant" && !r.offset) ?? recs[0];
  const expandTs = pinned ?? primary.ts_ms;
  const rows = recs
    .map((r) => {
      const stages = r.stages.map(([n, t]) => `${esc(n)}=${us(t)}`).join(" ");
      const killed = `rel:${r.rel_killed} img:${r.img_killed}`;
      const open = r.ts_ms === expandTs;
      const zero = r.zero ? ` <span class="flag">ZERO</span>` : "";
      return (
        `<tr class="${open ? "sel" : ""}" data-ts="${r.ts_ms}">` +
        `<td><span class="twist">${open ? "▾" : "▸"}</span> ${hhmmss(r.ts_ms)}</td>` +
        `<td class="q">${esc(r.q)}</td>` +
        `<td>${esc(scopeLabel(r))}</td>` +
        `<td>${esc(r.phase)}</td>` +
        `<td class="n">${us(r.total_us)}</td>` +
        `<td class="stages">${stages}</td>` +
        `<td class="n">${r.lex_n}/${r.sem_n}</td>` +
        `<td class="n">${killed}</td>` +
        `<td class="n">${r.served}${zero}</td>` +
        `</tr>` +
        (r.ts_ms === expandTs ? `<tr class="detail"><td colspan="9">${detail(r)}</td></tr>` : "")
      );
    })
    .join("");
  $searchBody.innerHTML =
    `<table><thead><tr>` +
    `<th>time</th><th>query</th><th>scope</th><th>phase</th><th>total</th>` +
    `<th>stages</th><th>lex/sem</th><th>killed</th><th>served</th>` +
    `</tr></thead><tbody>${rows}</tbody></table>`;
  for (const tr of $searchBody.querySelectorAll<HTMLElement>("tr[data-ts]")) {
    tr.addEventListener("click", () => {
      const ts = Number(tr.dataset.ts);
      pinned = pinned === ts ? null : ts;
      lastSearches = ""; // force re-render on next poll
      renderSearches(recs);
    });
  }
}

/** Expanded record: stage waterfall + provenance tables. */
function detail(r: SearchRecord): string {
  const total = Math.max(
    r.total_us,
    r.stages.reduce((a, [, t]) => a + t, 0),
    1,
  );
  const bars = r.stages
    .map(([n, t]) => {
      const pct = Math.max(0.5, (t / total) * 100);
      return (
        `<div class="bar-row"><span class="bar-label">${esc(n)}</span>` +
        `<span class="bar-track"><span class="bar" style="width:${pct}%"></span></span>` +
        `<span class="bar-us">${us(t)}</span></div>`
      );
    })
    .join("");

  let text = "";
  if (r.text_hits.length) {
    const rows = r.text_hits
      .map((h, i) => {
        const semOnly = h.lex_rank === null;
        return (
          `<tr>` +
          `<td class="n">${i}</td>` +
          `<td>${esc(h.doc)} p.${h.page} #${h.idx}</td>` +
          `<td class="n">${opt(h.lex_rank)}</td>` +
          `<td class="n">${opt(h.sem_rank)}</td>` +
          `<td class="n">${h.bm25.toFixed(2)}</td>` +
          `<td class="n">${h.rel.toFixed(2)}</td>` +
          `<td class="n">${opt(h.sem_dist, (d) => d.toFixed(3))}</td>` +
          `<td class="n">${h.rrf.toFixed(4)}</td>` +
          `<td>${semOnly ? `<span class="flag">SEM-ONLY</span>` : ""}</td>` +
          `</tr>`
        );
      })
      .join("");
    text =
      `<div class="prov-title">text hits (post-cutoff; lex#/sem# = 0-based rank in each ranker)</div>` +
      `<table><thead><tr><th>#</th><th>doc p. #chunk</th><th>lex#</th><th>sem#</th>` +
      `<th>bm25</th><th>rel</th><th>dist</th><th>rrf</th><th>flags</th></tr></thead>` +
      `<tbody>${rows}</tbody></table>`;
  }

  let imgs = "";
  if (r.img_fetched) {
    const rows = r.img_hits
      .map(
        (h, i) =>
          `<tr><td class="n">${i}</td><td>${esc(h.doc)} p.${h.page} #${h.idx}</td>` +
          `<td class="n">${h.sim.toFixed(3)}</td></tr>`,
      )
      .join("");
    imgs =
      `<div class="prov-title">image hits · top=${r.img_top.toFixed(3)} floor=${r.img_floor.toFixed(3)}` +
      ` kept ${r.img_fetched - r.img_killed}/${r.img_fetched} (spread cutoff)</div>` +
      `<table><thead><tr><th>#</th><th>doc p. #fig</th><th>clip sim</th></tr></thead>` +
      `<tbody>${rows}</tbody></table>`;
  }

  const cols = [text, imgs]
    .filter(Boolean)
    .map((c) => `<div class="prov-col">${c}</div>`)
    .join("");
  return `<div class="waterfall">${bars}</div>${cols ? `<div class="prov-cols">${cols}</div>` : ""}`;
}

// --- ingest -----------------------------------------------------------------

const T = (r: IngestRow) => r.metrics?.timings_ms ?? {};

/** Sortable column accessors; string columns sort asc first, numeric desc. */
const COLS: [string, (r: IngestRow) => string | number | undefined][] = [
  ["doc", (r) => r.title || r.doc],
  ["state", (r) => r.state + (r.stage ? `/${r.stage}` : "")],
  ["pages", (r) => r.pages],
  ["ocr t/v/c", (r) => r.metrics?.ocr?.[1]],
  ["ocr ms", (r) => T(r).ocr],
  ["clean ms", (r) => T(r).clean],
  ["embed ms", (r) => T(r).embed],
  ["fig ms", (r) => T(r).figures],
  ["clip ms", (r) => T(r).clip],
  ["commit ms", (r) => (T(r).commit_text ?? 0) + (T(r).commit_figures ?? 0) || undefined],
  ["leg ms", (r) => T(r).legibility],
  ["total ms", (r) => T(r).total],
  ["chunks", (r) => r.metrics?.chunks?.[0]],
  ["figs", (r) => r.metrics?.figures?.[0]],
  ["leg mean/med", (r) => r.metrics?.legibility?.mean],
  ["noisy%", (r) => r.metrics?.legibility?.noisy_pct],
  ["worst", (r) => r.metrics?.legibility?.worst?.[0]?.[1]],
  ["error", (r) => r.error],
];

function ingestCells(r: IngestRow): string {
  const m = r.metrics;
  const t = T(r);
  const leg = m?.legibility;
  const ms = (v: number | undefined) => opt(v, (n) => n.toLocaleString());
  return (
    `<td class="q" title="${esc(r.doc)}">${esc(r.title || r.doc)}</td>` +
    `<td>${esc(r.state)}${r.stage ? `/${esc(r.stage)}` : ""}</td>` +
    `<td class="n">${r.pages || "—"}</td>` +
    `<td class="n">${m?.ocr ? `${m.ocr[0]}/${m.ocr[1]}/${m.ocr[2]}` : "—"}</td>` +
    `<td class="n">${ms(t.ocr)}</td>` +
    `<td class="n">${ms(t.clean)}</td>` +
    `<td class="n">${ms(t.embed)}</td>` +
    `<td class="n">${ms(t.figures)}</td>` +
    `<td class="n">${ms(t.clip)}</td>` +
    `<td class="n">${
      t.commit_text !== undefined || t.commit_figures !== undefined
        ? ((t.commit_text ?? 0) + (t.commit_figures ?? 0)).toLocaleString()
        : "—"
    }</td>` +
    `<td class="n">${ms(t.legibility)}</td>` +
    `<td class="n">${ms(t.total)}</td>` +
    `<td class="n">${m?.chunks ? `+${m.chunks[0]}/−${m.chunks[1]}` : "—"}</td>` +
    `<td class="n">${m?.figures ? `+${m.figures[0]}/−${m.figures[1]}` : "—"}</td>` +
    `<td class="n">${leg ? `${leg.mean.toFixed(2)}/${leg.median.toFixed(2)}` : "—"}</td>` +
    `<td class="n">${leg ? (leg.noisy_pct * 100).toFixed(1) : "—"}</td>` +
    `<td class="n">${
      leg?.worst?.length ? leg.worst.map(([p, s]) => `p.${p}=${s.toFixed(2)}`).join(" ") : "—"
    }</td>` +
    `<td class="err" title="${esc(r.error ?? "")}">${esc((r.error ?? "").slice(0, 60))}</td>`
  );
}

function renderIngest(rows: IngestRow[]) {
  if (!rows.length) {
    $ingestBody.textContent = "no documents";
    return;
  }
  const col = COLS.find(([k]) => k === sortKey) ?? COLS[0];
  const sorted = [...rows].sort((a, b) => {
    const [va, vb] = [col[1](a), col[1](b)];
    if (va === undefined || va === null) return 1; // missing values sink
    if (vb === undefined || vb === null) return -1;
    const c = typeof va === "number" && typeof vb === "number"
      ? va - vb
      : String(va).localeCompare(String(vb));
    return c * sortDir;
  });

  const states = new Map<string, number>();
  for (const r of rows) states.set(r.state, (states.get(r.state) ?? 0) + 1);
  const sum = (f: (r: IngestRow) => number | undefined) =>
    rows.reduce((a, r) => a + (f(r) ?? 0), 0);
  const totals =
    `${rows.length} docs (${[...states].map(([s, n]) => `${s}:${n}`).join(" ")})` +
    ` · Σpages=${sum((r) => r.pages)} · Σchunks=${sum((r) => r.metrics?.chunks?.[0])}` +
    ` · Σfigs=${sum((r) => r.metrics?.figures?.[0])}` +
    ` · legibility 0..1 (mean/median per doc; noisy% = pages w/ worst window < 0.45)` +
    ` · ocr t/v/c = text-layer/vision/cached pages · — = not recorded`;

  const head = COLS.map(
    ([k]) =>
      `<th class="sort ${k === sortKey ? "on" : ""}" data-col="${esc(k)}">${esc(k)}` +
      `${k === sortKey ? (sortDir > 0 ? " ↑" : " ↓") : ""}</th>`,
  ).join("");
  const body = sorted.map((r) => `<tr>${ingestCells(r)}</tr>`).join("");
  $ingestBody.innerHTML =
    `<div class="perf-totals">${totals}</div>` +
    `<table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table>`;

  for (const th of $ingestBody.querySelectorAll<HTMLElement>("th.sort")) {
    th.addEventListener("click", () => {
      const k = th.dataset.col!;
      if (sortKey === k) sortDir = -sortDir;
      else {
        sortKey = k;
        // numeric columns start with biggest-first (that's what tuning wants)
        sortDir = k === "doc" || k === "state" || k === "error" ? 1 : -1;
      }
      lastIngest = "";
      renderIngest(rows);
    });
  }
}
