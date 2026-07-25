export type SnippetWord = { t: string; m: boolean };

/** Note context on a `kind: "card"` hit. `doc`/`page` are the real
 * page the card's first mark lives on — present only on mark-cards. */
export type CardMeta = {
  id: string;
  title: string;
  doc?: string;
  page?: number;
};

export type WireHit = {
  kind: "text" | "image" | "card";
  score: number;
  doc: string;
  page: number;
  idx: number;
  img: string;
  snippet: SnippetWord[];
  boxes: [number, number, number, number][];
  crop: [number, number, number, number];
  card?: CardMeta;
};
export type WireResponse = { seq: number; phase: string; us: number; hits: WireHit[] };

export type QueryMsg = {
  seq: number;
  q: string;
  mode: "instant" | "full";
  col: string;
  kind: string;
  /** restrict to a single doc id (reader find); "" = no restriction */
  doc: string;
  /** blended-list offset for infinite-scroll continuations; omitted = 0 (first page) */
  offset?: number;
};

export type Collections = Record<string, string[]>;

export type DocState =
  | "queued"
  | "preparing"
  | "staged"
  | "text_ready"
  | "ready"
  | "failed"
  | "deleted";

/** Durable ingest status (data/status/<doc>.json), null for docs that
 * predate status tracking. */
export type DocStatus = {
  state: DocState;
  stage?: string;
  done: number;
  total: number;
  updated: number;
  error?: string;
};

export type DocInfo = {
  id: string;
  title: string | null;
  pages: number;
  collections: string[];
  processing: boolean;
  status: DocStatus | null;
};

export type OcrWord = { t: string; x: number; y: number; w: number; h: number };

// --- marginalia: note-box cards ---------------------------------------------

/** Normalized [x, y, w, h], top-left origin, 0..1 — OCR word space. */
export type Box = [number, number, number, number];

export type LinkKind = "continues" | "relates";
export type CardLink = { to: string; kind: LinkKind };

/** Mirror of the Rust QuoteAnchor wire shape (serde-flattened kind tag):
 * a mark on a document page, kept as a card's evidence. */
export type QuoteAnchor = { doc: string; page: number } & (
  | { kind: "text"; w0: number; w1: number; text: string; boxes: Box[] }
  | { kind: "region"; bbox: Box }
);

export type CardRec = {
  id: string;
  title: string;
  body: string;
  evidence: QuoteAnchor[];
  links: CardLink[];
  created: number;
  modified: number;
  filed: boolean;
  split_hinted: boolean;
};

export type NewCard = {
  title: string;
  body: string;
  evidence: QuoteAnchor[];
  links: CardLink[];
};

export type NeighborCard = { id: string; title: string; score: number };

// --- hidden perf view (Cmd+.) ---------------------------------------------

/** Constants + live corpus counts framing every perf-view screenshot. */
export type PerfMeta = {
  debug: boolean;
  emb_dim: number;
  clip_dim: number;
  k: number;
  k_doc: number;
  lex_fetch: number;
  img_fetch: number;
  min_rel: number;
  img_min_rel: number;
  rrf_k: number;
  mmr_pool: number;
  mmr_lambda: number;
  search_log_cap: number;
  chunks: number;
  figures: number;
  docs: number;
  now_ms: number;
};

/** Per-hit ranker provenance; lex_rank null = semantic-only (bypasses the
 * MIN_REL cutoff). */
export type HitProv = {
  doc: string;
  page: number;
  idx: number;
  rrf: number;
  rel: number;
  bm25: number;
  lex_rank: number | null;
  sem_rank: number | null;
  sem_dist: number | null;
};

export type ImgProv = { doc: string; page: number; idx: number; sim: number };

/** One answered query from the server-side ring buffer. */
export type SearchRecord = {
  ts_ms: number;
  q: string;
  mode: string;
  kind: string;
  col: string;
  doc: string;
  offset: number;
  phase: string;
  total_us: number;
  stages: [string, number][];
  lex_n: number;
  sem_n: number;
  rel_killed: number;
  img_fetched: number;
  img_killed: number;
  img_top: number;
  img_floor: number;
  served: number;
  zero: boolean;
  text_hits: HitProv[];
  img_hits: ImgProv[];
};

export type LegibilitySummary = {
  mean: number;
  median: number;
  noisy_pct: number;
  scored: number;
  pages: number;
  worst: [number, number][];
};

/** Ingest performance persisted on the status file; every field optional —
 * docs from before metrics existed show "not recorded". */
export type IngestMetrics = {
  timings_ms?: Record<string, number>;
  pages?: number;
  ocr?: [number, number, number];
  chunks?: [number, number];
  figures?: [number, number];
  legibility?: LegibilitySummary;
  at: number;
};

