// Hidden performance view (Cmd+.): four tabs over the subsystems worth
// tuning — recent searches with per-stage timings and per-hit ranker
// provenance, per-doc ingest metrics, the chat agent's turn provenance,
// and memory provenance (where RAM goes, against process RSS).
// Dense and labeled for human tuning — and self-describing enough (grouped
// constants header, absolute numbers with units) that a screenshot alone
// carries the state an agent needs to troubleshoot.
//
// Search and ingest data come from the hosts' perf endpoints (web:
// /api/perf/*, desktop: perf_* commands) and are polled only while their tab
// is showing. The agent tab reads a browser-side ring fed by chat.ts, so it
// costs nothing to keep live.

import { agentLog, agentVersion } from "./agent-log";
import { renderAgent } from "./perf-agent";
import { bytes, esc, hhmmss, localStamp, opt, term, us } from "./perf-fmt";
import { GLOSS, evidence } from "./perf-gloss";
import { isTauri } from "./transport";
import type { IngestRow, MemoryBreakdown, PerfMeta, SearchRecord, StoreMem } from "./types";

const $perf = document.getElementById("perf")!;
const $id = document.getElementById("perf-id")!;
const $tabs = document.getElementById("perf-tabs")!;
const $facts = document.getElementById("perf-facts")!;
const $searchBody = document.getElementById("perf-search-body")!;
const $ingestBody = document.getElementById("perf-ingest-body")!;
const $agentBody = document.getElementById("perf-agent-body")!;
const $memoryBody = document.getElementById("perf-memory-body")!;
const $pop = document.getElementById("perf-pop")!;

type Tab = "search" | "ingest" | "agent" | "memory";
const PANES: Record<Tab, HTMLElement> = {
  search: $searchBody,
  ingest: $ingestBody,
  agent: $agentBody,
  memory: $memoryBody,
};

const HEAD_TICK_MS = 1000;
const SEARCH_POLL_MS = 1500;
const INGEST_POLL_MS = 3000;
// memory moves slowly and each poll takes read locks server-side
const MEMORY_POLL_MS = 5000;

let tab: Tab = "search";
let headTimer = 0;
let searchTimer = 0;
let ingestTimer = 0;
let memoryTimer = 0;
// ts_ms of the manually expanded record; null = follow the newest
let pinned: number | null = null;
// last-rendered payloads, to skip DOM churn on unchanged polls
let lastSearches = "";
let lastIngest = "";
let lastAgent = -1;
let lastMemory = "";
// ingest sort: column key + direction (numeric columns sort desc first)
let sortKey = "doc";
let sortDir = 1;
// search tab: which side of the shared ring to show
type SrcFilter = "all" | "ui" | "agent";
let srcFilter: SrcFilter = "all";
// cached snapshots: the head renders off these between polls, and the tab
// facts are derived from them
let meta: PerfMeta | null = null;
let searches: SearchRecord[] = [];
let ingest: IngestRow[] = [];
let mem: MemoryBreakdown | null = null;
// store names whose per-keyspace rows are expanded
const memOpen = new Set<string>();
// client clock minus server clock at the last fetch; row timestamps are
// server-stamped but rendered against the local clock, so drift matters
let skewMs = 0;
// per-tab header facts, written by the renderers
const facts: Record<Tab, [string, string][]> = { search: [], ingest: [], agent: [], memory: [] };

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

async function fetchMemory(): Promise<MemoryBreakdown> {
  if (isTauri()) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<MemoryBreakdown>("perf_memory");
  }
  return (await fetch("/api/perf/memory")).json();
}

export function perfOpen(): boolean {
  return !$perf.hidden;
}

export function togglePerf() {
  if (perfOpen()) closePerf();
  else openPerf();
}

/** Show a tab; exported for the digit hotkeys in main.ts. */
export function setPerfTab(t: Tab) {
  tab = t;
  for (const k of Object.keys(PANES) as Tab[]) PANES[k].hidden = k !== t;
  for (const b of $tabs.querySelectorAll<HTMLElement>("button[data-tab]")) {
    const on = b.dataset.tab === t;
    b.classList.toggle("on", on);
    b.setAttribute("aria-selected", String(on));
  }
  // only the visible tab polls; the head keeps ticking either way
  window.clearInterval(searchTimer);
  window.clearInterval(ingestTimer);
  window.clearInterval(memoryTimer);
  if (t === "search") {
    void pollSearches();
    searchTimer = window.setInterval(() => void pollSearches(), SEARCH_POLL_MS);
  } else if (t === "ingest") {
    void pollIngest();
    ingestTimer = window.setInterval(() => void pollIngest(), INGEST_POLL_MS);
  } else if (t === "memory") {
    void pollMemory();
    memoryTimer = window.setInterval(() => void pollMemory(), MEMORY_POLL_MS);
  } else {
    renderAgentPane();
  }
  renderHead();
}

