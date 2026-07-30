// The perf view's Agent tab: one row per librarian chat turn, expanding to
// the whole provenance chain — what the router planned, which tools ran when,
// which pages the model actually saw, and what it said. Data comes from the
// browser-side ring in agent-log.ts (the sidecar keeps no history), so every
// duration is an arrival time in this tab; the header's CLOCK fact says so.

import { esc, hhmmss, median, ms, opt, term } from "./perf-fmt";
import type { AgentTool, AgentTurn } from "./types";

// id of the manually expanded turn; null = follow the newest. Deliberately
// separate from the search tab's pin — the auto-expand rules differ.
let pinned: number | null = null;

/** Renders the pane and returns the header facts for this tab. `onJump`
 * takes a tool call to its record in the search ring. */
export function renderAgent(
  el: HTMLElement,
  turns: AgentTurn[],
  onJump: (ts: number) => void,
): [string, string][] {
  if (!turns.length) {
    el.textContent =
      "no agent turns yet — ask the librarian (the chat panel), then reopen. " +
      "Turns are captured in this tab only and are lost on reload.";
    return facts(turns);
  }
  const expand = pinned ?? turns[0].id;
  const rows = turns
    .map((t) => {
      const open = t.id === expand;
      return (
        `<tr class="${open ? "sel" : ""}" data-turn="${t.id}">` +
        `<td><span class="twist">${open ? "▾" : "▸"}</span> ${hhmmss(t.ts_ms)}</td>` +
        `<td class="q" title="${esc(t.prompt)}">${esc(t.prompt)}</td>` +
        `<td>${esc(approachLabel(t))}</td>` +
        `<td class="stages">${esc(chain(t.tools)) || "—"}</td>` +
        `<td class="n">${opt(t.plan?.ms, ms)}</td>` +
        `<td class="n">${opt(t.ttft_ms, ms)}</td>` +
        `<td class="n">${opt(t.stream_ms, ms)}</td>` +
        `<td class="n">${opt(t.total_ms, ms)}</td>` +
        `<td class="n">${t.deltas}</td>` +
        `<td class="n">${rate(t)}</td>` +
        `<td class="n">${t.chars}</td>` +
        `<td>${status(t)}</td>` +
        `</tr>` +
        (open ? `<tr class="detail"><td colspan="12">${detail(t)}</td></tr>` : "")
      );
    })
    .join("");
  el.innerHTML =
    `<table><thead><tr>` +
    `<th>time</th><th>prompt</th><th>approach</th><th>tools</th>` +
    `<th>plan</th><th>ttft</th><th>stream</th><th>total</th>` +
    `<th>Δtok</th><th>tok/s</th><th>chars</th><th>status</th>` +
    `</tr></thead><tbody>${rows}</tbody></table>`;

  for (const tr of el.querySelectorAll<HTMLElement>("tr[data-turn]")) {
    tr.addEventListener("click", () => {
      const id = Number(tr.dataset.turn);
      pinned = pinned === id ? null : id;
      renderAgent(el, turns, onJump);
    });
  }
  // the jump buttons live inside an expanded row, whose parent <tr> toggles
  // the pin — without stopPropagation a jump would also collapse the row
  for (const b of el.querySelectorAll<HTMLElement>("button[data-perf-ts]")) {
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      onJump(Number(b.dataset.perfTs));
    });
  }
  return facts(turns);
}

function approachLabel(t: AgentTurn): string {
  if (!t.plan) return "—";
  return t.plan.approach + (t.plan.collection ? `/col:${t.plan.collection}` : "");
}

/** search_library×2→read_pages — consecutive repeats collapse. */
function chain(tools: AgentTool[]): string {
  const out: [string, number][] = [];
  for (const c of tools) {
    const last = out[out.length - 1];
    if (last && last[0] === c.name) last[1]++;
    else out.push([c.name, 1]);
  }
  return out.map(([n, k]) => (k > 1 ? `${n}×${k}` : n)).join("→");
}

function rate(t: AgentTurn): string {
  if (!t.stream_ms || !t.deltas) return "—";
  return (t.deltas / (t.stream_ms / 1000)).toFixed(1);
}

function status(t: AgentTurn): string {
  switch (t.outcome) {
    case "streaming":
      return "streaming…";
    case "cancelled":
      return `<span class="flag">CANCELLED</span>`;
    case "error":
      return `<span class="flag">ERR</span>`;
    default:
      return "done";
  }
}

/** Expanded turn: an offset timeline plus the provenance columns. */
function detail(t: AgentTurn): string {
  return (
    `<div class="waterfall wide">${timeline(t)}</div>` +
    `<div class="prov-cols">` +
    `<div class="prov-col">${planCol(t)}</div>` +
    `<div class="prov-col">${toolsCol(t)}</div>` +
    `<div class="prov-col">${hitsCol(t)}</div>` +
    `</div>` +
    answerCol(t)
  );
}

/** Offset bars over the turn's wall clock — where each phase sat, not a
 * stack of durations (tools and streaming don't tile the timeline). */
