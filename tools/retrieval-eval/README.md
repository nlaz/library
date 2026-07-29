# retrieval-eval

Retrieval-*quality* eval harness: recall@k, MRR@10, NDCG@10 for the search
stack. The criterion benches measure speed; this measures whether the right
documents come back. Run it before and after any embedding or ranking
experiment (pooling changes, quantization, model swaps, fusion tweaks).

Two layers, deliberately separated:

- **`encoder`** — embedding quality in isolation. Ranks the corpus by exact
  brute-force cosine (no HNSW, no BM25), so a change in these numbers is a
  change in the embeddings and nothing else. Encoders: `ese` (the model
  baked into this build) and `potion:<hf-id>` (any model2vec hub model,
  e.g. `potion:minishlab/potion-retrieval-32M`) as baselines.
- **`pipeline`** — what the app actually does. Builds a throwaway temp-dir
  `Library` from the same corpus (real ingest embedding path), then runs
  the real ranker in four configurations: `lex-only`, `sem-only` (the raw
  HNSW list, capped at its compile-time K=40), `hybrid` (the shipping
  fusion), and `hybrid+mmr`. Gaps between these rows are pipeline effects,
  not model effects.
- **`fusion`** — the fusion laboratory. Pulls the raw BM25 and HNSW
  candidate lists per query (the exact inputs `rank::search` fuses) and
  scores alternative fusion functions offline, across four workloads:
  fixture paraphrase, fixture known-item, GooAQ questions, and a synthetic
  known-item-at-scale set (each gold answer's three longest words as the
  query). This sweep is how the shipping score-blend fusion (α=0.5,
  `FUSE_LEX_WEIGHT` in `rank.rs`) was chosen over rank-only RRF in July
  2026: it was the only variant beating RRF on every metric of every
  workload. Rerun it before touching fusion again.

## Usage

```sh
cargo run --release -p retrieval-eval -- encoder --docs 10000 --queries 1000 --out /tmp/ese.json
cargo run --release -p retrieval-eval -- encoder --encoder potion:minishlab/potion-retrieval-32M
cargo run --release -p retrieval-eval -- encoder --doc-encoder potion:<id> --query-encoder ese
cargo run --release -p retrieval-eval -- pipeline --docs 10000 --seed 42,43,44
cargo run --release -p retrieval-eval -- fusion --docs 10000
cargo run -p retrieval-eval -- smoke     # offline, asserted floors, exit 1 on fail
```

Statistical hygiene, built in:

- **Multi-seed**: `--seed` takes a comma list. Each seed resamples the
  corpus and query set; the table reports the macro average over the union
  of queries, followed by a per-seed ndcg@10 spread (with sd) so a delta
  can be judged against sampling noise before believing it. Rule of thumb:
  a difference smaller than the seed sd is not a result.
- **Paired win/loss**: `pipeline` and `fusion` print, for each config
  against the baseline (`hybrid`, resp. the shipping fusion), how many
  queries it ranked the gold strictly higher (W), lower (L), or tied (T)
  on reciprocal rank. All configs see the same queries over the same
  corpus, so a lopsided W/L split is meaningful even when the averages
  differ by a fraction of a point.

Asymmetric encoding: `--doc-encoder`/`--query-encoder` (defaulting to
`--encoder`) embed the corpus and the queries with different models — the
"smart embeddings at ingest, fast embeddings at query" scheme. The two
models must produce the same dimension *and be trained into the same
space* (a model2vec student vs. its teacher); mixing two unrelated models
of equal dimension will run but score garbage, by design.

- `encoder`/`pipeline` use GooAQ question→answer pairs; the first run
  downloads ~500 MB of parquet into `target/gooaq/` (shared with the ese
  gooaq bench). Sampling is seeded (`--seed`, default 42) and answers are
  deduped, so runs are reproducible.
- Debug builds are fine up to ~10k docs (the tool and its deps run at
  opt-level 2 in dev); use `--release` for bigger corpora.
- `smoke` needs no network: ese's weights are compiled in and the corpus is
  `fixtures/paraphrase.json` — 50 prose passages with two tagged query
  workloads. `paraphrase` queries share little vocabulary with their gold
  doc (the known ese weak spot); `known-item` queries are short
  remembered-phrase lookups whose words appear in the gold doc (how a
  personal library is actually searched). Smoke reports each workload and
  the mix separately. Floors sit ~5 points under the values observed when
  they were set; they catch collapses, not noise. Rebaseline them on
  purpose, not by accident.

## The in-domain gold set (`gen-gold` / `library`)