/** One perf-view ingest row: the status file joined with title + pages. */
export type IngestRow = {
  doc: string;
  title: string;
  pages: number;
  state: DocState;
  stage?: string;
  done: number;
  total: number;
  updated: number;
  error?: string;
  metrics?: IngestMetrics;
};

export type IngestEvent = {
  doc: string;
  stage: "log" | "ocr" | "clean" | "embed" | "figures" | "clip" | "indexing" | "done" | "error";
  done: number;
  total: number;
  message: string;
};

// --- librarian chat ---------------------------------------------------------

/** A doc/page the model actually saw, straight out of a tool result. */
export type ChatChip = { doc: string; title: string | null; page: number };

/** Sidecar NDJSON events, relayed verbatim by both hosts (web: SSE frames;
 * desktop: `chat:event`). The chat panel ignores members it has no use for,
 * so adding one here is non-breaking — `plan` in particular has been arriving
 * unread since the routing pre-pass landed. */
export type ChatEvent =
  | { e: "token"; text: string; replace?: boolean }
  | { e: "plan"; intent: string; approach: string; query: string; collection: string }
  | {
      e: "tool";
      name: string;
      status: "started" | "done";
      args?: Record<string, unknown>;
      summary?: string;
      hits?: ChatChip[];
      /** Retrieval quality from search_tool; absent on non-search tools. */
      confidence?: string;
      coverage?: number;
      top_bm25?: number;
      note?: string;
      /** ts_ms of this call's record in the perf search ring, for cross-link. */
      perf_ts?: number;
    }
  | { e: "done"; content: string; ms: number; model?: string }
  | { e: "cancelled" }
  | { e: "error"; message: string };

// --- agent provenance (client-side ring; see agent-log.ts) -----------------

/** One tool call, timed from event arrival in this tab. */
export type AgentTool = {
  name: string;
  args: Record<string, unknown>;
  /** ms after turn start when `started` arrived. */
  at_ms: number;
  /** started→done; null = never completed (turn ended mid-call). */
  ms: number | null;
  summary: string | null;
  hits: number;
  /** Distinct docs the call surfaced, in first-seen order. */
  docs: string[];
  chips: ChatChip[];
  confidence?: string;
  coverage?: number;
  top_bm25?: number;
  perf_ts?: number;
};

/** The schema-constrained routing pre-pass that precedes every turn. */
export type AgentPlan = {
  intent: string;
  approach: string;
  query: string;
  collection: string;
  /** turn start → plan event. */
  ms: number;
};

/** One chat turn as the perf view sees it. The sidecar keeps no ring of its
 * own, so every duration here is measured at event arrival in the browser. */
export type AgentTurn = {
  /** Monotonic; the expand key. */
  id: number;
  /** Date.now() at submit — directly comparable to SearchRecord.ts_ms. */
  ts_ms: number;
  prompt: string;
  conv: string;
  transport: "sse" | "tauri";
  outcome: "streaming" | "done" | "cancelled" | "error";
  error: string | null;
  plan: AgentPlan | null;
  tools: AgentTool[];
  /** submit → first token event. */
  ttft_ms: number | null;
  /** Token *events*, not model tokens: one delta can carry several. */
  deltas: number;
  chars: number;
  /** first token → last token. */
  stream_ms: number | null;
  /** submit → terminal event. */
  total_ms: number | null;
  /** done.ms — the model turn alone, excluding plan and host tool work. */
  sidecar_ms: number | null;
  model: string | null;
  content: string;
};

/** Mirror of the Rust atlas wire shapes (library-core atlas.rs). Compact
 * field names on the point/passage records are deliberate — the payload is
 * dominated by ~10-20k points. */
export type AtlasDoc = { id: string; title: string; collection: string; chunks: number };
/** One map dot: PCA position, doc index, theme id (-1 = none), neighborhood
 * doc-diversity, page, snippet. */
export type AtlasPoint = { x: number; y: number; d: number; c: number; e: number; p: number; s: string };
export type AtlasPassage = { d: number; p: number; s: string };
export type AtlasTheme = {
  id: number;
  size: number;
  ndocs: number;
  terms: string[];
  top: AtlasPassage[];
};
export type AtlasTrailStep = { d: number; p: number; s: string; sim: number };
export type AtlasTrail = { c: number; steps: AtlasTrailStep[] };
export type Atlas = {
  fingerprint: { version: number; docs: number; chunks: number; hash: number };
  built_ms: number;
  build_ms: number;
  docs: AtlasDoc[];
  points: AtlasPoint[];
  themes: AtlasTheme[];
  trails: AtlasTrail[];
};
/** Envelope from /api/atlas (web) and the `atlas` command (desktop). */
export type AtlasResponse =
  | { status: "ready"; atlas: Atlas }
  | { status: "building"; since: number; stage: string };
