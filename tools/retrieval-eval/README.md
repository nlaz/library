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
cargo run --release -p retrieval-eval -- pipeline --docs 10000
cargo run --release -p retrieval-eval -- fusion --docs 10000
cargo run -p retrieval-eval -- smoke     # offline, asserted floors, exit 1 on fail
```

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