GooAQ measures retrieval over clean web snippets; the actual library is
OCR'd book pages. The `gen-gold` subcommand builds a labeled eval set from
the real corpus: it samples pages from `data/text/*.md` (strictly
read-only — no fjall store is opened, so it runs alongside the app), asks
the librarian sidecar (`probe`, toolless, schema-constrained, no server
needed) to write the search query a reader would type to find each page,
and freezes corpus + queries into a JSON file:

```sh
swift build -c release --package-path apps/librarian   # once, if not built
cargo run -p retrieval-eval -- gen-gold --docs 2000 --queries 100 --out target/library-gold.json
cargo run -p retrieval-eval -- library --gold target/library-gold.json --out /tmp/library.json
```

`library` runs the encoder-level eval plus all four pipeline configs over
the frozen set, with paired stats. Generation is model-driven and not
reproducible — the frozen JSON is the artifact; keep it, diff eval runs
against it, and regenerate deliberately. It contains excerpts of the
personal library, so it lives under `target/` (untracked), not in git.
Guardrail refusals are skipped during generation (the page stays in the
corpus as a distractor). When GooAQ and the library set disagree about a
change, trust the library set — it is the deployment distribution.

## Encoder experiment history (2026-07)

Semantic-improvement sweep on the library gold set (2,000 pages / 100
queries, NDCG@10; ese baseline 0.678, BM25 0.774, shipping hybrid 0.753).
The `embed` subcommand exports ese's exact vectors for offline (Python)
experiments — quantization included.

