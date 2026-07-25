// These pin the ring's *arrival-time* semantics — nothing else documents
// them, and they are what the Agent tab's numbers mean. Durations come from
// performance.now() at the moment an event reaches this tab, so the clock is
// stubbed and stepped explicitly rather than slept on.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AGENT_LOG_CAP, agentLog, agentVersion, clearAgentLog, startTurn } from "./agent-log";
import type { ChatEvent } from "./types";

let now = 0;

beforeEach(() => {
  now = 0;
  vi.spyOn(performance, "now").mockImplementation(() => now);
  clearAgentLog();
});

afterEach(() => {
  vi.restoreAllMocks();
});

const at = (t: number) => {
  now = t;
};

const meta = { conv: "c1", transport: "sse" as const };

describe("startTurn", () => {
  it("captures a plan, tool chain, stream and completion", () => {
    const rec = startTurn("who wrote this", meta);
    at(120);
    rec.event({ e: "plan", intent: "lookup", approach: "search", query: "desserts", collection: "" });
    at(200);
    rec.event({ e: "tool", name: "search_library", status: "started", args: { query: "desserts" } });
    at(650);
    rec.event({
      e: "tool",
      name: "search_library",
      status: "done",
      summary: "6 hits",
      hits: [
        { doc: "gastronomy", title: "Gastronomy", page: 245 },
        { doc: "gastronomy", title: "Gastronomy", page: 11 },
      ],
    });
    at(700);
    rec.event({ e: "tool", name: "read_pages", status: "started", args: { doc: "gastronomy", from: 245 } });
    at(900);
    rec.event({ e: "tool", name: "read_pages", status: "done", summary: "gastronomy p.245", hits: [] });
    at(1000);
    rec.event({ e: "token", text: "Cre" });
    at(1400);
    rec.event({ e: "token", text: "me caramel." });
    at(1500);
    rec.event({ e: "done", content: "Creme caramel.", ms: 1310 });
    rec.finish({ aborted: false });

    const [t] = agentLog();
    expect(t.plan).toEqual({
      intent: "lookup",
      approach: "search",
      query: "desserts",
      collection: "",
      ms: 120,
    });
    expect(t.tools.map((c) => [c.name, c.at_ms, c.ms])).toEqual([
      ["search_library", 200, 450],
      ["read_pages", 700, 200],
    ]);
    expect(t.tools[0].hits).toBe(2);
    expect(t.tools[0].docs).toEqual(["gastronomy"]); // two pages, one doc
    expect(t.ttft_ms).toBe(1000);
    expect(t.stream_ms).toBe(400);
    expect(t.deltas).toBe(2);
    expect(t.chars).toBe("Creme caramel.".length);
    expect(t.sidecar_ms).toBe(1310);
    expect(t.total_ms).toBe(1500);
    expect(t.outcome).toBe("done");
  });

  it("marks an aborted turn cancelled and keeps the partial answer", () => {
    const rec = startTurn("long one", meta);
    at(300);
    rec.event({ e: "token", text: "partial" });
    at(800);
    rec.finish({ aborted: true });

    const [t] = agentLog();
    expect(t.outcome).toBe("cancelled");
    expect(t.total_ms).toBe(800);
    expect(t.content).toBe("partial");
  });

  it("keeps a transport error over the finish outcome", () => {
    const rec = startTurn("boom", meta);
    at(50);
    rec.event({ e: "error", message: "chat: 500" });
    at(60);
    rec.finish({ aborted: false });

    const [t] = agentLog();
    expect(t.outcome).toBe("error");
    expect(t.error).toBe("chat: 500");
    expect(t.total_ms).toBe(50);
  });

  it("survives unpaired tool events", () => {
    const rec = startTurn("odd", meta);
    at(100);
    // done with no started — host-side prefetch emits these
    rec.event({ e: "tool", name: "library_overview", status: "done", summary: "63 books" });
    at(200);
    // started with no done — turn ends mid-call
    rec.event({ e: "tool", name: "search_library", status: "started" });
    at(300);
    rec.finish({ aborted: true });

    const [t] = agentLog();
    expect(t.tools).toHaveLength(2);
    expect(t.tools[0].summary).toBe("63 books");
    expect(t.tools[1].ms).toBeNull();
  });

  it("ignores events it does not model", () => {
    const rec = startTurn("hi", meta);
    expect(() => rec.event({ e: "ready" } as unknown as ChatEvent)).not.toThrow();
    expect(agentLog()[0].outcome).toBe("streaming");
  });

  it("carries retrieval quality and the search-ring link when present", () => {
    const rec = startTurn("rhodium", meta);
    rec.event({ e: "tool", name: "search_library", status: "started" });
    at(400);
    rec.event({
      e: "tool",
      name: "search_library",
      status: "done",
      summary: "2 hits · weak",
      hits: [{ doc: "catalog", title: null, page: 8 }],
      confidence: "weak",
      coverage: 0.5,
      top_bm25: 3.2,
      perf_ts: 1_700_000_000_000,
    });

    const [c] = agentLog()[0].tools;
    expect(c.confidence).toBe("weak");
    expect(c.coverage).toBe(0.5);
    expect(c.top_bm25).toBe(3.2);
    expect(c.perf_ts).toBe(1_700_000_000_000);
  });
});

describe("the ring", () => {
  it("caps at AGENT_LOG_CAP, newest first, and bumps the version", () => {
    const v0 = agentVersion();
    for (let i = 0; i < AGENT_LOG_CAP + 3; i++) startTurn(`q${i}`, meta);

    const log = agentLog();
    expect(log).toHaveLength(AGENT_LOG_CAP);
    expect(log[0].prompt).toBe(`q${AGENT_LOG_CAP + 2}`);
    expect(agentVersion()).toBeGreaterThan(v0);
  });
});