/** Jump to a search record and expand it — the agent tab's tool-call link. */
export function pinSearch(ts: number) {
  pinned = ts;
  // the target is an agent record; a filter excluding it would land the user
  // on a table that doesn't contain what they clicked
  srcFilter = "all";
  setPerfTab("search");
  lastSearches = "";
  renderSearches(searches);
  $searchBody.querySelector("tr.sel")?.scrollIntoView({ block: "center" });
}

function openPerf() {
  $perf.hidden = false;
  // this layer covers everything; focus left in a now-hidden input would
  // swallow the digit tab keys (and strand a keyboard user behind the view)
  (document.activeElement as HTMLElement | null)?.blur();
  $searchBody.textContent = "loading…";
  $ingestBody.textContent = "computing… (first open scores legibility for older docs)";
  $memoryBody.textContent = "loading…";
  lastSearches = "";
  lastIngest = "";
  lastAgent = -1;
  lastMemory = "";
  // both fire once regardless of tab: searches carry the only copy of
  // PerfMeta (the head would be blank without it), and the ingest endpoint
  // backfills legibility lazily, which is the slow part — paying it now
  // keeps the first switch to that tab from stalling
  void pollSearches();
  void pollIngest();
  setPerfTab(tab);
  headTimer = window.setInterval(tick, HEAD_TICK_MS);
}

function closePerf() {
  $perf.hidden = true;
  window.clearInterval(headTimer);
  window.clearInterval(searchTimer);
  window.clearInterval(ingestTimer);
  window.clearInterval(memoryTimer);
}

/** Cheap always-on tick: advances the clock and catches a chat turn that is
 * streaming behind the overlay. No network. */
function tick() {
  renderHead();
  if (tab === "agent" && agentVersion() !== lastAgent) renderAgentPane();
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
  meta = p.meta;
  // hysteresis: raw skew jitters a few ms per poll, which is noise at this
  // scale and would rewrite the header a second — tearing out whatever term
  // the pointer is resting on
  const raw = Date.now() - p.meta.now_ms;
  if (Math.abs(raw - skewMs) > 25) skewMs = raw;
  searches = p.searches;
  const json = JSON.stringify(p.searches);
  // the table only re-renders on real change — meta.now_ms moves every poll
  // and would churn away text selection
  if (json !== lastSearches) {
    lastSearches = json;
    renderSearches(p.searches);
  }
  renderHead();
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
  ingest = rows;
  const json = JSON.stringify(rows);
  if (json === lastIngest) return;
  lastIngest = json;
  renderIngest(rows);
  renderHead();
}

async function pollMemory() {
  let m: MemoryBreakdown;
  try {
    m = await fetchMemory();
  } catch (e) {
    $memoryBody.textContent = `perf endpoint unreachable: ${e}`;
    return;
  }
  if (!perfOpen()) return;
  mem = m;
  // now_ms moves every poll; diff the rest so a quiet heap doesn't churn
  // the DOM (and tear out text selection) every 5 s
  const json = JSON.stringify({ ...m, now_ms: 0 });
  if (json === lastMemory) return;
  lastMemory = json;
  renderMemory(m);
  renderHead();
}

function renderAgentPane() {
  lastAgent = agentVersion();
  facts.agent = renderAgent($agentBody, agentLog(), pinSearch);
  renderHead();
}

// --- header -----------------------------------------------------------------

function num(n: number): string {
  return n.toLocaleString();
}

// last-written header markup. The clock ticks every second but the rest of
// the head rarely changes, and rewriting it would tear out the term the
// pointer is resting on — which dismissed the popover a second after it
// appeared. So the clock is a text node and everything else is diff-guarded.
let lastId = "";
let lastFacts = "";
let $clock: HTMLElement | null = null;

