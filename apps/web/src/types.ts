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
};

export type Collections = Record<string, string[]>;

export type DocInfo = {
  id: string;
  title: string | null;
  pages: number;
  collections: string[];
  processing: boolean;
};

export type OcrWord = { t: string; x: number; y: number; w: number; h: number };

export type IngestEvent = {
  doc: string;
  stage: "log" | "ocr" | "embed" | "figures" | "clip" | "indexing" | "done" | "error";
  done: number;
  total: number;
  message: string;
};
