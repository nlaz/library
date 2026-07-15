# Chat agent spike: Apple Foundation Models over The Library

*July 2026 · M3 MacBook Air, 8 GB, macOS 26.3. Model: the on-device Apple
Foundation Models system model (~3B, ANE) via a Swift sidecar
(`apps/librarian`). Eval harness: `examples/librarian-eval` (rerunnable:
`retrieval` battery + `probe` fixtures P1–P8). Raw dumps beside this file:
`retrieval-results.json`, `probe-results.json`.*

## Verdict: go

The on-device model is good enough to ship the chat experience this spike
built, provided the app keeps doing the retrieval thinking and treats the
model as a *narrator over tool results*, not a knower. Tool calling is
reliable, grounded QA over supplied pages is accurate, latency is fine
(0.5–8 s per turn, no RAM cost to our processes), and the failure modes are
consistent and designable-around.

## Headline numbers

| probe | result |
|---|---|
| P1 tool-call reliability | 10/10 correct decisions (8/8 searched when needed, 2/2 answered greetings without a tool call) |
| P2 grounded QA | answerable: correct; unanswerable: clean abstention ("the text does not mention…") |
| P3 summarization | correct on real text; on an (accidentally) empty page it *abstained* — good |
| P4 structured extraction | recipe → clean JSON steps/ingredients; catalog page → exact item/price/supplier (`$4.00 postpaid`, University of Detroit Press) through OCR noise |
| P5 cross-doc synthesis | 4 tool calls, correctly sequenced (search ×2 → read ×2), coherent comparison — but it read *plausible* pages, not the *hit* pages, so the comparison was generic |
| P6 query reformulation | one search, then hedged honestly instead of retrying with new terms |
| P7 latency vs context | 0.65 s (1 page) → 1.1 s (2) → 1.8 s (4). Linear and cheap; 2-page reads are not the bottleneck |
| P8 guardrails (permissive mode) | 0 trips in 8 pointed probes (butchering, knife sharpening, meat curing, brewing, wine) |

Chat turns end-to-end through the full pipeline (browser → SSE → sidecar →
search → answer): ~2–10 s. Memory: no measurable pressure — the OS hosts the
model.

## The one dangerous failure mode

**Empty context + direct question = confident confabulation with a fake
citation.** Page 12 of the Carlston book is blank (the intro starts on 13).
Asked to *summarize* it, the model abstained. Asked a *question* against it,
the model invented "he worked as a programmer for Acorn Computers **[doc
p.12]**". With real text present it answered the same question correctly
(lawyer in rural Maine). Implication: never let the model see an empty or
failed tool result that looks like content. `read_pages` should return a loud
`"error": "page 12 is blank/has no text"` rather than an empty string — the
model treats explicit errors correctly (it relayed the server-unreachable
error verbatim rather than inventing an answer).

## What the model is uniquely good at (given the right tools)

1. **Guided generation is the standout.** With a `DynamicGenerationSchema`
   the OS *constrains decoding* to the schema — malformed JSON is impossible,
   and extraction quality through OCR noise was excellent. This is a
   capability the app can build features on beyond chat: batch recipe
   extraction (recipe index/shopping lists), Whole Earth catalog-entry
   mining (item/price/supplier database), spec sheets from the CS books.
   These can run as background jobs over `data/text/` at zero API cost.
2. **Faithful narration over supplied text.** Grounded QA and page
   summarization are accurate with real text in context, and abstention on
   absent details is reliable. The "read me the page" experience
   (search → read_pages → narrate with citation) is the sweet spot.
