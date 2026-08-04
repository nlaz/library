// Plain-English readings for the perf header's constants, in two layers.
//
// GLOSS is hand-written and always correct: what the knob is, what units it
// carries, what it does *not* apply to. `evidence` is the layer that actually
// contextualizes it — arithmetic over the rings already on screen, so the
// number in the header comes with what it has been doing lately.
//
// Both layers stay separate strings and render as separate blocks, leaving
// room for a third: an on-device rewrite from the librarian sidecar, phrased
// for a reader who doesn't know the retrieval pipeline. That would need a
// one-shot `librarian explain` mode — deliberately NOT the `serve` sidecar,
// whose stdin loop cancels the active turn on every new request and would
// kill an in-flight chat. Not built; the seam is here.

import { spanMedian } from "./perf-flame";
import { median, us } from "./perf-fmt";
import type { AgentTurn, IngestRow, SearchRecord } from "./types";

export type Gloss = {
  /** One or two sentences: what this is. */
  what: string;
  /** Units, bounds, scope caveats. */
  range?: string;
};

export const GLOSS: Record<string, Gloss> = {
  // --- identity ---
  debug: {
    what: "Whether the host is running a debug build. Debug builds are several times slower — timings from one are not comparable to a release run.",
  },
  skew: {
    what: "Client clock minus server clock at the last poll. Row timestamps are stamped server-side but rendered against this browser's clock, so a large skew means the times you're reading are shifted.",
    range: "0 is perfect agreement",
  },
  docs: { what: "Documents in the library, in any state — including ones that failed to ingest." },
  chunks: {
    what: "Text chunks across all documents. This is what lexical and semantic search actually rank; a page is usually several chunks.",
  },
  figures: {
    what: "Figures extracted from page layouts and embedded with CLIP. Image search ranks these, not whole pages.",
  },

  // --- retrieval ---
  K: {
    what: "How many text hits a search returns per page of results. The rankers fetch far more than this and cut down.",
  },
  K_DOC: {
    what: "The cap on hits taken from any single document, so one dense book can't fill the whole result page.",
  },
  LEX_FETCH: {
    what: "How many candidates BM25 pulls before fusion. Raising it costs lexical search time but gives the fuser more to work with.",
  },
  IMG_FETCH: {
    what: "How many figure candidates CLIP pulls before the spread cutoff. Only relevant when image results are in play.",
  },
  MIN_REL: {
    what: "The relevance floor for text hits. Anything scoring below it is dropped as a degraded match — this is what makes a weak query return nothing rather than nonsense.",
    range: "0–1 · text only · semantic-only hits bypass it",
  },
  IMG_MIN_REL: {
    what: "The relevance floor for figure hits, applied alongside the spread cutoff (which drops figures too close to the noise floor).",
    range: "0–1 · figures only",
  },
  FUSE_W: {
    what: "The lexical share of the fused score: w·lexical + (1−w) semantic, each normalized to its own per-query top. 0.5 was the only setting in the 2026-07 sweep that beat rank-only RRF on every workload — lower favors paraphrase and loses known-item recall, higher the reverse.",
    range: "0–1 · 1.0 = lexical only",
  },
  NOTE_BOOST: {
    what: "How much a note card's fused score is multiplied by. Prose the reader wrote is a stronger statement of what matters to them than a page the ingester happened to OCR, so a card should outrank a page carrying comparable evidence. Not a sweep result — the gold set is books only.",
  },
  RERANK: {
    what: "The MaxSim late-interaction re-rank, shown as weight/pool. Each query token takes its best match against the chunk's word vectors — word-level interaction the pooled embedding averages away — and that blends with the fused score. Pool is how many top hits get rescored.",
    range: "weight 0–1 · quality peaks at pool 30 and declines past 50",
  },
  MMR: {
    what: "Maximal-marginal-relevance diversification, shown as lambda/pool. Lambda trades relevance against variety (1.0 = pure relevance); pool is how many candidates it reranks.",
  },
  FUZZ_CANDIDATES: {
    what: "How many real vocabulary words each unknown query word is corrected to before it reaches the lexical index. Words already in the vocabulary expand to nothing, so a clean query is untouched and only typos and OCR garble pay.",
  },
  emb_dim: { what: "Dimensionality of the ese text embeddings backing semantic search." },
  clip_dim: { what: "Dimensionality of the CLIP embeddings backing figure search." },
  search_log_cap: {
    what: "How many searches the server's ring buffer keeps. Older ones are gone — this view can only show what's still in the ring.",
  },

  // --- search spans (the flame chart's blocks) ---
  text_track: {
    what: "The whole text half of a search: embed the query, expand its terms, run both rankers, fuse and resolve. Runs on the calling thread while the image track runs beside it, so the two lanes overlap on purpose.",
  },
  img_track: {
    what: "The whole image half of a search: encode the query with CLIP, search the figure index, cut on the spread. Spawned on its own thread so its cost hides under the text track instead of adding to the total.",
  },
  ese_embed: {
    what: "Encoding the query into a text embedding with the compiled-in ese model. Skipped entirely on instant (per-keystroke) and doc-scoped queries, which stay lexical.",
  },
  term_expand: {
    what: "Tokenizing the query, then completing the trailing word from the term dictionary (type-ahead) and correcting unknown words to their nearest real vocabulary (fuzzy). Both feed the exact lexical index.",
  },
  lex_search: {
    what: "The BM25 postings scan, plus assembling the relevance map. Cost is independent of how many hits are wanted — postings are scanned in full and truncated at the end — so it tracks corpus size and query term count, not LEX_FETCH.",
  },
  vec_search: {
    what: "The HNSW nearest-neighbor search over chunk embeddings. Absent when the query has no embedding — an instant query records no span here at all rather than a measured zero.",
  },
  "fuse+resolve": {
    what: "Everything after both rankers return: blending their lists, the MaxSim re-rank, MMR diversification, and the point-reads that turn keys into hits. The blocks nested under it are where its time actually goes.",
  },
  fuse: {
    what: "Blending the lexical and semantic lists into one order, each normalized to its own per-query top and weighted by FUSE_W. Note cards take their boost here.",
  },
  maxsim: {
    what: "The MaxSim late-interaction re-rank — normally the most expensive block on the chart. Every query token is scored against every distinct word of every pooled chunk, which at a 30-chunk pool is ~95k dot products. Watch this one before raising RERANK's pool.",
  },
  mmr: {
    what: "Maximal-marginal-relevance diversification: demoting near-duplicates among the top hits. Full queries only, so an instant search records no span here.",
  },
  resolve: {
    what: "Reading each surviving hit's words back out of the primary table — one point-read per hit. Grows with how many hits are asked for, which is why doc-scoped find (K_DOC) costs more here than a normal page.",
  },
  clip_embed: {
    what: "Encoding the query into CLIP's shared text/image space. Runs through ONNX Runtime, so it is the slowest thing on the image lane and the reason that lane starts before it can search anything.",
  },
  image_search: {
    what: "The HNSW search over figure embeddings plus the spread cutoff that drops figures too close to the noise floor.",
  },
  blend: {
    what: "Interleaving the text and image hits into the served order and slicing out one page of it. The last thing a search does, and reliably the cheapest.",
  },
  search_tool: {
    what: "One search the librarian agent ran as a tool call. Same ranker as a human's search, different framing: no type-ahead completion, no diversification, and its own page-level dedup afterwards.",
  },
  search: {
    what: "The ranker call inside an agent's tool search — the same code path a UI search takes, with its phases nested underneath.",
  },
  "dedup+cutoff": {
    what: "Dropping hits below the relevance floor, then collapsing multiple hits from one page down to one so the model sees distinct pages rather than the same page repeated.",
  },
  top_hit_page: {
    what: "Reading the top hit's whole page so it rides back with the tool result — one hop instead of the model asking for it in a second turn. Skipped when confidence is none.",
  },

  // --- ingest ---
  legibility: {
    what: "How readable a document's OCR text is, scored per page and summarized per doc. Low scores mean garbled text that search can't match even when the page is relevant.",
    range: "0–1 · higher is cleaner",
  },
  "noisy%": {
    what: "Share of a document's pages whose worst text window scores below 0.45. These are the pages a re-OCR pass would fix.",
  },
  "ocr t/v/c": {
    what: "Where each page's words came from: the PDF's own text layer, Vision OCR, or a cached earlier run. Text-layer pages are free and exact; Vision pages are the slow, fallible ones.",
  },

  // --- memory ---
  "corpus disk": {
    what: "Everything the documents weigh on disk, across sources: original pdfs, rendered page scans, OCR text, and the chunk/figure record tables. None of it is resident — content streams from disk on demand.",
  },
  originals: {
    what: "The source PDFs as imported. Read once at ingest (and for re-ingest); serving the reader uses the rendered page scans, not these.",
  },
  "page scans": {
    what: "Rendered page images, one per page — usually the biggest slice of the corpus on disk. Streamed to the reader per request, so they cost page cache at most, never heap.",
  },
  "ocr text": {
    what: "OCR output and cleaned text overlays per document. Input to chunking and legibility scoring; not loaded at query time.",
  },
  "chunk records": {
    what: "The primary table: every chunk's words (with positions) and its embedding, plus the figure records. This is what search hits resolve against — read per hit, not held in memory.",
  },
  "emb payload": {
    what: "The exact bytes of all embeddings (chunks + figures × dimension × 4). Resident, but inside the HNSW index rows — listed here to show how much of the index cost is the vectors themselves versus graph links and maps.",
  },
  footprint: {
    what: "phys_footprint: everything this process is charged for, including pages the memory compressor has squeezed and pages swapped out. This is what Activity Monitor's Memory column shows and what memory pressure is judged on, so it is the total every other number on this tab is trying to explain.",
  },
  rss: {
    what: "Resident set size: only the pages currently backed by physical RAM. macOS compresses an idle process's heap, so this can fall to a small fraction of the footprint while the app is holding exactly as much — it once read 23 MiB against 205 MiB of accounted line items, which is why the percentages are taken against footprint instead. Shown because the gap between the two is itself the read: rss far below footprint means most of the heap is sitting compressed.",
  },
  accounted: {
    what: "The sum of every RAM line item below: indexes, caches, the ese weights, and the stores' memtables, block caches, and pinned blocks. Disk figures are never included.",
  },
  unaccounted: {
    what: "footprint minus accounted — memory we can see but not name. Mostly the CLIP ONNX runtime's arena, plus page cache for memory-mapped store files, allocator retention, and per-thread search scratch. Still signed: it can go negative when capacity-based estimates overshoot, and reliably does on the rss fallback path, where the total excludes compressed pages the estimates still count.",
  },
  slots: {
    what: "The HNSW graph's high-water mark. Removals tombstone nodes onto a free list and never shrink the arrays, so a churned index costs slots × per-vector bytes, not live × — that's the gap between live and slots.",
  },
  "per-vector bytes": {
    what: "The dense per-slot cost of the graph: the embedding itself plus layer-0 neighbor links plus node metadata. Multiply by slots for the graph's floor; the sink's id/key maps come on top.",
  },
  stale: {
    what: "The in-memory graph has diverged from the store (a transaction aborted after a mid-transaction flush). The numbers describe the pre-rebuild graph; the next search pays a full rebuild from the persisted vectors.",
  },
  "doclen cache": {
    what: "BM25's one resident structure: an in-memory mirror of every chunk's token count, so scoring never point-reads the store. Cold until the first search pays one sequential scan; postings themselves stay on disk.",
  },
  "ese weights": {
    what: "The text-embedding model is compiled into the binary as read-only data. The pages are file-backed and shared, so they count toward RSS only as they're touched — a fixed, exactly-known ceiling.",
  },
  onnx: {
    what: "The CLIP model runs inside ONNX Runtime, which allocates from its own arena that Rust-side accounting can't see. Its real cost shows up only in RSS — it's usually the biggest slice of unaccounted.",
  },
  memtable: {
    what: "fjall's in-RAM write buffers (active + sealed) awaiting flush to disk. Uncapped under the current config, so a heavy ingest can grow this until the flush worker catches up.",
  },
  "block cache": {
    what: "fjall's cache of recently read data blocks, shown as used/capacity. Capacity is per database — each store gets its own.",
    range: "32 MiB default per store",
  },
  "pinned filters": {
    what: "Bloom filters and index blocks pinned in RAM outside the block cache, per keyspace. Easy to miss and grows with the tree — expand a store row to see which keyspace holds it.",
  },
  "orphan keyspace": {
    what: "A keyspace present in the store but not opened by the current graph — left behind by an older pipeline shape. Costs disk (and backup size), not RAM; safe to reclaim in principle but nothing here deletes data.",
  },
  "thread scratch": {
    what: "Each thread that has ever searched an HNSW graph keeps a visited-set buffer of 4 bytes per slot. With a blocking pool answering searches, that's slots × 4 × threads — real memory no line item claims.",
  },

  // --- agent ---
  ttft: {
    what: "Time to first token: submit until the first text arrives. Everything before it is routing and tool work, not generation.",
  },
  sidecar_ms: {
    what: "The turn duration the sidecar itself reported — the model's generation alone. The gap between it and total is planning, tool execution, and relay.",
  },
  plan: {
    what: "The schema-constrained routing pre-pass that runs before the turn: it classifies intent and picks the first tool, because a 3B model is most reliable at guided classification and least reliable at spontaneous tool choice.",
  },
  confidence: {
    what: "What retrieval told the model about its own results — strong, weak, or none — derived from the top BM25 score and query coverage. On weak or none the model is instructed to say the library doesn't cover it rather than stretch the hits.",
  },
  coverage: {
    what: "Fraction of the query's terms that appear in the returned hits. Low coverage with a high score usually means one term is carrying the match.",
    range: "0–1",
  },
};

