# How our search compares to transformer models

**External calibration of The Library's retrieval stack — July 2026**

We built a search system with no neural network in the query path. This report
measures it against the models it deliberately avoids, on 13 public benchmarks,
with every system scored by identical code. It assumes no prior knowledge of the
system or the techniques.

**In one line:** our architecture buys a real transformer's worth of quality from
last-place embeddings, and a specific, measurable amount is still missing.

| | |
|---|---|
| **Our pipeline** | **0.591** NDCG@10, averaged over 13 datasets |
| vs all-MiniLM-L6-v2 | **+2.9 points** — ahead of the industry's default embedder |
| vs bge-small-en-v1.5 | **−3.6 points** — behind a strong modern transformer |
| Architecture's contribution | **+10.5 points** over our own embedding table |

Three findings matter for planning:

1. **The system beats the models; the embeddings don't.** Our embedding table alone
   scores 0.486 — last in this field. The pipeline built on it scores 0.591. That
   +10.5 comes from architecture (keyword search, score fusion, late-interaction
   reranking), not from embeddings. It is why a weak static encoder finishes ahead
   of a mainstream transformer.
2. **We are at parity with the deployed-transformer tier, not the frontier tier.**
   Both halves of that sentence should always travel together. Quoting only the
   MiniLM win overstates our position.
3. **Our wins and losses are structural.** We win where relevance is carried by
   shared vocabulary and lose where it requires semantic reasoning the words don't
   contain. This predicts which product surfaces we serve well.