function renderHead() {
  const host = isTauri() ? "tauri" : "web";
  const skew = `${skewMs >= 0 ? "+" : ""}${Math.round(skewMs)}ms`;
  const corpus = meta
    ? ` · ${term("docs", `docs=${num(meta.docs)}`)} ${term("chunks", `chunks=${num(meta.chunks)}`)}` +
      ` ${term("figures", `figs=${num(meta.figures)}`)}`
    : "";
  const id =
    `<b>PERF</b> · <span id="perf-clock"></span> · host=${host}` +
    (meta ? ` · ${term("debug", `debug=${meta.debug}`)}` : "") +
    ` · ${term("skew", `skew=${skew}`)}` +
    corpus +
    ` · <kbd>Cmd+.</kbd>/<kbd>Esc</kbd> close · <kbd>1</kbd><kbd>2</kbd><kbd>3</kbd><kbd>4</kbd> tabs`;
  if (id !== lastId) {
    lastId = id;
    $id.innerHTML = id;
    $clock = $id.querySelector("#perf-clock");
  }
  if ($clock) $clock.textContent = localStamp(Date.now());

  const counts: Record<Tab, number> = {
    search: searches.length,
    ingest: ingest.length,
    agent: agentLog().length,
    memory: mem ? mem.indexes.length + mem.caches.length + mem.models.length + mem.stores.length : 0,
  };
  for (const b of $tabs.querySelectorAll<HTMLElement>("button[data-tab]")) {
    const n = b.querySelector(".n");
    const v = String(counts[b.dataset.tab as Tab]);
    if (n && n.textContent !== v) n.textContent = v;
  }

  if (tab === "search") facts.search = searchFacts();
  const rows = facts[tab];
  const html = rows.length
    ? rows.map(([g, body]) => `<span class="fact"><i>${esc(g)}</i> ${body}</span>`).join("")
    : "";
  if (html !== lastFacts) {
    lastFacts = html;
    $facts.innerHTML = html;
  }
}

function searchFacts(): [string, string][] {
  if (!meta) return [];
  // f32 constants arrive with f64 noise (0.3499999940395355) — trim for display
  const f = (n: number) => parseFloat(n.toPrecision(4));
  const zero = searches.filter((r) => r.zero).length;
  return [
    [
      "fetch",
      `${term("K", `K=${meta.k}`)} ${term("K_DOC", `K_DOC=${meta.k_doc}`)}` +
        ` ${term("LEX_FETCH", `LEX_FETCH=${meta.lex_fetch}`)}` +
        ` ${term("IMG_FETCH", `IMG_FETCH=${meta.img_fetch}`)}`,
    ],
    [
      "cutoff",
      `${term("MIN_REL", `MIN_REL=${f(meta.min_rel)}`)}` +
        ` ${term("IMG_MIN_REL", `IMG_MIN_REL=${f(meta.img_min_rel)}`)}`,
    ],
    [
      "fuse",
      `${term("FUSE_W", `FUSE_W=${f(meta.fuse_lex_weight)}`)}` +
        ` ${term("NOTE_BOOST", `NOTE_BOOST=${f(meta.note_boost)}`)}`,
    ],
    [
      "rerank",
      `${term("RERANK", `RERANK=${f(meta.rerank_weight)}/${meta.rerank_pool}`)}` +
        ` ${term("MMR", `MMR=${f(meta.mmr_lambda)}/${meta.mmr_pool}`)}` +
        ` ${term("FUZZ_CANDIDATES", `FUZZ=${meta.fuzz_candidates}`)}`,
    ],
    [
      "dim",
      `${term("emb_dim", `emb_dim=${meta.emb_dim}`)} ${term("clip_dim", `clip_dim=${meta.clip_dim}`)}`,
    ],
    [
      "ring",
      `${term("search_log_cap", `search_log_cap=${meta.search_log_cap}`)}` +
        ` · ${searches.length} records, ${zero} zero` +
        (searches.length ? ` · newest ${hhmmss(searches[0].ts_ms)}` : ""),
    ],
  ];
}

// --- search -----------------------------------------------------------------

function scopeLabel(r: SearchRecord): string {
  let s = `${r.mode || "?"}/${r.kind || "all"}`;
  if (r.doc) s += `/doc:${r.doc}`;
  if (r.col) s += `/col:${r.col}`;
  if (r.offset) s += `/off=${r.offset}`;
  return s;
}

