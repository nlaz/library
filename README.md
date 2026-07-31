# The Library

The Library is an on-device search engine and chat agent for a personal
collection of scanned PDFs — books, catalogs, papers. It reads your
documents, indexes them for combined keyword and meaning-based search, and
answers questions about them with a local model. Nothing leaves the
machine: no cloud, no accounts, no API keys.

It is built on three reusable Rust crates — `fold`, `ese`, and `anny` —
that are useful on their own for building fast, incremental, searchable
data stores.

## The big idea

Most systems index in two steps: write the data, then run a job later that
builds indexes from it. The gap between those steps is where bugs live —
indexes drift, deletes leave debris, and "reindex everything" becomes
routine maintenance.

Here, **writing a document is indexing it**. A single write fans out
through a dataflow graph and lands in every index as one atomic
transaction:

```
                          one write transaction
                       ┌──▶ keyword index      (find the word you typed)
   document ─▶ chunks ─┼──▶ vector index       (find the idea you meant)
                       ├──▶ manifest           (what exists, where)
                       └──▶ term dictionary    (typeahead & spell-fix)

              all four update together, or none of them do
```

There is no reindex job, and no code path that could leave two indexes
disagreeing about what exists.

Scanned books are the hard version of this problem, and they shape
everything below:

- There is often **no text layer**, so words are recovered by OCR and must
  carry their pixel coordinates back to the page, so a search hit can
  highlight the actual words on the scan.
- The OCR is **noisy**, so queries have to survive misspellings — in the
  corpus *and* in what you type.
- In a catalog or a cookbook, **pictures carry as much meaning as prose**,
  so figures are indexed as searchable objects, not skipped.

## What it is built on

### ese — text embeddings with no model

An *embedding* turns text into a vector so that similar meanings land near
each other. Normally that takes a neural network: hundreds of megabytes of
weights, a warm-up, a forward pass per input.

`ese` skips the network. It starts from a *static* embedding model — one
where the entire model is a lookup table from token to vector — and bakes
that table into the binary at build time, quantized to one byte per value:

```
   "aluminum extrusion"
         │  normalize, split into word pieces
         ▼
   [alum] [##inum] [extrusion]
         │  perfect-hash lookup, one table row each
         ▼
   ┌─ static table, baked into the binary ─┐
   │  token ─▶ 512 bytes                   │
   └───────────────────────────────────────┘
         │  average the rows
         ▼
   one 512-dimension vector        (~1 µs; no model load, no GPU)
```

Because there is nothing to load and no state, embedding is a **pure
function** — which is what lets it run *inside* the indexing pipeline
rather than as a separate stage before it. And the quantized table is
small enough to stay resident in CPU cache, which is what actually
determines throughput here.

### anny — approximate nearest neighbours

Once every chunk is a vector, "what's similar to my query?" becomes "which
stored vectors are nearest this one?" `anny` answers that with an HNSW
index — a layered graph where each vector links to its near neighbours,
and a query walks greedily from a start point toward wherever is closest.

Two design choices are worth knowing:

**Configuration lives in the type.** Dimensions, distance metric, and
tuning parameters are compile-time constants, so an index built for
512-dim cosine distance is a *different Rust type* from one built for
128-dim L2. Mixing them is a compile error, not a runtime surprise. Queries
also allocate nothing — fixed-size stack arrays, buffers reused across
calls.

**Deletion is real.** Most vector indexes only pretend to delete:

```
   tombstone delete (typical)          repair delete (anny)

        a ─── x̶ ─── b                      a ───── b
              │                             \     /
              c                              \   /
                                               c
   x is marked dead but still            x is unlinked; its neighbours
   routed through and filtered           are cross-linked so the graph
   out at query time — recall            stays navigable — recall holds
   rots as tombstones pile up            as the collection churns
```

Filtered search ("only hits from this document") gets similar care. The
obvious approaches both fail: filtering *after* the search starves — ask
for twenty hits, get three back — and refusing to *visit* excluded nodes
can disconnect the graph, because excluded nodes are often the only path
to included ones. `anny` walks through excluded nodes but only collects
allowed ones:

```
   query ─▶ ○ ─── ○ ─── ● ─── ○ ─── ●     ● allowed: collect
                                           ○ excluded: traverse, skip
```

### fold — incremental dataflow

`fold` is the engine, and it is where "index as you write" actually lives.

Everything moves through it as a **delta**: a record paired with a signed
count.

```
   ("page 12, chunk 3", +1)     insert it everywhere
   ("page 12, chunk 3", −1)     retract it — every index is now
                                exactly as if it had never existed
```

Deletion is not a special code path that has to remember to visit every
index — it is the same push with the sign flipped. That is why a delete
can't be *partial*.

