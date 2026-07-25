// Hidden corpus-atlas view (Cmd+/): a semantic map of every passage in the
// library, with the recurring cross-book themes down the left rail. Picking
// a theme lights its cluster on the map (highlighter, like everything else
// in The Stacks) and opens the side panel with that theme's throughline —
// a trail of passages hopping book to book — plus exemplar quotes. Trail
// steps and quotes deep-link into the reader.
//
// The viewport is locked: the cloud is always fitted whole (per-axis
// stretch — PCA axes are abstract, so filling the frame is free spread).
// Theme titles come from the librarian sidecar at build time; the
// distinctive terms render as badges either way.
//
// Data comes whole from the hosts' atlas endpoint (web: /api/atlas,
// desktop: the `atlas` command). While the sidecar is being built the view
// polls and shows the build stage; once ready it renders from the cached
// payload and stops talking to the host entirely.

import { findPoint, fitView, hitTest, toScreen, trailFor } from "./atlas-model";
import type { View } from "./atlas-model";
import { esc } from "./perf-fmt";
import { desktop } from "./state";
import { isTauri } from "./transport";
import type { Atlas, AtlasResponse, AtlasTheme } from "./types";

const $atlas = document.getElementById("atlas")!;
const $status = document.getElementById("atlas-status")!;
const $refresh = document.getElementById("atlas-refresh") as HTMLButtonElement;
const $rail = document.getElementById("atlas-rail")!;
const $stage = document.getElementById("atlas-stage")!;
const $map = document.getElementById("atlas-map") as HTMLCanvasElement;
const $tip = document.getElementById("atlas-tip")!;
const $panel = document.getElementById("atlas-panel")!;
const ctx = $map.getContext("2d")!;

const POLL_MS = 2000;
const PAD = 40;
const HIT_RADIUS = 12;
const DOT_R = 2.5;

let atlas: Atlas | null = null;
let sel = -1; // selected theme id
let view: View = { sx: 1, sy: 1, tx: 0, ty: 0 };
let hover: number | null = null;
let pollTimer = 0;

async function fetchAtlas(refresh?: boolean): Promise<AtlasResponse> {
  if (isTauri() && desktop) return desktop.atlas(refresh);
  const q = refresh ? "?refresh=true" : "";
  const r = await fetch(`/api/atlas${q}`);
  if (!r.ok) throw new Error((await r.text()) || `${r.status}`);
  return r.json();
}

export function atlasOpen(): boolean {
  return !$atlas.hidden;
}

export function toggleAtlas() {
  if (atlasOpen()) closeAtlas();
  else openAtlas();
}

/** Escape's innermost layer: drop the theme selection if one is active.
 * Returns whether it consumed the key. */
export function dismissAtlasSelection(): boolean {
  if (sel === -1) return false;
  select(-1);
  return true;
}

function openAtlas() {
  $atlas.hidden = false;
  // the layer covers everything; stranded focus would swallow keys
  (document.activeElement as HTMLElement | null)?.blur();
  if (!atlas) $status.textContent = "loading…";
  void load(false);
}

function closeAtlas() {
  $atlas.hidden = true;
  window.clearTimeout(pollTimer);
  $tip.hidden = true;
}

async function load(refresh: boolean) {
  window.clearTimeout(pollTimer);
  let resp: AtlasResponse;
  try {
    resp = await fetchAtlas(refresh);
  } catch (e) {
    $status.textContent = `atlas endpoint unreachable: ${e instanceof Error ? e.message : e}`;
    // transient failures happen (e.g. the engine is still starting right
    // after app launch) — retry while the view stays open rather than
    // sticking on an empty layer
    if (atlasOpen()) pollTimer = window.setTimeout(() => void load(false), 3000);
    return;
  }
  if (resp.status === "building") {
    const secs = Math.max(0, Math.round((Date.now() - resp.since) / 1000));
    $status.textContent = `building atlas… ${resp.stage} · ${secs}s`;
    if (atlasOpen()) pollTimer = window.setTimeout(() => void load(false), POLL_MS);
    return;
  }
  const first = atlas === null || atlas.built_ms !== resp.atlas.built_ms;
  atlas = resp.atlas;
  $status.textContent =
    `${atlas.points.length.toLocaleString()} passages · ${atlas.themes.length} themes · ` +
    `built in ${(atlas.build_ms / 1000).toFixed(1)}s` +
    (resp.rebuilding ? " · rebuilding…" : "");
  // a manual rebuild runs behind the still-fresh sidecar: keep polling
  // until the new build lands (built_ms changes)
  if (resp.rebuilding && atlasOpen()) {
    pollTimer = window.setTimeout(() => void load(false), POLL_MS);
  }
  if (first) {
    if (!atlas.themes.some((t) => t.id === sel)) sel = -1;
    renderRail();
    renderPanel();
    refit();
  }
  draw();
}