function renderSearches(all: SearchRecord[]) {
  if (!all.length) {
    $searchBody.textContent = "no searches recorded yet — type a query in the main view, then reopen";
    return;
  }
  // the agent's tool searches share this ring; they're the majority during a
  // chat session and drown out the UI's, so the source is filterable
  const recs = all.filter((r) => srcFilter === "all" || (r.mode === "agent") === (srcFilter === "agent"));
  const agentN = all.filter((r) => r.mode === "agent").length;
  const chips = (["all", "ui", "agent"] as const)
    .map(
      (k) =>
        `<button class="chip divot ghost ${k === srcFilter ? "on" : ""}" data-src="${k}">${k}` +
        `<span class="n">${k === "all" ? all.length : k === "agent" ? agentN : all.length - agentN}</span>` +
        `</button>`,
    )
    .join("");
  if (!recs.length) {
    $searchBody.innerHTML = `<div class="perf-chips">${chips}</div><div class="perf-note">no ${srcFilter} searches in the ring</div>`;
    wireSrcChips(all);
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
        // the agent path calls search() directly, which never reports
        // pre-fusion list sizes — say "not measured", not "0/0"
        `<td class="n">${r.mode === "agent" ? "—" : `${r.lex_n}/${r.sem_n}`}</td>` +
        `<td class="n">${killed}</td>` +
        `<td class="n">${r.served}${zero}</td>` +
        `</tr>` +
        (r.ts_ms === expandTs ? `<tr class="detail"><td colspan="9">${detail(r)}</td></tr>` : "")
      );
    })
    .join("");
  $searchBody.innerHTML =
    `<div class="perf-chips">${chips}</div>` +
    `<table><thead><tr>` +
    `<th>time</th><th>query</th><th>scope</th><th>phase</th><th>total</th>` +
    `<th>stages</th><th>lex/sem</th><th>killed</th><th>served</th>` +
    `</tr></thead><tbody>${rows}</tbody></table>`;
  for (const tr of $searchBody.querySelectorAll<HTMLElement>("tr[data-ts]")) {
    tr.addEventListener("click", () => {
      const ts = Number(tr.dataset.ts);
      pinned = pinned === ts ? null : ts;
      lastSearches = ""; // force re-render on next poll
      renderSearches(all);
    });
  }
  wireSrcChips(all);
}

function wireSrcChips(all: SearchRecord[]) {
  for (const b of $searchBody.querySelectorAll<HTMLElement>("button[data-src]")) {
    b.addEventListener("click", () => {
      srcFilter = b.dataset.src as SrcFilter;
      lastSearches = "";
      renderSearches(all);
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
          `<td class="n">${h.score.toFixed(4)}</td>` +
          `<td>${semOnly ? `<span class="flag">SEM-ONLY</span>` : ""}</td>` +
          `</tr>`
        );
      })
      .join("");
    text =
      `<div class="prov-title">text hits (post-cutoff; lex#/sem# = 0-based rank in each ranker)</div>` +
      `<table><thead><tr><th>#</th><th>doc p. #chunk</th><th>lex#</th><th>sem#</th>` +
      `<th>bm25</th><th>rel</th><th>dist</th><th>score</th><th>flags</th></tr></thead>` +
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

function ingestFacts(rows: IngestRow[]): [string, string][] {
  const states = new Map<string, number>();
  for (const r of rows) states.set(r.state, (states.get(r.state) ?? 0) + 1);
  const sum = (f: (r: IngestRow) => number | undefined) =>
    rows.reduce((a, r) => a + (f(r) ?? 0), 0);
  return [
    ["state", [...states].map(([s, n]) => `${s}:${n}`).join(" ") || "—"],
    [
      "total",
      `${rows.length} docs · Σpages=${num(sum((r) => r.pages))}` +
        ` · Σchunks=${num(sum((r) => r.metrics?.chunks?.[0]))}` +
        ` · Σfigs=${num(sum((r) => r.metrics?.figures?.[0]))}`,
    ],
    [
      "scale",
      `${term("legibility", "legibility")} 0..1 (mean/median per doc)` +
        ` · ${term("noisy%", "noisy%")} = pages w/ worst window < 0.45` +
        ` · ${term("ocr t/v/c", "ocr t/v/c")} = text-layer/vision/cached pages · — = not recorded`,
    ],
    ["sort", `${sortKey} ${sortDir > 0 ? "↑" : "↓"}`],
  ];
}

function renderIngest(rows: IngestRow[]) {
  facts.ingest = ingestFacts(rows);
  if (!rows.length) {
    $ingestBody.textContent = "no documents";
    return;
  }
  const col = COLS.find(([k]) => k === sortKey) ?? COLS[0];
  const sorted = [...rows].sort((a, b) => {
    const [va, vb] = [col[1](a), col[1](b)];
    if (va === undefined || va === null) return 1; // missing values sink
    if (vb === undefined || vb === null) return -1;
    const c =
      typeof va === "number" && typeof vb === "number"
        ? va - vb
        : String(va).localeCompare(String(vb));
    return c * sortDir;
  });

  const head = COLS.map(
    ([k]) =>
      `<th class="sort ${k === sortKey ? "on" : ""}" data-col="${esc(k)}">${esc(k)}` +
      `${k === sortKey ? (sortDir > 0 ? " ↑" : " ↓") : ""}</th>`,
  ).join("");
  const body = sorted.map((r) => `<tr>${ingestCells(r)}</tr>`).join("");
  $ingestBody.innerHTML = `<table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table>`;

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
      renderHead();
    });
  }
}