A pipeline is built by composition — each operator owns the one downstream
of it — so an entire graph is a single concrete Rust type:

```
   push(delta)
      │
      ▼
   filter ──▶ map ──┬──▶ BM25 sink        (keyword search)
                    ├──▶ HNSW sink        (vector search)
                    ├──▶ table sink       (manifest)
                    └──▶ dictionary sink  (typeahead terms)

   no scheduler, no queue, no dynamic dispatch —
   a push is a chain of direct calls the compiler sees through
```

`fold` ships the useful sinks: counts, running statistics, key-value
tables, forward and inverted indexes, score-ordered rankings, histograms,
BM25 full-text search, and vector search over `anny`.

The performance idea underneath: stateful sinks **buffer in memory during
a transaction and fold once at commit**. A key touched a thousand times in
one transaction costs one disk write, not a thousand — which is what makes
bulk ingestion cheap without a separate bulk-loading path.

### fjall — the store underneath

Under `fold` sits fjall, an embedded LSM key-value store: ordered bytes on
disk, one writer, many concurrent snapshot readers, no server process.

The key layout is chosen so anything that *can* be a sequential scan *is*
one — postings contiguous under their term, a prefix-scannable term
dictionary, scores encoded so "the top ten" is a range read:

```
   scattered point reads:   ●···●······●··●·····●    hundreds of seeks
   sequential range scan:   ●●●●●●●●●●              one seek, then stream
```