function refit() {
  if (!atlas) return;
  view = fitView(atlas.points, $stage.clientWidth, $stage.clientHeight, PAD);
}

function select(id: number) {
  sel = sel === id ? -1 : id;
  for (const b of $rail.querySelectorAll<HTMLElement>("button[data-theme]")) {
    b.classList.toggle("on", Number(b.dataset.theme) === sel);
  }
  renderPanel();
  draw();
}

// --- rail + panel ----------------------------------------------------------

const badges = (terms: string[], n: number) =>
  `<span class="a-badges">${terms
    .slice(0, n)
    .map((t) => `<span class="a-badge">${esc(t)}</span>`)
    .join("")}</span>`;

const themeTitle = (t: AtlasTheme) => t.title ?? t.terms.slice(0, 2).join(" · ");

function renderRail() {
  if (!atlas) return;
  const themes = [...atlas.themes].sort((a, b) => b.ndocs - a.ndocs || b.size - a.size);
  $rail.innerHTML =
    `<div class="a-rail-head">recurring themes<span>${themes.length}</span></div>` +
    themes
      .map(
        (t) => `<button data-theme="${t.id}" class="${t.id === sel ? "on" : ""}">
          <span class="a-title">${esc(themeTitle(t))}</span>
          ${badges(t.terms, 3)}
          <span class="a-count">${t.size} passages · ${t.ndocs} books</span>
        </button>`,
      )
      .join("");
}

function themeById(id: number): AtlasTheme | null {
  return atlas?.themes.find((t) => t.id === id) ?? null;
}

function renderPanel() {
  const theme = themeById(sel);
  if (!atlas || !theme) {
    $panel.hidden = true;
    $panel.innerHTML = "";
    return;
  }
  const doc = (d: number) => atlas!.docs[d];
  const trail = trailFor(atlas, theme.id);
  const steps = trail
    ? trail.steps
        .map(
          (s, i) => `<a class="a-step" href="#/read/${encodeURIComponent(doc(s.d).id)}?p=${s.p}">
            <span class="a-n">${i + 1}</span>
            <span class="a-src">${esc(doc(s.d).title)} · p.${s.p}</span>
            <q>${esc(s.s)}</q>
            ${i > 0 ? `<span class="a-sim">cos ${s.sim.toFixed(2)}</span>` : ""}
          </a>`,
        )
        .join("")
    : `<div class="a-none">no trail long enough for this theme</div>`;
  const quotes = theme.top
    .map(
      (q) => `<a class="a-quote" href="#/read/${encodeURIComponent(doc(q.d).id)}?p=${q.p}">
        <span class="a-src">${esc(doc(q.d).title)} · p.${q.p}</span>
        <q>${esc(q.s)}</q>
      </a>`,
    )
    .join("");
  $panel.innerHTML = `
    <div class="a-title a-panel-title">${esc(themeTitle(theme))}</div>
    ${badges(theme.terms, 9)}
    <div class="a-count">${theme.size} passages · ${theme.ndocs} books</div>
    <div class="a-sect">throughline — one passage per book</div>
    ${steps}
    <div class="a-sect">closest to the theme's center</div>
    ${quotes}`;
  $panel.hidden = false;
}

// a deep link is a navigation: close the layer so the reader is visible
$panel.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest("a")) closeAtlas();
});

$rail.addEventListener("click", (e) => {
  const b = (e.target as HTMLElement).closest<HTMLElement>("button[data-theme]");
  if (b) select(Number(b.dataset.theme));
});

$refresh.addEventListener("click", () => {
  $status.textContent = "rebuilding…";
  void load(true);
});

// --- canvas ----------------------------------------------------------------

