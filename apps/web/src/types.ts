export type SnippetWord = { t: string; m: boolean };

/** Note-box context on a `kind: "card"` hit. */
export type CardMeta = {
  id: string;
  address: string;
  title: string;
  thread: number;
  breadcrumb: string;
};

/** Jump target on a `kind: "annotation"` hit — the real doc/page. */
export type AnnotMeta = { id: string; doc: string; page: number };

export type WireHit = {
  kind: "text" | "image" | "card" | "annotation";
  score: number;
  doc: string;
  page: number;
  idx: number;
  img: string;
  snippet: SnippetWord[];
  boxes: [number, number, number, number][];
  crop: [number, number, number, number];
  card?: CardMeta;
  annot?: AnnotMeta;
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

// --- marginalia: annotations + note-box cards -------------------------------

/** Normalized [x, y, w, h], top-left origin, 0..1 — OCR word space. */
export type Box = [number, number, number, number];

/** Mirror of the Rust AnnotRec wire shape (serde-flattened kind tag). */
export type AnnotRec = {
  id: string;
  doc: string;
  page: number;
  note: string;
  created: number;
} & (
  | { kind: "text"; w0: number; w1: number; text: string; boxes: Box[] }
  | { kind: "region"; bbox: Box }
);

export type LinkKind = "continues" | "relates";
export type CardLink = { to: string; kind: LinkKind };

/** A quoted passage: word range + snapshot, deep-linking into the reader. */
export type QuoteAnchor = { doc: string; page: number; w0: number; w1: number; text: string };

export type CardRec = {
  id: string;
  thread: number;
  addr: number[];
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
  parent: string | null;
  thread: number | null;
};

export type NeighborCard = { id: string; address: string; title: string; score: number };

export type ThreadProposal = {
  parent: string;
  parent_address: string;
  parent_title: string;
  thread: number;
  address: string;
};

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