That difference is the difference between a search that feels instant and
one that visibly lags (see the [latency note](#end-to-end-search-latency)
below for the time it bit us).

Reads pin one snapshot across the whole graph — the keyword side and the
vector side can never disagree about which chunks exist — and writes are
atomic across every index at once.

fjall also locks its directory: exactly one process can open a store. The
Library builds on that instead of working around it: **whoever holds the
store owns ingestion**. The background worker exits if the app is running,
and hands finished work off on disk rather than recomputing it.

### How they stack

```
  ┌─────────────────────────────────────────────┐
  │ library-core   search graph, ranking, tools │
  ├─────────────────────────────────────────────┤
  │ fold           deltas in, indexed views out │
  ├──────────────────────┬──────────────────────┤
  │ anny    HNSW graphs  │ fjall   LSM storage  │
  └──────────────────────┴──────────────────────┘
    ese   text → vector, callable from anywhere
```

The layering is strict and acyclic. `anny` knows nothing about storage or
transactions; `ese` is just a function from string to vector; everything
about durability, atomicity, and identity lives in one place — `fold`'s
sink layer. Everything through `library-core` is cross-platform with no
Apple dependencies, so the search engine tests anywhere; only ingestion
and chat need macOS.

## Ingesting a document

The queue is the filesystem — drop a file in the library's folder and it
gets picked up. Every phase caches to disk, so an interruption costs only
the page in progress.

```
  ┌────────┐  ┌─ text layer ┐  ┌────────┐  ┌────────┐  ┌────────┐
  │ render │─▶┤             ├─▶│ chunk  │─▶│ embed  │─▶│ commit │─▶ searchable
  └────────┘  └─ or OCR ────┘  └────────┘  └────────┘  └────────┘
    page        words with       sliding     one vector  four indexes
    images      their boxes      windows     per chunk   updated at once
```

**Render.** Each page becomes an image at a fixed width. The images are
kept — they are what the reader displays and what figure detection runs
on.

**Read.** A born-digital PDF already carries exact text, so its text layer
is used whenever it holds enough to be real. A scan doesn't, so the page
image goes through the system OCR. Either way the output is the same
shape: words, each with its bounding box, in reading order.

**Clean (optional).** OCR of old print fails in predictable ways. A small
on-device model proposes corrections page by page; they are re-checked
locally and applied as a sparse overlay, so the original OCR is never
overwritten and a bad pass can be discarded. Hyphenated line-breaks are
rejoined deterministically.

**Chunk.** Words are grouped into overlapping windows, bounded to one
page:

```
   page text:   w1 w2 w3 w4 w5 w6 w7 w8 w9 ...
   chunk 1:     [w1 ─────────── w6]
   chunk 2:              [w4 ─────────── w9]      ← overlap, so a passage
   chunk 3:                       [w7 ── ... ]      straddling a boundary
                                                    is still findable
```

Page-bounding matters because a hit has to point somewhere a reader can
actually be sent.

**Embed.** One vector per chunk — a batch table lookup via `ese`, inline
in the pipeline, not a staged inference job.

**Commit.** The prepared chunks are diffed against what the store already
holds for this document, and only the difference is applied, in one
transaction. Re-ingesting an unchanged document is nearly free;
re-ingesting a corrected one leaves nothing behind.

Figures travel a parallel track over the same rendered pages, into a store
of their own:

```
  ┌────────┐  ┌────────┐  ┌────────┐
  │ detect │─▶│ embed  │─▶│ commit │─▶ figures searchable
  └────────┘  └────────┘  └────────┘
    regions     same space  a second
    on a page   as text     store
```

A layout model marks pictures, tables, and formulas; a word-gap heuristic
catches regions the model misses; mostly-blank regions are dropped. Each
crop is embedded with an image model whose text and image encoders share
one vector space — that shared space is the whole trick, because it lets a
typed English query be compared directly against a picture with no words
anywhere near it.

The two detectors are not redundant. Measured over 183 pages of this
corpus, **69% of the figures the layout model finds sit in places the
word-gap heuristic cannot look** — a heuristic that reads gaps between
words is blind to anything embedded in a text column, and structurally
blind to tables, which are full of words. On dense catalog spreads the gap
is eightfold (43 figures against 5). The heuristic earns its place the
other way round: it catches full-bleed spreads the model whiffs, and it is
what keeps figure search working at all if the model is unavailable.

Two structural notes:

- Ingestion is split into a **prepare** phase that touches no store and a
  brief **commit** phase that does. All the expensive work happens without
  the lock, so the background worker and the app never fight over it — and
  if the store is taken when a document finishes, the prepared records
  wait on disk for whoever holds the lock to commit. Nothing is
  recomputed.
- A Markdown edition of every document is written alongside the indexes;
  that reading-order text is what the chat agent quotes from.

## Answering a query

Search runs on every keystroke, so the budget is a single-digit number of
milliseconds. The whole query path follows from that.

```
  ┌────────┐  ┌── lexical ──┐  ┌──────┐  ┌───────────┐  ┌───────┐
  │ expand │─▶┤             ├─▶│ fuse │─▶│ diversify │─▶│ shape │─▶ results
  └────────┘  └── semantic ─┘  └──────┘  └───────────┘  └───────┘
    typeahead   run together     combined  drop near-     snippet,
    and fuzzy   on one snapshot  by rank   duplicates     word boxes,
    correction                                            crop rect
```

**Expand.** A live-typed query is usually incomplete, and a query against
OCR is often misspelled on one side or the other. Both are fixed against a
dictionary of terms that *actually exist in this corpus*:

```
   typed:     rhodum micro▌
   expanded:  rhodum  +rhodium          (edit-distance fix; corpus term)
              micro   +microscope       (prefix completion)

   terms are only ever added, never substituted —
   a correctly spelled query passes through untouched
```

**Two tracks.** The keyword ranker (BM25) scores by term overlap, weighted
so rare words count more and long chunks don't win on length. The semantic
ranker embeds the query and walks the vector graph. They answer different
questions — one finds *the word you typed*, the other *the idea you meant*
— and they fail in different places: keywords are helpless against
vocabulary you didn't guess, vectors are vague about exact strings. Both
read the same pinned snapshot.

**Fuse.** The two lists are combined by **rank, not score** — a BM25 score
and a vector distance are not comparable quantities, and blending them
numerically means inventing a conversion and tuning it forever.
Reciprocal rank fusion uses only each item's *position* in each list:

```
              keyword rank    semantic rank    fused
   chunk A         #1              #3          top    — both liked it
   chunk B         #2              —           middle — keywords only
   chunk C         —               #1          middle — meaning only
```

**Diversify.** A book repeats itself, and a fused top-ten is often ten
near-identical passages from adjacent pages. A diversity pass trades a
little relevance for coverage — but only on library-wide searches. Inside
a single document, where you want *every* occurrence, suppressing
near-duplicates is exactly wrong.

**Shape.** Each hit becomes something renderable:

```
   page scan
   ┌────────────────────────────┐
   │  ┌───────────────────────┐ │ ← crop rect: zoom past the margins
   │  │ …the frame was cast   │ │
   │  │ in ▓▓▓▓▓▓▓▓ alloy and │ │ ← matched words highlighted by
   │  │ finished by hand…     │ │   their OCR bounding boxes
   │  └───────────────────────┘ │
   └────────────────────────────┘
     + a text snippet windowed around the first match
```

Figures are searched at the same time on their own thread, so they hide
entirely under the text search rather than adding to it. They are merged
positionally — dealt into the stream at a steady cadence, each figure's
slot decided by a hash of its identity:

```
   text hits:   t1  t2  t3  t4  t5  t6  t7 …
   figures:            f1              f2
   merged:      t1  t2  f1  t3  t4  t5  f2  t6 …

   slots are stable: loading more text results never
   shuffles the figures already on screen
```

One consequence of the per-keystroke budget: there is **no debouncing**.
Waiting to see if you've stopped typing costs more latency than simply
answering, so every keystroke gets an answer — and superseded answers are
dropped on arrival rather than rendered.

## Benchmarks

Everything below was measured on a **base Apple M3 — 8 cores, 8 GB of
RAM**, macOS 26.3: deliberately ordinary hardware, because the point is
what the architecture makes possible on machines people actually own.

The corpus is 61 documents, 22,843 rendered pages, a 1.9 GB text store and
a 580 MB figure store.

### Vector search

`anny` on SIFT10K (10,000 base vectors, 128 dimensions, ef_search 64), at
**recall@10 = 0.990** — latency without a recall figure next to it is
meaningless, so the harness reports both.

| operation            | time    | throughput      |
| -------------------- | ------- | --------------- |
| build (10k vectors)  | 940 ms  | 10.6K vectors/s |
| query                | 20.8 µs | 48.2K queries/s |
| delete (1k vectors)  | 159 ms  | 6.3K removals/s |

The deletion number is doing real work — unlinking each node and repairing
its neighbourhood, not dropping a tombstone. That 159 µs per removal is
what buys an index whose recall doesn't rot as the collection churns.

### Text embedding

`ese`, 512 dimensions, weights quantized to one byte.

| workload                   | time    | throughput       |
| -------------------------- | ------- | ---------------- |
| batched (100k sentences)   | 192 ms  | 520K sentences/s |
| single sentence, 10 chars  | 507 ns  | —                |
| single sentence, 50 chars  | 1.33 µs | —                |
| single sentence, 100 chars | 2.96 µs | —                |

Roughly a microsecond for a typical query — embedding is cheaper than the
storage read that follows it, which is what makes "no forward pass"
concrete.

### Ingestion

Measured across the 2,064 pages in this corpus whose ingest runs actually
did work (no-op resweeps excluded, since averaging real work over skipped
pages would flatter the numbers).

| stage                       | ms/page |
| --------------------------- | ------- |
| figure detection            | 235     |
| reading (text layer or OCR) | 133     |
| figure embedding            | 29      |
| text embedding              | 14      |
| commit — text               | 12      |
| commit — figures            | 4       |
| **total**                   | **429** |

Two things stand out. Reading is strongly bimodal — ~78 ms for a usable
text layer vs ~507 ms for OCR, the single best reason to prefer a
born-digital PDF over a scan of the same book. And **indexing is a
rounding error**: embedding plus committing to all four indexes is under
7% of the total. Almost the entire cost is recovering content from pixels,
which is what makes indexing on every write viable instead of a nightly
job.

### End-to-end search latency

Not tabulated above, because it needs a running server against a live
store. The one recorded measurement comes from [an investigation into the
keyword ranker][rca], which found it fetching document lengths as hundreds
of scattered point reads per keystroke. Replacing that with a single
warm-once sequential scan took the round trip from **~300–900 ms to
~8–40 ms** — the thesis of the whole design in one measurement: the work
was not too expensive, it was happening at the wrong time.

[rca]: docs/rca-bm25-doclen-point-reads.md

## Installing

Download [`TheLibrary-macos-arm64.dmg`][dmg] from the latest release and
drag the app to `/Applications`. It needs macOS 14 or newer on Apple
silicon. Reading, OCR, figure indexing and search are Vision and PDFKit
calls that have been there since 10.15, and ONNX Runtime links statically,
so the floor is set by what has been tested rather than by what is
reachable. Only asking the librarian needs macOS 26, because that is where
Apple Foundation Models arrived: the app checks for them at startup and
leaves the chat out where they're absent, rather than offering a door that
opens onto an error.

The build is signed with a Developer ID certificate and notarized by
Apple, with the ticket stapled to both the app and the disk image, so it
opens like anything else you install — no quarantine step, and no first
launch that has to be argued with. Releases before 0.1.0 were ad-hoc
signed and do need that argument; the fix for one already downloaded is
`xattr -dr com.apple.quarantine '/Applications/The Library.app'`.

The first launch does take a while on a machine that has never run it:
the layout and image models come down from HuggingFace before the first
book can be read. The launch screen says how far along that is.

[dmg]: https://github.com/nlaz/library/releases/latest/download/TheLibrary-macos-arm64.dmg

## Working on this

[`AGENTS.md`](AGENTS.md) covers building, testing, and the conventions
this repository follows.

`examples/` holds standalone databases built on `fold`, `ese`, and `anny`
alone, with no trace of The Library in them — a persistent counter, an
incrementally-aggregated time series, a websocket-backed chat, and text
search three ways over one document stream.
