export type SnippetWord = { t: string; m: boolean };
export type WireHit = {
  kind: "text" | "image";
  score: number;
  doc: string;
  page: number;
  idx: number;
  img: string;
  snippet: SnippetWord[];
  boxes: [number, number, number, number][];
  crop: [number, number, number, number];
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

export type IngestEvent = {
  doc: string;
  stage: "log" | "ocr" | "clean" | "embed" | "figures" | "clip" | "indexing" | "done" | "error";
  done: number;
  total: number;
  message: string;
};