// --- memory -----------------------------------------------------------------

// Keyspaces the current graphs open. Anything else in a store is an orphan
// left behind by an older pipeline shape — disk weight worth flagging.
const KNOWN_KS: Record<string, string[]> = {
  "library.db": ["sink_lex", "sink_vec", "sink_vec_graph", "sink_manifest", "sink_terms", "keyed_root"],
  "images.db": ["sink_imgvec", "sink_imgvec_graph", "sink_imgmeta", "sink_imgmanifest", "keyed_root"],
};

/** Everything the corpus weighs on disk, across its sources. */
function corpusDisk(m: MemoryBreakdown): number {
  const c = m.corpus;
  return c.pdf_bytes + c.page_bytes + c.ocr_bytes + c.chunk_table_bytes + c.figure_table_bytes;
}

function memoryFacts(m: MemoryBreakdown): [string, string][] {
  const rss = m.rss_bytes;
  const pct = rss ? ` (${Math.round((m.accounted_bytes / rss) * 100)}%)` : "";
  return [
    [
      "resident",
      `${term("rss", `rss=${opt(rss, bytes)}`)}` +
        ` ${term("accounted", `accounted=${bytes(m.accounted_bytes)}${pct}`)}` +
        ` ${term("unaccounted", `unaccounted=${opt(m.unaccounted_bytes, bytes)}`)}` +
        (m.atlas_building ? ` <span class="flag">ATLAS BUILDING</span>` : ""),
    ],
    [
      "corpus",
      `docs=${num(m.corpus.docs)} chunks=${num(m.corpus.chunks)} figs=${num(m.corpus.figures)}` +
        ` · ${term("corpus disk", `disk=${bytes(corpusDisk(m))}`)}`,
    ],
    [
      "caveat",
      `estimates ≠ rss: ${term("onnx", "onnx arena")}, file-backed pages,` +
        ` allocator retention, ${term("thread scratch", "thread scratch")}`,
    ],
  ];
}

/** "% of rss" cell with a mini bar; "—" when RSS is unknown. */
function pctCell(part: number | null, rss: number | null): string {
  if (part === null || !rss || rss <= 0) return `<td class="n">—</td>`;
  const pct = (part / rss) * 100;
  const w = Math.max(0.5, Math.min(100, pct));
  return (
    `<td class="n"><span class="mem-bar"><span class="bar" style="width:${w}%"></span></span>` +
    `${pct.toFixed(1)}%</td>`
  );
}

/** A store's summary row plus, when expanded, one dimmed row per keyspace
 * (disk-heavy first) — where the orphans and the pinned RAM show up. */