function timeline(t: AgentTurn): string {
  const span = Math.max(t.total_ms ?? 0, ...t.tools.map((c) => c.at_ms + (c.ms ?? 0)), 1);
  const bar = (label: string, from: number, len: number | null) => {
    const open = len === null;
    const width = Math.max(0.5, ((open ? span - from : len) / span) * 100);
    return (
      `<div class="bar-row"><span class="bar-label">${esc(label)}</span>` +
      `<span class="bar-track">` +
      `<span class="bar${open ? " open" : ""}" style="margin-left:${(from / span) * 100}%;width:${width}%"></span>` +
      `</span><span class="bar-us">${open ? "—" : ms(len)}</span></div>`
    );
  };
  const rows: string[] = [];
  if (t.plan) rows.push(bar("plan", 0, t.plan.ms));
  for (const c of t.tools) rows.push(bar(c.name, c.at_ms, c.ms));
  if (t.ttft_ms !== null) rows.push(bar("stream", t.ttft_ms, (t.total_ms ?? span) - t.ttft_ms));
  return rows.join("");
}

function planCol(t: AgentTurn): string {
  const title = `<div class="prov-title">plan (schema-constrained pre-pass)</div>`;
  if (!t.plan) {
    return `${title}<div class="perf-note">no plan event — planless turn (pre-pass failed, or an older sidecar)</div>`;
  }
  const kv: [string, string][] = [
    ["intent", t.plan.intent],
    ["approach", t.plan.approach],
    ["query", t.plan.query || "—"],
    ["collection", t.plan.collection || "(all)"],
    ["ms", ms(t.plan.ms)],
  ];
  return (
    title +
    `<table><tbody>` +
    kv.map(([k, v]) => `<tr><td class="k">${k}</td><td>${esc(v)}</td></tr>`).join("") +
    `</tbody></table>`
  );
}

function toolsCol(t: AgentTurn): string {
  const title = `<div class="prov-title">tool calls (conf/cov/bm25 = what retrieval told the model)</div>`;
  if (!t.tools.length) {
    return `${title}<div class="perf-note">no tools — the model answered from the conversation alone</div>`;
  }
  const rows = t.tools
    .map((c, i) => {
      const weak =
        c.confidence === "none"
          ? ` <span class="flag">NONE</span>`
          : c.confidence === "weak"
            ? ` <span class="flag">WEAK</span>`
            : "";
      const args = Object.entries(c.args)
        .map(([k, v]) => `${k}=${String(v)}`)
        .join(" ");
      const jump = c.perf_ts
        ? `<button class="jump divot quiet" data-perf-ts="${c.perf_ts}" title="show this search in the search tab">↗</button>`
        : "";
      return (
        `<tr><td class="n">${i}</td><td>${esc(c.name)}${jump}</td>` +
        `<td class="q" title="${esc(args)}">${esc(args) || "—"}</td>` +
        `<td class="n">${opt(c.ms, ms)}</td>` +
        `<td>${esc(c.summary ?? "—")}${weak}</td>` +
        `<td class="n">${c.hits}</td>` +
        `<td class="n">${opt(c.coverage, (n) => n.toFixed(2))}</td>` +
        `<td class="n">${opt(c.top_bm25, (n) => n.toFixed(2))}</td></tr>`
      );
    })
    .join("");
  return (
    title +
    `<table><thead><tr><th>#</th><th>tool</th><th>args</th><th>ms</th>` +
    `<th>result</th><th>hits</th><th>cov</th><th>bm25</th></tr></thead>` +
    `<tbody>${rows}</tbody></table>`
  );
}

function hitsCol(t: AgentTurn): string {
  const title = `<div class="prov-title">pages the model saw (the citation ground truth)</div>`;
  const rows = t.tools.flatMap((c) => c.chips.map((h) => [c.name, h] as const));
  if (!rows.length) return `${title}<div class="perf-note">none</div>`;
  return (
    title +
    `<table><thead><tr><th>#</th><th>via</th><th>doc p.</th></tr></thead><tbody>` +
    rows
      .map(
        ([tool, h], i) =>
          `<tr><td class="n">${i}</td><td>${esc(tool)}</td>` +
          `<td>${esc(h.doc)} p.${h.page}</td></tr>`,
      )
      .join("") +
    `</tbody></table>`
  );
}

function answerCol(t: AgentTurn): string {
  const body = t.outcome === "error" ? (t.error ?? "unknown error") : t.content;
  if (!body) return "";
  return (
    `<div class="prov-title">${t.outcome === "error" ? "error" : "answer"}</div>` +
    `<div class="agent-answer">${esc(body)}</div>`
  );
}

function facts(turns: AgentTurn[]): [string, string][] {
  const newest = turns[0];
  const ttfts = turns.map((t) => t.ttft_ms).filter((n): n is number => n !== null);
  const med = median(ttfts);
  return [
    [
      "sidecar",
      `librarian · Apple Foundation Models, on-device · model=${
        newest?.model ? esc(newest.model) : "—"
      }`,
    ],
    [
      "transport",
      newest ? `${esc(newest.transport)} · conv=${esc(newest.conv.slice(0, 8))}` : "—",
    ],
    [
      "capture",
      `browser ring · this page load only · ${turns.length} turns` +
        (med === null ? "" : ` · median ${term("ttft", "ttft")} ${ms(med)}`),
    ],
    [
      "clock",
      `${term("plan", "plan")}/tool/${term("ttft", "ttft")} measured at event arrival in this tab` +
        ` · ${term("sidecar_ms", "sidecar_ms")} = model turn only (excludes plan + host tool work)` +
        ` · Δtok counts stream events, not model tokens` +
        ` · ${term("confidence", "conf")} = what retrieval told the model`,
    ],
  ];
}