type Rings = { searches: SearchRecord[]; ingest: IngestRow[]; agent: AgentTurn[] };

/** Live evidence for a term, from the data already on screen. Returns null
 * when there's nothing honest to say — an empty ring gets silence, never a
 * fabricated or NaN number. */
export function evidence(name: string, d: Rings): string | null {
  const { searches, ingest, agent } = d;
  const n = searches.length;
  switch (name) {
    case "MIN_REL": {
      const m = median(searches.map((r) => r.rel_killed));
      if (m === null) return null;
      const zero = searches.filter((r) => r.zero).length;
      return `last ${n} searches: dropped a median of ${m} hits here${
        zero ? ` · ${zero} returned nothing at all` : ""
      }`;
    }
    case "IMG_MIN_REL": {
      const withImgs = searches.filter((r) => r.img_fetched);
      if (!withImgs.length) return "no image searches in the ring";
      const m = median(withImgs.map((r) => r.img_killed));
      return `${withImgs.length} image searches: a median of ${m} figures cut`;
    }
    case "K": {
      const m = median(searches.map((r) => r.served));
      return m === null ? null : `last ${n} searches served a median of ${m} hits`;
    }
    case "LEX_FETCH": {
      const m = median(searches.map((r) => r.lex_n));
      return m === null ? null : `lexical ranker returned a median of ${m} candidates`;
    }
    // the re-rank is the block that dominates the chart, and its share of a
    // search is the number to check before touching RERANK's pool or weight
    case "RERANK":
    case "maxsim": {
      const m = spanMedian(searches, "maxsim");
      if (m === null) return null;
      const total = median(searches.filter((r) => r.spans.some((s) => s.name === "maxsim")).map((r) => r.total_us));
      const share = total ? ` · ${Math.round((m / total) * 100)}% of a median search` : "";
      return `median re-rank across the ring: ${us(m)}${share}`;
    }
    case "lex_search":
    case "vec_search":
    case "ese_embed":
    case "resolve":
    case "fuse+resolve": {
      const m = spanMedian(searches, name);
      return m === null ? null : `median across ${n} searches: ${us(m)}`;
    }
    case "search_log_cap": {
      if (!n) return "ring is empty";
      return `${n} in the ring · oldest ${new Date(searches[n - 1].ts_ms).toLocaleTimeString()}`;
    }
    case "chunks":
    case "docs": {
      const m = median(searches.map((r) => r.total_us));
      return m === null ? null : `median search over this corpus: ${(m / 1000).toFixed(1)}ms`;
    }
    case "legibility": {
      const scored = ingest.filter((r) => r.metrics?.legibility);
      if (!scored.length) return null;
      const m = median(scored.map((r) => r.metrics!.legibility!.mean));
      return `${scored.length} docs scored · median mean ${m!.toFixed(2)}`;
    }
    case "noisy%": {
      const scored = ingest.filter((r) => r.metrics?.legibility);
      if (!scored.length) return null;
      const bad = scored.filter((r) => r.metrics!.legibility!.noisy_pct > 0.2).length;
      return `${bad} of ${scored.length} scored docs are over 20% noisy`;
    }
    case "ocr t/v/c": {
      const withOcr = ingest.filter((r) => r.metrics?.ocr);
      if (!withOcr.length) return null;
      const tot = withOcr.reduce(
        (a, r) => {
          const o = r.metrics!.ocr!;
          return [a[0] + o[0], a[1] + o[1], a[2] + o[2]] as [number, number, number];
        },
        [0, 0, 0] as [number, number, number],
      );
      const sum = tot[0] + tot[1] + tot[2];
      if (!sum) return null;
      return `${withOcr.length} docs: ${Math.round((tot[0] / sum) * 100)}% text-layer, ${Math.round(
        (tot[1] / sum) * 100,
      )}% vision, ${Math.round((tot[2] / sum) * 100)}% cached`;
    }
    case "ttft": {
      const m = median(agent.map((t) => t.ttft_ms).filter((x): x is number => x !== null));
      return m === null ? null : `median across ${agent.length} captured turns: ${Math.round(m)}ms`;
    }
    case "plan": {
      const planned = agent.filter((t) => t.plan);
      if (!planned.length) return null;
      const m = median(planned.map((t) => t.plan!.ms));
      return `${planned.length}/${agent.length} turns planned · median ${Math.round(m!)}ms`;
    }
    case "confidence": {
      const calls = agent.flatMap((t) => t.tools).filter((c) => c.confidence);
      if (!calls.length) return null;
      const weak = calls.filter((c) => c.confidence !== "strong").length;
      return `${weak} of ${calls.length} agent searches came back weak or empty`;
    }
    default:
      return null;
  }
}