function storeRows(s: StoreMem, rss: number | null): string {
  const open = memOpen.has(s.name);
  const known = KNOWN_KS[s.name] ?? [];
  const isOrphan = (name: string) => known.length > 0 && !known.includes(name);
  const orphans = s.keyspaces.filter((k) => isOrphan(k.name)).length;
  const pinned = s.keyspaces.reduce((a, k) => a + k.pinned_filter_bytes + k.pinned_index_bytes, 0);
  const detail =
    `${term("memtable", `memtable=${bytes(s.write_buffer_bytes)}`)}` +
    ` · ${term("block cache", `cache ${bytes(s.block_cache_bytes)}/${bytes(s.block_cache_capacity)}`)}` +
    ` · ${term("pinned filters", `pinned=${bytes(pinned)}`)}` +
    ` · ${s.journal_count} journal${s.journal_count === 1 ? "" : "s"}` +
    (orphans ? ` <span class="flag">${term("orphan keyspace", `${orphans} ORPHAN`)}</span>` : "");
  const main =
    `<tr data-store="${esc(s.name)}"><td class="q"><span class="twist">${open ? "▾" : "▸"}</span> ${esc(s.name)}</td>` +
    `<td class="n">${bytes(s.ram_bytes)}</td>` +
    pctCell(s.ram_bytes, rss) +
    `<td class="n">${bytes(s.disk_bytes)}</td>` +
    `<td class="n">${s.keyspaces.length}</td>` +
    `<td>${detail}</td></tr>`;
  if (!open) return main;
  const ks = [...s.keyspaces]
    .sort((a, b) => b.disk_bytes - a.disk_bytes)
    .map(
      (k) =>
        `<tr class="ks"><td class="q">· ${esc(k.name)}</td>` +
        `<td class="n">${bytes(k.pinned_filter_bytes + k.pinned_index_bytes)}</td>` +
        `<td class="n">pinned</td>` +
        `<td class="n">${bytes(k.disk_bytes)}</td>` +
        `<td class="n">${num(k.approx_len)}</td>` +
        `<td>filters=${bytes(k.pinned_filter_bytes)} idx=${bytes(k.pinned_index_bytes)}` +
        (k.sealed_memtables ? ` · sealed=${k.sealed_memtables}` : "") +
        (isOrphan(k.name) ? ` <span class="flag">ORPHAN</span>` : "") +
        `</td></tr>`,
    )
    .join("");
  return main + ks;
}

// which store keyspaces persist each index, so its row can carry a real
// disk figure (the vectors plus the checkpointed graph blob)
const IDX_DISK: Record<string, [string, string[]]> = {
  "text hnsw (vec)": ["library.db", ["sink_vec", "sink_vec_graph"]],
  "image hnsw (imgvec)": ["images.db", ["sink_imgvec", "sink_imgvec_graph"]],
};

function renderMemory(m: MemoryBreakdown) {
  facts.memory = memoryFacts(m);
  const rss = m.rss_bytes;
  const c = m.corpus;
  const group = (label: string) => `<tr class="mem-group"><td colspan="6">${esc(label)}</td></tr>`;
  // `name`/`detail` arrive as HTML — callers escape their own data
  const row = (name: string, resident: number | null, disk: number | null, items: string, detail: string) =>
    `<tr><td class="q">${name}</td>` +
    `<td class="n">${resident === null ? "—" : bytes(resident)}</td>` +
    pctCell(resident, rss) +
    `<td class="n">${disk === null ? "—" : bytes(disk)}</td>` +
    `<td class="n">${items}</td>` +
    `<td>${detail}</td></tr>`;

  const docs =
    row(
      term("originals", "originals (pdfs)"),
      null,
      c.pdf_bytes,
      num(c.docs),
      "source documents — read at ingest, never held whole",
    ) +
    row(
      term("page scans", "page scans"),
      null,
      c.page_bytes,
      num(c.docs),
      "rendered page images, streamed to the reader per request",
    ) +
    row(term("ocr text", "ocr text"), null, c.ocr_bytes, "—", "ocr output + clean overlays") +
    row(
      term("chunk records", "chunk records"),
      null,
      c.chunk_table_bytes + c.figure_table_bytes,
      num(c.chunks + c.figures),
      `words + embeddings per chunk · ${term("emb payload", `emb=${bytes(c.emb_bytes)}`)}` +
        ` resident in the indexes below`,
    );

  const idxDisk = (name: string): number | null => {
    const map = IDX_DISK[name];
    if (!map) return null;
    const store = m.stores.find((s) => s.name === map[0]);
    if (!store) return null;
    return store.keyspaces
      .filter((k) => map[1].includes(k.name))
      .reduce((a, k) => a + k.disk_bytes, 0);
  };
  const idx = m.indexes
    .map((i) => {
      const tomb = i.slots ? Math.round(((i.slots - i.live) / i.slots) * 100) : 0;
      const detail =
        `${term("slots", `live=${num(i.live)} slots=${num(i.slots)}`)}` +
        (tomb ? ` (${tomb}% tombstoned)` : "") +
        ` · ${term("per-vector bytes", `${num(i.per_vector_bytes)} B/vec`)}` +
        ` · maps=${bytes(i.map_bytes)} (est.)` +
        (i.stale ? ` <span class="flag">${term("stale", "STALE")}</span>` : "");
      return row(esc(i.name), i.bytes, idxDisk(i.name), num(i.live), detail);
    })
    .join("");

  const caches = m.caches
    .map((cc) => {
      const doclen = cc.name.includes("doclen");
      const detail = cc.warmed
        ? doclen
          ? term("doclen cache", "warm — one entry per chunk")
          : ""
        : `${term("doclen cache", "cold")} — warms on first search`;
      return row(esc(cc.name), cc.bytes, null, num(cc.entries), detail);
    })
    .join("");

  const models = m.models
    .map((x) =>
      row(
        esc(x.name),
        x.bytes,
        x.bytes === null ? c.model_dir_bytes : null, // clip's files live in data/models
        "—",
        x.bytes === null
          ? `${term("onnx", "opaque")} — visible only in rss`
          : term("ese weights", esc(x.residency)),
      ),
    )
    .join("");

  const remainder =
    m.unaccounted_bytes === null
      ? ""
      : group("remainder") +
        row(
          term("unaccounted", "unaccounted"),
          m.unaccounted_bytes,
          null,
          "—",
          "rss − accounted: onnx arena, page cache, allocator retention, thread scratch",
        );

  $memoryBody.innerHTML =
    `<table><thead><tr>` +
    `<th>component</th><th>resident (est.)</th><th>% of rss</th><th>disk</th><th>items</th><th>detail</th>` +
    `</tr></thead><tbody>` +
    group("documents") +
    docs +
    group("indexes") +
    idx +
    group("caches") +
    caches +
    group("models") +
    models +
    group("stores") +
    m.stores.map((s) => storeRows(s, rss)).join("") +
    remainder +
    `</tbody></table>`;

  for (const tr of $memoryBody.querySelectorAll<HTMLElement>("tr[data-store]")) {
    tr.addEventListener("click", () => {
      const name = tr.dataset.store!;
      if (memOpen.has(name)) memOpen.delete(name);
      else memOpen.add(name);
      lastMemory = ""; // force re-render on next poll
      renderMemory(m);
    });
  }
}