A fourth finding emerged while investigating the third, and is arguably the most
actionable thing here: **the rarity weighting our ranking depends on comes from a
property of the embedding table nobody designed, and nothing protects it.** See
[The rarity signal](#the-rarity-signal-lives-in-the-row-norms).

---

## Glossary

Assumes no prior context. Skip if these are familiar.

| Term | Meaning |
|---|---|
| **NDCG@10** | The score used throughout. For each query, look at the top 10 results: you get credit for each correct document, discounted by how far down it appears, normalized so a perfect ranking is 1.0 and nothing-relevant is 0. Averaged over queries, then datasets. "1 point" means 0.01. |
| **BEIR / NanoBEIR** | The standard public suite for testing whether retrieval generalizes across domains rather than overfitting one. NanoBEIR is the sampled version — 13 datasets, 50 queries each — comparable to published leaderboard figures. |
| **Embedding** | A list of numbers representing text, positioned so similar meanings sit near each other. Search becomes "find the document vector closest to the query vector." |
| **Static embedding** | Our approach. Every word has one precomputed vector in a lookup table; encoding is table lookups plus averaging — microseconds, no model inference. Tradeoff: a word gets the same vector regardless of context, so *bank* in "river bank" and "savings bank" are identical. |
| **Transformer / bi-encoder** | A neural network that reads the whole passage and produces vectors informed by surrounding words, so *bank* differs per sentence. Far more accurate, thousands of times more expensive per encode. "Bi-encoder" means query and document are encoded separately, so documents can be precomputed. |
| **BM25** | Classical keyword search: score documents by how many query terms they contain, weighting rare terms more and discounting long documents. No machine learning. Still a very strong baseline — it beats both transformers on one dataset here. |
| **Hybrid / fusion** | Running keyword and vector search together and blending their scores. Each covers the other's blind spot: keywords nail exact names and jargon, vectors handle paraphrase. |
| **Late interaction (MaxSim)** | Our reranking step. Instead of comparing one vector per document, compare *every query word* against *every document word* and keep each query word's best match. Catches word-level evidence that averaging into a single vector destroys. We do it with table lookups, so it costs about a millisecond. |
| **SIF weighting** | Weighting words by rarity when averaging them into a vector, so "the" contributes less than "haleem". Baked into our table at build time. |
| **Row norm** | The length of a word's vector in the table (as opposed to its direction, which carries meaning). Turns out to matter a great deal — see the root-cause section. |

---

## What was compared

Seven systems, same data, same scoring code. "Query cost" is the compute needed to
answer one search, which is what our latency budget constrains.

| System | What it is | Query cost | Avg |
|---|---|---|---|
| **ours** | BM25 + static-embedding vector search, score-fused, then MaxSim reranked, with rarity weighting throughout. No neural network anywhere. | table lookups, ~1.4 ms rerank | **0.591** |
| **bge-small-en-v1.5** | Strong modern small transformer embedder (33M params). Represents what we'd gain by putting a neural model in the query path. | full model inference | 0.627 |
| **all-MiniLM-L6-v2** | The most widely deployed sentence embedder in the industry (22M params) — the default in most RAG stacks. Our practical reference point. | full model inference | 0.563 |
| **BM25** | Our own keyword search alone, pipeline's other stages removed. Shows what the non-semantic half contributes. | index scan | 0.542 |
| **potion-retrieval-32M** | Best published static embedding model, tuned for retrieval. The benchmark for our encoder's category. | table lookups | 0.511 |
| **our encoder, plain** | Our embedding table alone, simple averaging. | table lookups | 0.494 |
| **our encoder, SIF** | Our embedding table with rarity weighting — the version inside the pipeline. | table lookups | 0.486 |

---

## Results

NDCG@10 per dataset. **Bold** marks the best system on that row. Each dataset has
50 queries (Touché 49) over 2,200–6,000 documents.

| Dataset | ours | bge-small | MiniLM-L6 | BM25 | potion | enc. plain | enc. SIF |
|---|---|---|---|---|---|---|---|
| ArguAna — find the counter-argument | 0.416 | **0.628** | 0.553 | 0.443 | 0.410 | 0.388 | 0.409 |
| ClimateFEVER — claim → evidence | **0.348** | 0.348 | 0.296 | 0.339 | 0.339 | 0.333 | 0.316 |
| DBPedia — entity lookup | **0.640** | 0.612 | 0.550 | 0.538 | 0.565 | 0.561 | 0.554 |
| FEVER — fact verification | 0.870 | **0.942** | 0.793 | 0.805 | 0.687 | 0.695 | 0.668 |
| FiQA-2018 — financial opinion QA | 0.407 | **0.488** | 0.477 | 0.341 | 0.385 | 0.352 | 0.333 |
| HotpotQA — multi-hop questions | **0.853** | 0.840 | 0.596 | 0.811 | 0.602 | 0.632 | 0.616 |
| MS MARCO — web search passages | 0.516 | **0.632** | 0.554 | 0.486 | 0.410 | 0.382 | 0.377 |
| NFCorpus — medical / nutrition | **0.360** | 0.358 | 0.332 | 0.319 | 0.335 | 0.319 | 0.324 |
| NQ — real Google questions | 0.561 | **0.593** | 0.590 | 0.446 | 0.443 | 0.436 | 0.419 |
| Quora — duplicate questions | 0.933 | **0.962** | 0.937 | 0.791 | 0.897 | 0.899 | 0.886 |
| SCIDOCS — citation prediction | 0.310 | 0.430 | **0.433** | 0.315 | 0.305 | 0.266 | 0.273 |
| SciFact — scientific claims | 0.727 | **0.762** | 0.727 | 0.694 | 0.680 | 0.588 | 0.591 |
| Touché-2020 — argument retrieval | **0.743** | 0.551 | 0.475 | 0.724 | 0.581 | 0.566 | 0.552 |
| **Average** | **0.591** | 0.627 | 0.563 | 0.542 | 0.511 | 0.494 | 0.486 |

Summary: 5 outright wins of 13, ahead of MiniLM on 7, best non-transformer on 11.
Three of the five wins have margins under a point and are ties in practice.

### Where the 3.6-point gap comes from

It is not spread evenly. Difference between our score and bge-small's, in points:

| Dataset | Δ | | Dataset | Δ |
|---|---|---|---|---|
| Touché-2020 | **+19.2** | | NQ | −3.2 |
| DBPedia | **+2.8** | | SciFact | −3.5 |
| HotpotQA | **+1.3** | | FEVER | −7.2 |
| NFCorpus | **+0.2** | | FiQA-2018 | −8.1 |
| ClimateFEVER | **+0.1** | | MS MARCO | −11.6 |
| Quora | −2.8 | | SCIDOCS | −12.0 |
| | | | ArguAna | **−21.1** |

Four datasets — ArguAna, SCIDOCS, MS MARCO, FiQA — account for nearly all of the
deficit.

---

## Reading the results

### Where we win, and why

We are the best system on five datasets: Touché-2020 (argument retrieval, 19.2
points ahead of bge), DBPedia (entity lookup), HotpotQA (multi-hop questions),
NFCorpus (medical), and ClimateFEVER.

The common thread is that relevance is carried by **shared vocabulary** — the right
document contains the entity name, the drug name, the topic term. Our keyword half
finds those exactly and MaxSim confirms them word by word. Notably BM25 *alone*
also beats both transformers on Touché: transformers can actively hurt when precise
term matching is what the task needs.

This matters for us specifically. Searching your own library is mostly this kind of
task — you remember a phrase, a name, a dish, a chapter. Our in-domain evaluation
agrees: on our own corpus the pipeline reaches 0.798, with keyword search alone
nearly matching it on "I know what I'm looking for" queries.

### Where we lose, and why

The worst datasets are those where **the answer shares few words with the
question**. ArguAna (−21.1) asks for the *counter*-argument to a passage — a good
answer argues the opposite, using different vocabulary by definition. SCIDOCS
(−12.0) predicts which papers a paper cites from titles alone. MS MARCO (−11.6) and
FiQA (−8.1) connect short conversational queries to passages phrased differently.

No amount of term weighting recovers these. They need a model that understands two
differently-worded sentences mean related things. This is the irreducible part of
the gap.

### What the architecture is worth

Our embedding table alone scores 0.486, below every other system here. The pipeline
built on it scores 0.591. That +10.5 is the value of hybrid search and
late-interaction reranking — and it is what lets a last-place encoder finish ahead
of a mainstream transformer.

Put plainly: **our engineering advantage is real and our embeddings are genuinely
weak.** Both facts should inform where we invest. Improving the encoder has more
headroom than improving the pipeline, because the pipeline is already extracting
near its theoretical value from a poor signal.

---

## The rarity signal lives in the row norms

This began as a loose end. Our SIF-weighted encoder scores *below* the plain one
(0.486 vs 0.494), which looked like a wart worth explaining. Ablating it turned up
something more fundamental.

**Rarity weighting is essential.** Strip it entirely — unit-normalize every row
before averaging, so "the" counts as much as "haleem" — and the encoder collapses
from 0.496 to 0.313. An 18-point hole. Nothing else in this report is worth that
much.

**But the table already does it, for free, through vector length.** Frequent words
have short vectors, rare words long ones. The correlation between log frequency and
row norm is **−0.404**:

| Frequency band | Mean row norm |
|---|---|
| 100 most frequent tokens | 164 |
| next 1,000 | 283 |
| next 10,000 | 399 |
| rarest 5,000 | 495 |

Averaging vectors therefore *already* weights by rarity, automatically, with no
weighting scheme attached. Decomposing the encoder's pooling into the table's own
norms ("implicit") and SIF's frequency formula ("explicit"), over 649 queries:

| Rarity signal used | NDCG@10 | Reading |
|---|---|---|
| implicit only — plain `mean(RAW)` | **0.496** | the norms alone are strongest |
| explicit only — `mean(unit × sif)` | 0.397 | SIF is a *worse* signal, by 9.8 points |
| neither — `mean(unit)` | 0.313 | confirms rarity weighting is worth 18 points |
| both — `mean(RAW × sif)`, ships | 0.486 | rarity counted twice, −1.0 |

That resolves the puzzle: SIF's pooling role adds nothing because **it is redundant
with the table's own geometry**, and being the weaker of the two signals, layering
it on top over-corrects slightly.

**MaxSim is the mirror image, and this is why it needs SIF.** MaxSim compares
*directions*: it normalizes every token vector to unit length before taking the max.
That step discards the norms — and with them the free rarity weighting. So the
protection must be handed back explicitly, which is exactly what our reranker does:
its weight for a token is the length of that token's baked row.

| MaxSim weight source | NDCG@10 | vs unweighted |
|---|---|---|
| unweighted | 0.560 | — |
| SIF factor alone | 0.577 | +1.7 |
| row norm alone | 0.586 | +2.6 |
| **row norm × SIF** (ships) | **0.591** | **+3.1** |

In-domain agrees on direction: MaxSim weighting is worth +2.0 on our own gold set
(95% CI [+1.2, +2.9], P > 0 = 1.000), concentrated in paraphrase (+2.4) and question
(+3.6) queries with known-item flat — precisely where lexical evidence runs out.
SIF's pooling role in-domain is +0.5, CI [−0.7, +1.6], not significant.

**Net accounting.** Rarity weighting is one of the most valuable properties in the
system. **SIF is not where most of it comes from** — the embedding table supplied it
all along through vector length. SIF's marginal contribution is −1.0 in pooling and
+0.5 on top of the norms in MaxSim. Keeping it is justified but marginal, and the
framing in our notes that SIF carried the weighting was wrong.

> **Why this matters beyond SIF.** The row norms are a property inherited from the
> source model, not something we designed, and they carry an 18-point signal across
> two pipeline stages. Any future change that normalizes rows — a new base model, a
> re-quantization, a distillation step — would silently delete it, with no test
> failing. This deserves a regression test in `ese`, not just a note.

---

## Recommendations

- **Keep the current architecture.** At ~100× less query compute than a transformer
  and ahead of the industry default embedder, the hybrid + late-interaction design
  is validated. Nothing here argues for replacing it.
- **Quote both numbers externally.** "Beats all-MiniLM-L6-v2 with no neural network
  in the query path" is true and defensible. It should always be paired with the
  3.6-point gap to bge-small.
- **Target the encoder, not the pipeline.** The measurements localize our weakness
  precisely: the embeddings are last place in this field and the pipeline is already
  compensating hard. Encoder quality is where remaining headroom lives.
- **Add a regression test protecting the row norms.** Rarity weighting is worth up
  to 18 points and comes mostly from an inherited, undesigned property. Pin the
  frequency–norm correlation so a future model swap can't delete it silently.
  *(Not yet implemented — flagged for review.)*
- **Expect a ceiling on paraphrase-shaped queries.** The ArguAna/SCIDOCS class of
  failure is structural to static embeddings. If a product surface needs that
  capability, it needs a model — our separate prototype costed a query-side
  transformer at ~6 ms and ~2 GB storage, inside our latency budget but a genuine
  architectural dependency. See the contextual doc-token prototype in
  `tools/retrieval-eval/README.md`.

---

## Method, corrections, and caveats

### How systems were scored

One harness computes NDCG@10 for every system from a ranked list of document
indices, so no system gets a different metric implementation. Our pipeline is a
line-by-line reimplementation of production (`apps/library-core/src/rank.rs`): BM25
with k1=1.2, b=0.75, top-512, relevance normalized to the query's top hit; vector
search top-40 normalized to the top similarity; fused 50/50; MaxSim rerank over the
top 30 at weight 0.7. Vector search uses exact cosine rather than our HNSW index,
which removes approximation noise and slightly favors us.

### Model configuration

bge-small is CLS-pooled with the retrieval query prefix its authors specify. MiniLM
is mean-pooled at its native 256-token limit. potion uses the model2vec runtime with
truncation disabled. Our MaxSim weights are the norms of the baked SIF rows,
matching `rank.rs`.

### Corrections made during this run

Both were caught by cross-checking against an earlier independent run, and both
moved the headline:

1. **bge-small was previously scored without its retrieval query prefix**, which the
   model needs to rank correctly. Our notes had recorded a 2.1-point gap; the real
   gap is 3.6. The correction is entirely on bge's side — our own score reproduced.
2. **Our replica initially used the SIF factor as MaxSim's weight rather than the row
   norm**, understating our pipeline by 1.4 points (0.577 → 0.591). Separately,
   scoring MiniLM at 512 tokens rather than its native 256 cost it 1.4 points and
   would have inflated our margin; both were fixed before these numbers.

Six of seven systems now agree within 0.003 across two independently written
harnesses (potion 0.511, our encoder 0.494/0.486, MiniLM 0.563, our pipeline 0.591
vs its 0.588). That agreement is the main evidence these numbers are trustworthy —
and it is what allowed the one real disagreement to be localized rather than
dismissed as noise.

### Caveats

- 50 queries per dataset makes per-dataset numbers noisy. Only averages and large
  per-dataset gaps should be read as signal.
- NanoBEIR is out-of-domain for us by construction. It measures generalization, not
  performance on a personal library, where we score considerably higher (0.798 on
  our own gold set).
- Our reimplementation of the pipeline is faithful but not bit-identical to the Rust:
  our BM25 came out 0.6 points above the previously recorded run, and the pipeline
  0.3 points above.

### Reproducing

Scripts live in the session scratchpad (regenerable, not committed):
`nanobeir.py` (main run), `minilm_check.py` (truncation fidelity), `ours_fixed.py`
(production weighting), `sif_ablation.py` / `sif_indomain.py` / `sif_rootcause.py` /
`sif_rootcause2.py` / `maxsim_weights.py` (the rarity investigation). Raw
per-dataset output in `nanobeir-results.json`. Datasets load from the public
`zeta-alpha-ai/Nano*` collections. Full run ~25 minutes on an M-series laptop.

Summary of record: `tools/retrieval-eval/README.md`, commits `a3b837c` and
`39356ce`.

---

*The Library · retrieval quality · run 2026-07-29 · NanoBEIR, 13 datasets, NDCG@10*