function css(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function draw() {
  if (!atlas) return;
  const w = $stage.clientWidth;
  const h = $stage.clientHeight;
  const dpr = window.devicePixelRatio || 1;
  if ($map.width !== Math.round(w * dpr) || $map.height !== Math.round(h * dpr)) {
    $map.width = Math.round(w * dpr);
    $map.height = Math.round(h * dpr);
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const ink = css("--ink");
  const hl = css("--hl");
  for (const p of atlas.points) {
    const [x, y] = toScreen(view, p.x, p.y);
    const lit = sel >= 0 && p.c === sel;
    ctx.globalAlpha = sel >= 0 ? (lit ? 0.95 : 0.07) : 0.25 + 0.6 * Math.min(1, p.e / 8);
    ctx.fillStyle = lit ? hl : ink;
    ctx.beginPath();
    ctx.arc(x, y, lit ? DOT_R + 1 : DOT_R, 0, 6.2832);
    ctx.fill();
  }
  ctx.globalAlpha = 1;
  if (sel >= 0) drawTrail(ink);
}

/** The selected theme's throughline as a numbered polyline over the map. */
function drawTrail(ink: string) {
  if (!atlas) return;
  const trail = trailFor(atlas, sel);
  if (!trail) return;
  const knots: [number, number][] = [];
  for (const s of trail.steps) {
    const i = findPoint(atlas.points, s.d, s.p, sel);
    if (i !== null) knots.push(toScreen(view, atlas.points[i].x, atlas.points[i].y));
  }
  if (knots.length < 2) return;
  ctx.strokeStyle = ink;
  ctx.globalAlpha = 0.6;
  ctx.lineWidth = 1.25;
  ctx.setLineDash([4, 3]);
  ctx.beginPath();
  knots.forEach(([x, y], i) => (i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y)));
  ctx.stroke();
  ctx.setLineDash([]);
  ctx.globalAlpha = 1;
  ctx.font = `9px ${css("--font-mono")}`;
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  knots.forEach(([x, y], i) => {
    ctx.fillStyle = css("--bg");
    ctx.beginPath();
    ctx.arc(x, y, 7, 0, 6.2832);
    ctx.fill();
    ctx.strokeStyle = ink;
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.fillStyle = ink;
    ctx.fillText(String(i + 1), x, y + 0.5);
  });
}

// --- map interaction: hover + click (viewport is locked, no pan/zoom) ------

$map.addEventListener("pointermove", (e) => {
  if (!atlas) return;
  const rect = $map.getBoundingClientRect();
  const mx = e.clientX - rect.left;
  const my = e.clientY - rect.top;
  const i = hitTest(
    atlas.points,
    view,
    mx,
    my,
    HIT_RADIUS,
    sel >= 0 ? (p) => p.c === sel : undefined,
  );
  if (i === hover) return;
  hover = i;
  if (i === null) {
    $tip.hidden = true;
    return;
  }
  const p = atlas.points[i];
  const doc = atlas.docs[p.d];
  const bridge = p.e >= 5 ? `<span class="a-sim">bridges ${p.e} other books</span>` : "";
  $tip.innerHTML = `<span class="a-src">${esc(doc.title)} · p.${p.p}</span><q>${esc(p.s)}</q>${bridge}`;
  $tip.hidden = false;
  const tw = $tip.offsetWidth;
  const th = $tip.offsetHeight;
  $tip.style.left = `${Math.min(mx + 14, $stage.clientWidth - tw - 8)}px`;
  $tip.style.top = `${Math.min(my + 12, $stage.clientHeight - th - 8)}px`;
});

$map.addEventListener("pointerleave", () => {
  hover = null;
  $tip.hidden = true;
});

// map click: a dot in a theme selects that theme; empty space deselects
$map.addEventListener("click", (e) => {
  if (!atlas) return;
  const rect = $map.getBoundingClientRect();
  const i = hitTest(atlas.points, view, e.clientX - rect.left, e.clientY - rect.top, HIT_RADIUS);
  if (i !== null && atlas.points[i].c >= 0) select(atlas.points[i].c);
  else if (i === null && sel >= 0) select(-1);
});

// --- environment: resize, theme flips --------------------------------------

new ResizeObserver(() => {
  // fires on open too (the section unhides); refit keeps the cloud whole
  if (atlasOpen()) {
    refit();
    draw();
  }
}).observe($stage);

matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (atlasOpen()) draw();
});