// --- tab switching ----------------------------------------------------------

$tabs.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (btn?.dataset.tab) setPerfTab(btn.dataset.tab as Tab);
});

// --- term popover -----------------------------------------------------------
//
// Fixed-position rather than anchored in flow: the head is sticky and #perf
// scrolls under it, so anything absolutely positioned inside would drift.

const POP_DELAY_MS = 120;
let popTimer = 0;

/** True if a popover was showing (so Escape can close it instead of the
 * whole overlay). */
export function dismissPerfPopover(): boolean {
  window.clearTimeout(popTimer);
  if ($pop.hidden) return false;
  $pop.hidden = true;
  return true;
}

function showPopover(el: HTMLElement) {
  const name = el.dataset.term!;
  const g = GLOSS[name];
  if (!g) return;
  const live = evidence(name, { searches, ingest, agent: agentLog() });
  $pop.innerHTML =
    `<div class="pop-head">${esc(el.textContent?.trim() ?? name)}</div>` +
    `<div class="pop-what">${esc(g.what)}</div>` +
    (g.range ? `<div class="pop-range">${esc(g.range)}</div>` : "") +
    (live ? `<div class="pop-live"><i>live</i> ${esc(live)}</div>` : "");
  $pop.hidden = false;

  // clamp to the viewport: these sit in a wrapping header, so a term near
  // the right edge is common
  const r = el.getBoundingClientRect();
  const w = $pop.offsetWidth;
  $pop.style.left = `${Math.max(4, Math.min(r.left, window.innerWidth - w - 4))}px`;
  $pop.style.top = `${r.bottom + 4}px`;
}

for (const ev of ["mouseover", "focusin"] as const) {
  $perf.addEventListener(ev, (e) => {
    const el = (e.target as HTMLElement).closest<HTMLElement>("[data-term]");
    window.clearTimeout(popTimer);
    if (!el) {
      $pop.hidden = true;
      return;
    }
    popTimer = window.setTimeout(() => showPopover(el), POP_DELAY_MS);
  });
}
$perf.addEventListener("mouseleave", () => dismissPerfPopover());
$perf.addEventListener("scroll", () => dismissPerfPopover());