- **Static-model swaps**: potion-retrieval-32M looked like +1.4
  (0.693) in the Python runtime — but baking it into ese scored 0.665,
  and the discrepancy traced to model2vec's **default 512-token document
  truncation**: full-length potion is 0.679 ≈ ese's 0.678. The swap is
  neutral; the artifact was the truncation. (Baking it works
  mechanically — parse is tensor-name-agnostic, but special-token rows
  must be found by NAME, not BERT's ids 100..102: potion uses 1..3.)
  potion-base-8M 0.576 (regression). quant-8/dim-512 vs f32/full-dim:
  no measurable cost, on either model.
- **Text-per-vector (the truncation finding)**: encoding only a page's
  first 512 tokens beats encoding all of it (0.693 vs 0.679) — dilution
  in action — but 256 tokens is too little (0.586), and multi-chunk
  max-sim scoring at 350/150/80 words is flat (0.676–0.687): smaller
  chunks trade dilution for false-positive surface. Granularity moves
  points but is not a breakthrough at page scale.
- **Pooling** (current model, same tokens): SIF weighting a/(a+p(t))
  0.689 (+1.1); IDF 0.680; dropping CLS/SEP: nil. The tokenizer emits no
  [UNK] but fragments identifying rare words into meaningless pieces
  (mignonette → mig ##non ##ette) — that dilution is the failure mode,
  and per-token *weighting* recovers only part of it.
- **Projection alignment** (train ridge W between independently trained
  spaces, both directions): transformer-docs→ese-space + ese queries
  peaked at 0.637; ese docs + transformer-query→ese-space 0.459. Both
  *below* plain ese — equal dimension count is not equal space. Dead end
  as a bolt-on; don't retry without a jointly trained student/teacher.
- **DIY distillation** (model2vec distill of bge-small, ±27k-word corpus
  vocabulary): 0.436 / 0.401. Naive distillation is not the potion
  recipe — the quality lives in the post-distillation tokenlearn
  training, not the distill step. A corpus vocabulary also can't capture
  the words that matter: true identifiers are df=1 in this corpus.
- **Transformer ceilings** (both sides, own space): bge-small-en-v1.5
  0.735 — still under BM25 on this workload; MiniLM-L6-v2 0.649 — under
  ese. Full-context encoding is not automatically better on long OCR
  pages; truncation and page length dominate.

- **SIF weighting, baked (2026-07-28)**: general-English frequencies
  (Norvig count_1w, downloaded/cached like the model files) perform on
  par with in-domain frequencies (0.686 vs 0.684 in the prototype), so
  the weights bake at build time: each vocab row is pre-scaled by
  a/(a+p(token)) in `ese/build.rs` — cosine is scale-invariant, so the
  runtime is untouched and mean pooling of scaled rows *is* weighted
  pooling. CLS/SEP are scaled to ~0 (the validated configuration).
  Measured after baking: library encoder 0.678→**0.689**, sem-only
  0.657→**0.685** (the ANN gap nearly closed), hybrid 0.753→**0.768**
  with recall@1 0.66→0.68 — hybrid now ties lex-only (11W/15L).
  Cost: GooAQ encoder −1.9, but hybrid there only −0.3 (0.828→0.825);
  smoke floors all pass, several points higher. Shipping requires
  re-embedding existing libraries — vectors from different weightings
  don't mix in one index.
- **Word-level token table (Layer 2 prototype)**: grouping tokens into
  per-word composed vectors with library-df weights scored 0.636 — worse
  than unweighted pooling — and the ablation (word grouping, general
  weights) 0.604 shows the word-level grouping itself is the regression,
  not the weighting. Concept withdrawn before implementation.

- **PRF / RM3 query expansion (2026-07-28)**: the `prf` subcommand runs
  the classic round-2 lexical expansion offline (terms from round-1's
  top-K docs scored tf·idf, K/E swept). Decisively negative on the gold
  set: every setting craters recall@1 (0.34–0.59 vs 0.68) from query
  drift — round-1's top hit already IS the gold for ~68% of queries, so
  expansion mostly dilutes winners. Paired stats cap the upside at ~9
  queries against 19–49 losses. Closed.
- **Cross-encoder reranker spike (2026-07-28)**: `library --dump` exports
  the real hybrid top-20; a MiniLM-class ms-marco cross-encoder rescores
  the pairs. First configuration to beat BM25:
  ms-marco-MiniLM-L6-v2 rerank-only 0.796, blended 0.9·CE + 0.1·fused
  **0.805 NDCG@10 / recall@1 0.72** (hybrid 0.768/0.68, BM25 0.774/0.67),
  at ~150 ms per query for 20 pairs on CPU (PyTorch batch; ONNX int8
  would roughly halve that). MiniLM-L12 matches at 2× cost — take L6.
  The rerank stage is the lever aimed at recall@1: run on Enter, never
  per keystroke.
- **Static MaxSim reranker (2026-07-28)** — the embeddable answer to the
  cross-encoder's latency: ColBERT-style late interaction using ese's own
  baked token vectors. Each query token takes its best cosine over the
  page's token set (deduped), SIF-weighted sum, blended with the fused
  score. Blend 0.7·maxsim + 0.3·fused scores **0.801 NDCG@10 / recall@1
  0.72** — statistically equal to the cross-encoder (0.805/0.72) — at
  **1.4 ms/query in numpy** (sub-ms expected in Rust). No new model, no
  runtime: table lookups and dot products, "the way ese worked." Blend
  weight was tuned on the gold set — verify on GooAQ + smoke when
  implementing, and prefer w=0.5–0.7 (0.792–0.801, both strong).

- **Char-n-gram splitter / OCR robustness (2026-07-28)**: measured the
  disease first — true OCR garbage is **0.020%** of corpus tokens (197
  occurrences/1M; several are legitimate identifiers like `rdwr`). The
  3.8% dictionary-OOV mass is French/Italian cookery vocabulary, i.e.
  content. Then the cure: fastText (char 3–6-grams) trained on the full
  library text is genuinely corruption-robust (corrupted word forms stay
  at cosine 0.9+) but scores **0.33** NDCG@10 on the gold set — the
  splitter family can't carry retrieval. Closed: wrong disease, and a
  cure that costs 36 points. If the foreign-vocabulary mass ever
  matters, that's a multilingual-model question, not a splitter one.

- **MaxSim reranker SHIPPED into rank.rs (2026-07-28)**: the
  late-interaction re-rank now runs inside `search()` on every query —
  `maxsim_rerank` rescores the fused top-20 (RERANK_POOL) with each query
  token's best cosine against the chunk's deduped token vectors (via the
  new `ese::for_each_token_vector`; row norms carry the SIF weights),
  blends 0.7·maxsim + 0.3·fused (RERANK_WEIGHT), and rescales into the
  pool's score range so the list stays monotonic. Measured live through
  the real pipeline: library gold hybrid 0.768→**0.813** NDCG@10,
  recall@1 0.68→**0.73** (exactly matching the offline spike); GooAQ
  hybrid 0.825→**0.857**, recall@1 0.725→**0.784** — hybrid now beats
  sem-only on GooAQ for the first time; all smoke floors pass at their
  highest observed values (paraphrase hybrid r@5 0.92, mixed ndcg 0.89).
  Latency: ~16 ms/query total in the dev profile — well inside the
  keystroke budget, so it is not gated behind Enter.

Standing conclusion: hybrid search now beats every single-signal
configuration on both corpora — the MaxSim rerank supplied the word-level
interaction the pooled bi-encoder averages away, which was the last
measured gap. Every other lever (model swap, truncation, chunking,
quantization, word tables, PRF, char-ngrams, projection alignment) is
neutral or negative. Remaining unexplored: librarian document expansion,
adaptive fusion α, and full in-domain training.

## Gold set v2 baseline (2026-07-29)

`target/library-gold-v2.json`: 500 queries over 4,000 pages (seed 43),
generated with three cycled steering styles recorded per query; the
`library` subcommand reports per-kind tables alongside the blend. Noise
floor ~±1.3 NDCG points (vs ±3 for v1's 100 queries). First run with the
shipped stack (SIF + MaxSim pool-30) — NOT comparable to v1 numbers, the
corpus is twice as hard:

| config | all (500) | known-item (167) | paraphrase (167) | question (166) |
|---|---|---|---|---|
| hybrid | **0.798** | 0.876 | 0.767 | 0.750 |
| lex-only | 0.791 | 0.875 | 0.759 | 0.740 |
| encoder | 0.581 | 0.547 | 0.637 | 0.559 |
| sem-only | 0.568 | 0.532 | 0.628 | 0.542 |

Reading: hybrid's edge over lex-only lives entirely in paraphrase and
question workloads (paired overall 66W/44L for hybrid); known-item is
dead even at 0.88 — BM25 already solves it and the semantic side adds
nothing there. The static encoder degrades further at 4k-page scale
(0.581 vs 0.689 at 2k) while BM25 holds — corpus growth widens the
lexical-semantic gap, strengthening the case for the ceiling-raiser
roads. v1 stays frozen for cross-checking; v2 is the referee for all
future decisions.

## External calibration: NanoBEIR (2026-07-29, rerun and corrected)

The shipped recipe replicated faithfully in Python and run on NanoBEIR
(13 public datasets × 50 queries, NDCG@10 averaged) against the industry
reference models — all contestants scored by identical metric code.

| system | avg | class |
|---|---|---|
| bge-small-en-v1.5 | 0.627 | transformer, 33M params |
| **ours: BM25 + SIF-ese fusion + MaxSim** | **0.577** | static + lexical |
| all-MiniLM-L6-v2 | 0.563 | transformer, 22M params |
| BM25 | 0.543 | lexical |
| potion-retrieval-32M | 0.511 | best published static |
| ese pre-SIF / SIF (encoder alone) | 0.494 / 0.486 | static |

**These numbers supersede the first run's.** That run recorded ours 0.588
and bge 0.609 (a 2.1-point gap); the real gap is 5.0. The error was on
bge's side — it was scored without the retrieval query prefix BAAI
specifies, which it needs to perform correctly. Four of seven systems
reproduce across the two independently-written harnesses to within 0.001
(potion 0.511, ese 0.494/0.486, MiniLM 0.563), which is what makes the
bge discrepancy diagnosable rather than ambient noise. MiniLM must be
scored at its native max_seq_length=256: at 512 it loses 1.4 points
(FEVER −5.4, FiQA −3.7) and our margin over it is inflated.

The system beats the models, but by less than we claimed: the pipeline
outscores MiniLM-L6 — the most widely deployed transformer embedder —
with no neural net in the query path, and beats the best static model by
6.7 points, at ~100× less query compute. It remains 5.0 points behind
bge-small. Wins outright on 2/13 (DBPedia, NFCorpus), beats both
transformers on 3 (adding Touché, where BM25 alone also beats both), and
is the best non-transformer on 9/13. Transformers keep an irreducible
edge on reasoning-shaped tasks where relevance isn't carried by shared
vocabulary at any weighting — ArguAna (−20.8 vs bge, the query's answer
argues the *opposite* by construction), SCIDOCS (−12.3), FEVER (−11.1),
FiQA (−9.8) account for most of the average gap.

The load-bearing internal number: the encoder alone scores 0.486, last
in this field, while the pipeline built on it scores 0.577. That **+9.1
points is architecture** — hybrid fusion plus late interaction — and it
is why a last-place encoder finishes ahead of a mainstream transformer.
Encoder quality, not pipeline design, is where the remaining headroom
is. SIF alone measures slightly below plain mean out-of-domain (0.486 vs
0.494, consistent with the GooAQ cost); its value is in-domain and
inside MaxSim's weights.

Raw per-dataset results: session scratchpad `nanobeir-results.json`,
scripts `nanobeir.py` (main) and `minilm_check.py` (truncation
fidelity). Shareable writeup with glossary and per-dataset table
published as an artifact 2026-07-29.

## Contextual doc-token prototype (2026-07): closed, and what it opened

The proposed ceiling-raiser: store the teacher transformer's PER-OCCURRENCE
token vectors at ingest (bge-small last_hidden_state, pages windowed at 510
tokens), distill the query-side static table as the centroid of each token's
occurrence cloud (shared space by construction), and run the shipped MaxSim
blend unchanged. Python prototype over the v2 gold set, reranking the same
rerank-neutered hybrid top-100 dump at pool 30 / weight 0.7. The static-static
replication reproduced the shipped Rust result (0.789 vs 0.798 live, within
the ±1.3 noise floor), validating the frame.

| condition | query side | doc side | NDCG@10 | known | para | ques |
|---|---|---|---|---|---|---|
| A fused baseline | — | — | 0.727 | 0.749 | 0.744 | 0.689 |
| B static-static (shipped) | ese table | ese table | 0.789 | 0.879 | 0.758 | 0.730 |
| C distilled-static | distilled table | distilled table | 0.796 | 0.874 | 0.772 | 0.740 |
| **D ctx-doc (the proposal)** | distilled table | **stored occurrences** | **0.783** | 0.838 | 0.769 | 0.743 |
| E ctx-ctx | bge tokens | stored occurrences | **0.820** | 0.850 | 0.819 | 0.792 |
| F pooled-bge rerank | bge CLS | bge CLS | 0.753 | 0.747 | 0.792 | 0.721 |

Paired bootstrap (2,000 resamples, per-query NDCG@10): D−C **−1.2 points,
P(Δ>0)=0.04** — storing doc-side context while the query stays static is
*significantly worse* than the free distilled table, not merely equal. C−B
+0.6 is noise (CI spans zero): a table distilled purely from the corpus
teacher reaches parity with ese, no more. E−B **+3.1 [CI +1.3,+5.0],
P=1.000, 88W/48L** — token-level context is a real win only when BOTH sides
have it, and the gain lands exactly where v2 said the headroom is
(paraphrase +6, question +6, known-item −3 on an already-saturated workload).
The "free retriever upgrade" (pooled contextual doc vectors + static query)
scored 0.203 brute-force — closed; pooled cross-space mixing fails just like
the projection-alignment spike.

Costs, measured on the 4,000-page corpus (~755 occurrences/page, projected
to the 21,537-page live library):

| doc-side storage | KiB/page | library | D | E |
|---|---|---|---|---|
| fp16 384d | 566 | 12.5 GB | 0.783 | 0.820 |
| int8 384d | 283 | 6.3 GB | 0.784 | — |
| PCA-128 + int8 | 94 | 2.1 GB | 0.791 | 0.809 |
| + SIF prune (keep 81%) | 76 | 1.7 GB | 0.792 | — |
| PCA-64 + int8 | 47 | 1.0 GB | 0.775 | — |

Ingest encode: ~25 pages/s on MPS (~14 min full library), ~9 pages/s on CPU
(~40 min) — vs 149 s for the current full ese re-embed. Query-side bge-small
encode of a real query: 6.1 ms p50 / 11.0 ms p95 on 4 CPU threads.

**Verdict: the two-lane architecture (contextual docs, static queries) is
closed** — it pays gigabytes and minutes to score below its own zero-byte
control. The prototype's real finding is the reframed road: the +3.1 that
survives (0.809 at 2.1 GB compressed) requires a query-side transformer,
and that transformer costs ~6 ms — well inside the sub-100 ms budget. The
open question is engineering, not quality: a 33M-param encoder embedded in
the app (candle/ort, or the librarian sidecar) vs the shipped zero-dependency
static path, for +2–3 NDCG points concentrated in paraphrase/question.
Scripts: session scratchpad `ctxtok_encode.py` / `ctxtok_eval.py` /
`ctxtok_followup.py`, results `ctxtok-results.json`.

## Comparing compile-time ese variants

Quantization (`quant-8`/`quant-16`/f32) and dimension (`dim-*`) are cargo
features of `ese`, fixed at build time — they cannot be runtime flags here.
To compare variants: edit the ese feature list in the workspace
`Cargo.toml`, rebuild, and rerun with a distinguishing `--label`:

```sh
cargo run --release -p retrieval-eval -- encoder --label quant8-dim512 --out q8.json
# edit workspace Cargo.toml: drop quant-8 → rebuild
cargo run --release -p retrieval-eval -- encoder --label f32-dim512 --out f32.json
```

The JSON `meta` block records git rev, seed, corpus sizes, encoder, and
`emb_dim`, so labeled files stay comparable after the fact.

## Relation to the Python NanoBEIR eval

`ese/benches/py/nanobeir.py` (via sentence-transformers) remains the
multi-domain leaderboard for cross-model comparisons — 13 BEIR subsets,
standard tooling, but it needs the maturin wheel and a Python env. This
crate is the fast iteration loop (pure cargo, seconds-to-minutes) and the
only place that measures the *pipeline*, which no encoder-level eval can.
