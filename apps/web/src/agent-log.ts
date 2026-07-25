// Client-side provenance ring for librarian chat turns: what the router
// planned, which tools ran and for how long, which pages the model actually
// saw, and what came back. The perf view's Agent tab reads it.
//
// Why the browser and not a server ring like perf::SEARCH_LOG: the sidecar
// keeps no history, and its events already pass through this tab on both
// hosts. So every duration here is measured at *event arrival*, not inside
// the model — the Agent tab's header says so, because the difference is real
// (relay overhead lands in these numbers, and `sidecar_ms` is the check).
//
// The ring dies with the page. Turn volume is low and the write churn of
// persisting streamed events isn't worth it; the empty state says so.

import type { AgentTurn, ChatEvent } from "./types";

export const AGENT_LOG_CAP = 50;

// newest first, matching perf::search_log()
let log: AgentTurn[] = [];
// bumped on every mutation; the perf view polls this instead of diffing JSON
let version = 0;
let nextId = 1;

export function agentLog(): AgentTurn[] {
  return log;
}

export function agentVersion(): number {
  return version;
}

export function clearAgentLog(): void {
  log = [];
  nextId = 1;
  version++;
}

/** Capture handle for one turn; chat.ts holds it for the turn's lifetime. */
export type TurnRecorder = {
  event(ev: ChatEvent): void;
  finish(o: { aborted: boolean }): void;
};

export function startTurn(
  prompt: string,
  meta: { conv: string; transport: "sse" | "tauri" },
): TurnRecorder {
  const t0 = performance.now();
  const turn: AgentTurn = {
    id: nextId++,
    ts_ms: Date.now(),
    prompt,
    conv: meta.conv,
    transport: meta.transport,
    outcome: "streaming",
    error: null,
    plan: null,
    tools: [],
    ttft_ms: null,
    deltas: 0,
    chars: 0,
    stream_ms: null,
    total_ms: null,
    sidecar_ms: null,
    model: null,
    content: "",
  };
  // pushed up front so a turn already streaming is visible the moment the
  // overlay opens, not only once it finishes
  log.unshift(turn);
  log.length = Math.min(log.length, AGENT_LOG_CAP);
  version++;

  let firstTokenAt: number | null = null;
  let lastTokenAt: number | null = null;
  const since = () => performance.now() - t0;

  /** The newest still-open call, preferring a name match — the sidecar pairs
   * started/done in order, but host-side prefetch can emit a lone `done`. */
  const openCall = (name: string) => {
    for (let i = turn.tools.length - 1; i >= 0; i--) {
      if (turn.tools[i].ms === null && turn.tools[i].name === name) return turn.tools[i];
    }
    for (let i = turn.tools.length - 1; i >= 0; i--) {
      if (turn.tools[i].ms === null) return turn.tools[i];
    }
    return null;
  };

  const terminal = (outcome: AgentTurn["outcome"]) => {
    if (turn.outcome !== "streaming") return false;
    turn.outcome = outcome;
    turn.total_ms = since();
    return true;
  };

  return {
    event(ev: ChatEvent) {
      switch (ev.e) {
        case "plan":
          // first wins: one pre-pass per turn
          turn.plan ??= {
            intent: ev.intent,
            approach: ev.approach,
            query: ev.query,
            collection: ev.collection,
            ms: since(),
          };
          break;
        case "tool": {
          if (ev.status === "started") {
            turn.tools.push({
              name: ev.name,
              args: ev.args ?? {},
              at_ms: since(),
              ms: null,
              summary: null,
              hits: 0,
              docs: [],
              chips: [],
            });
            break;
          }
          let call = openCall(ev.name);
          if (!call) {
            // unpaired done: keep the evidence rather than drop it
            call = {
              name: ev.name,
              args: ev.args ?? {},
              at_ms: since(),
              ms: null,
              summary: null,
              hits: 0,
              docs: [],
              chips: [],
            };
            turn.tools.push(call);
          }
          call.ms = Math.max(0, since() - call.at_ms);
          call.summary = ev.summary ?? null;
          call.chips = ev.hits ?? [];
          call.hits = call.chips.length;
          call.docs = [...new Set(call.chips.map((h) => h.doc))];
          if (ev.confidence !== undefined) call.confidence = ev.confidence;
          if (ev.coverage !== undefined) call.coverage = ev.coverage;
          if (ev.top_bm25 !== undefined) call.top_bm25 = ev.top_bm25;
          if (ev.perf_ts !== undefined) call.perf_ts = ev.perf_ts;
          break;
        }
        case "token":
          firstTokenAt ??= since();
          turn.ttft_ms ??= firstTokenAt;
          lastTokenAt = since();
          turn.deltas++;
          turn.content = ev.replace ? ev.text : turn.content + ev.text;
          turn.chars = turn.content.length;
          break;
        case "done":
          turn.content = ev.content;
          turn.chars = ev.content.length;
          turn.sidecar_ms = ev.ms;
          turn.model = ev.model ?? null;
          if (firstTokenAt !== null && lastTokenAt !== null) {
            turn.stream_ms = lastTokenAt - firstTokenAt;
          }
          terminal("done");
          break;
        case "cancelled":
          terminal("cancelled");
          break;
        case "error":
          if (terminal("error")) turn.error = ev.message;
          break;
        default:
          // `ready` and anything the sidecar grows later: not our business
          return;
      }
      version++;
    },
    finish({ aborted }) {
      // a terminal event already landed — never downgrade it
      if (turn.outcome === "streaming") terminal(aborted ? "cancelled" : "done");
      turn.total_ms ??= since();
      version++;
    },
  };
}