3. **Multi-step tool use is real** (P5's 4-call sequence), and the free/local
   economics mean multi-call turns cost only seconds, not dollars.

## What it can't do (design around, don't fight)

- **Query formulation is naive.** It searches with the user's own words,
  sometimes picks `kind=images` wrongly ("early software industry" → image
  search → false "nothing found"), and doesn't retry with better terms after
  weak hits (P6). The app should own query strategy: always run text search
  (add images as a supplement, never a substitute), and consider having the
  tool auto-retry variants server-side.
- **It navigates by plausibility, not by the hits.** In P5 it read intro
  pages instead of the tandoori pages its own search returned. Tool results
  should *push* the right next step: include a `"read_next": [{doc, page}]`
  hint in search results, or fold the top hit's full page text directly into
  the search result so a second hop isn't needed.
- **Long doc ids get mangled in prose** (already mitigated: fuzzy id match in
  `/api/text`, citation chips come from tool events, not prose).
- **4k context is real but sufficient** for search + one 2-page read + a
  short history. The overflow-retry path exists in the sidecar; it didn't
  trigger in any probe.

## What the retrieval battery says about search (for tool use)

Retrieval quality is the weaker half of the experience today; three concrete
issues:

1. **`rel` is relative, not absolute — it cannot gate "did we find
   anything?".** Expected-miss queries scored *rel = 1.0* on junk:
   "quantum entanglement" → a page's "entanglement between methods";
   "kubernetes deployment" → generic "deployment" hits; "micro" →
   *"microscope"* hits at rel 1.0 (trailing-token prefix expansion). The
   `MIN_REL=0.25` floor keeps UI noise down but tells an agent nothing.
   Recommendation: expose an absolute signal in `/api/search` (raw BM25 of
   the top hit, or top-hit-vs-corpus-baseline), and have `search_library`
   say "weak hits — the library may not cover this" below a threshold so
   the model can abstain honestly.
2. **Prefix expansion of the last token hurts programmatic queries.**
   It's built for type-ahead ("micro…" → microscope) but agent queries are complete
   words. Worth a `complete=false` flag on `/api/search` (skip
   `TermDict::complete`) for tool calls.
3. **Semantic (ese) recall is shallow on paraphrase.** "how to keep bread
   from going stale" surfaced fish books; 0 of 5 paraphrase queries put a
   rel≥0.9 text hit in the top 5 (vs 11 for known-item). Known-item and
   proper-noun search is strong. For the agent this means: snippets alone
   often can't answer paraphrased questions — the read_pages hop is
   load-bearing, which is another reason to fold page text into search
   results.
4. **Image hits lead many text queries** (rank-1 slot via the blend, rel
   0.0). For the *chat tool* they're mostly noise; kind="all" tool calls
   might exclude images unless the query asks for figures.

## Recommendations (priority order)

1. Make `read_pages`/`/api/text` return explicit errors for blank/empty
   pages (kills the confabulation vector). Cheap, do first.
2. Fold the top text hit's full page into `search_library` results (one hop
   instead of two, fixes plausibility-navigation, fits 4k budget for ~1 page).
3. Add an absolute-confidence signal + "weak hits" phrasing to the search
   tool; add `complete=false` for agent queries.
4. Build one guided-generation feature to exploit the schema-constrained
   decoding (recipe index is the obvious candidate — P4 quality was
   striking).
5. Keep guardrails permissive; surface the rare trip as the friendly error
   row (already wired). Trip rate in normal library use rounds to zero.

## Infrastructure notes (for whoever touches this next)

- Sidecar gotchas (all handled in `apps/librarian`): default guardrails trip
  on benign cooking content → `permissiveContentTransformations`; `@Generable`
  macros need full Xcode → hand-built `GenerationSchema`; streaming yields
  cumulative snapshots → sidecar diffs to deltas; first snapshot can be a
  literal `"null"` → filtered.
- The eval harness is rerunnable against any future model/runtime (the
  fixtures are runtime-agnostic JSON): `cargo run -p librarian-eval --
  retrieval` / `probe` with the server up. If AFM ever feels too small, the
  fallback plan is Ollama + Qwen3-4B (~2.6 GB resident — the RAM cost is why
  AFM won this round).
