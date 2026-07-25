# The Library

The Library is an on-device search engine and chat agent for a personal
collection of scanned PDFs — books, catalogs, papers. It reads your
documents, indexes them for combined keyword and meaning-based search, and
answers questions about them with a local model. Nothing leaves the machine:
no cloud, no accounts, no API keys.

It is built on three reusable Rust crates — `fold`, `ese`, and `anny` — that
are useful on their own for building fast, incremental, searchable data
stores.

## The idea

Most search systems treat indexing and querying as separate worlds: you
write your data somewhere, then a job comes along later and builds indexes
out of it. That gap is where the bugs live. Indexes drift out of sync with
the data, deletes leave debris behind, and "reindex everything" becomes a
routine operation instead of an emergency.

The Library takes the opposite position: **do as much work as possible as
early as possible, so that reads are cheap.** Writing a document *is*
indexing it. A single write pushes through a statically-composed dataflow
graph and lands in every index — keyword, vector, manifest, dictionary — as
one atomic transaction. There is no reindex job, and no code path that could
leave two indexes disagreeing about what exists.

Scanned books are the hard version of this problem, and they shape every
decision below. There is often no text layer, so the words must be
recovered by OCR and carry pixel coordinates back to the page so hits can be
highlighted. The OCR is noisy, so queries have to survive misspellings on
both sides. And in a catalog or a cookbook the pictures carry as much
meaning as the prose, so figures are indexed as first-class objects rather
than skipped.

## What it is built on

### ese — text embeddings with no model

`ese` turns a string into a vector, and it does it without running a model.

A conventional embedding model loads hundreds of megabytes of weights and
runs a transformer forward pass per input. `ese` starts from a *static*
embedding model — one where the entire model is a single lookup table from
token to vector — and compiles that table directly into the binary. At build
time the weights are downloaded, quantized down to one byte per value,
truncated to a shorter vector, and flattened together with the tokenizer
into a perfect hash function. Encoding a string is then: normalize it, split
it into word pieces, look up each piece, and average.

The consequences matter more than the speed. There is no model to load, so
search works the instant the store opens rather than after a warm-up. There
is no runtime, no GPU, and no state — which makes embedding a **pure
function**, and that is what lets it run *inside* the indexing pipeline
rather than as a separate step before it. And because the table is quantized
to single bytes, it is small enough to stay resident in cache, which is what
actually determines throughput here — this is a memory problem, not an
arithmetic one.

### anny — approximate nearest neighbours

`anny` is a hierarchical navigable small world (HNSW) index: a layered
proximity graph you descend greedily to find the vectors nearest a query.

Two design choices distinguish it. First, every tuning parameter is a
compile-time constant, so the entire configuration of an index lives in its
type. A graph built for 512-dimensional vectors under cosine distance is a
different type from one built for 128 dimensions under L2, and mixing them
is a compile error rather than a runtime surprise. Second, queries allocate
nothing: the search frontier is a fixed-size array on the stack, and the
"have I seen this node" set is a buffer reused across queries and cleared by
bumping a counter instead of being rewritten.

The feature that matters most for a library that changes is deletion. Most
vector indexes only pretend to delete — they mark a node dead and filter it
out at query time, so the graph slowly fills with tombstones and recall
degrades as the collection churns. `anny` removes the node for real and
repairs the hole, cross-linking the neighbours that were relying on it so
the graph stays navigable. Deleting a document actually deletes it.

Filtered search is handled with similar care. The obvious approach — run a
normal search and throw away results that fail the filter — starves: you ask
for twenty hits within one document and get back three, because the other
seventeen belonged to other documents. Filtering inside the walk has the
opposite failure: excluded nodes are often the only path to the included
ones, so refusing to traverse them disconnects the graph. `anny` routes
*through* excluded nodes while collecting only the allowed ones.

### fold — incremental dataflow

`fold` is the engine, and it is where the "index as you write" idea
actually lives.

Data moves through it as **deltas**: a record paired with a signed count.
`+1` inserts, `-1` retracts, and the governing invariant is that pushing a
record and later pushing it again with the opposite sign leaves every index
exactly as it was. Deletion is not a special case with its own code path —
it is the same push with the sign flipped, which is why it is hard for a
delete to be incomplete.

A pipeline is built by composition: each operator owns the operator
downstream of it, so an entire graph — filters, maps, and the indexes at the
leaves — is one concrete Rust type. There is no scheduler, no work queue,
and no dynamic dispatch; pushing a delta is a chain of direct calls the
compiler can see through end to end. The shape of the graph is also the
shape of its reader, so reading from a four-index graph destructures into
exactly four readers.

Indexes are just sinks on that graph, and `fold` ships a useful set: counts,
running statistics, key-value tables, forward and inverted indexes,
score-ordered rankings, histograms, BM25 full-text search, and vector search
over `anny`. Different sinks handle retraction differently — counting sinks
accumulate signed multiplicities so deltas cancel exactly, while posting
sinks decide membership by net sign — but the guarantee is the same from
outside.

The performance idea underneath is that stateful nodes **buffer in memory
during a transaction and fold once at commit**. A key touched a thousand
times in one transaction costs one write, not a thousand. This is what makes
bulk ingestion cheap without a separate bulk-loading path.

### fjall — the store underneath

Underneath `fold` is fjall, an embedded log-structured merge-tree: an
ordered key-value store with one writer and many concurrent snapshot
readers, running in-process with no server.

`fold` is a typed, incremental layer over ordered bytes, and the layout is
chosen so that anything that *can* be a sequential scan *is* one. Keyword
postings sit contiguously under their term. The term dictionary is stored
with raw keys precisely so prefix scans work on it. Ranked sinks encode
scores in an order-preserving form, so "the top ten" is a range read rather
than a sort. This is not incidental tuning — the difference between a
sequential scan and a few hundred scattered point reads is the difference
between a search that feels instant and one that visibly lags.

Reads pin a single snapshot across the whole graph, so the keyword side and
the vector side can never disagree about which chunks exist. Writes are
atomic across every index at once.

fjall takes a lock on its directory, so exactly one process can open a store
at a time. Rather than work around this, The Library builds on it: the rule
is that whoever holds the store owns ingestion. The background worker exits
immediately if the app is running, and if the app starts mid-run the worker
hands off its finished work on disk rather than recomputing it.

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

The layering is strict and acyclic, and it buys three things.

Search algorithms stay decoupled from persistence. `anny` knows nothing
about transactions, documents, or storage — it is a graph over fixed-size
arrays. `ese` knows nothing about anything; it is a function from string to
vector. Everything about durability, atomicity, and identity lives in one
place, `fold`'s sink layer, which is why the tricky parts (the vector graph
lives in memory, its vectors live on disk, and a crashed transaction must
rebuild it) are contained in a single file instead of smeared across the
app.

Incrementality becomes a property of the system rather than of each index.
Because the keyword index, the vector index, the manifest, and the term
dictionary are all just nodes on one delta stream, updating a chunk updates
all four atomically and removing it retracts it from all four.

And each layer is independently testable. The whole stack up through
`library-core` is cross-platform and has no Apple dependencies, so the
search engine can be tested anywhere; only ingestion and chat need macOS.

## Ingesting a document

Ingestion turns a file into indexed chunks. The queue is the filesystem —
dropping a file into the library's folder is enough to schedule it — and
every phase is cached on disk, so interrupting the process costs only the
page it was working on.

```
  ┌────────┐  ┌─ text layer ┐  ┌────────┐  ┌────────┐  ┌────────┐
  │ render │─▶┤             ├─▶│ chunk  │─▶│ embed  │─▶│ commit │─▶ searchable
  └────────┘  └─ or OCR ────┘  └────────┘  └────────┘  └────────┘
    page        words with       sliding     one vector  four indexes
    images      their boxes      windows     per chunk   updated at once
```

**Render.** Each page is rasterized to an image at a fixed width. These are
not a byproduct — they are what the reader displays and what figure
detection runs against, so they are kept.

**Read.** Getting words off a page has two cases. A born-digital PDF already
carries a text layer, which is exact and free, so it is preferred whenever
it holds enough text to be real rather than a scanner's stray metadata. A
scan has no usable layer, so the page image goes through the system's OCR.
Either way the output is the same shape: a list of words, each with its
bounding box on the page, in reading order. Those coordinates are what let a
search hit highlight the exact words on the scan later.

**Clean (optional).** OCR of old print is noisy in predictable ways. A small
on-device model proposes corrections page by page, which are re-checked
locally before being applied as a sparse overlay — the original OCR is never
overwritten, so a bad cleanup pass can be discarded. Hyphenated line breaks
are rejoined deterministically and always.

**Chunk.** Words are grouped into overlapping windows in reading order,
bounded to a single page. Overlap matters because a passage that straddles a
window boundary would otherwise be findable by neither half. Page-bounding
matters because a hit has to point somewhere a reader can actually be sent.

**Embed.** Each chunk gets a vector. Because `ese` is a pure table lookup
this is a fast batch operation rather than an inference pass, and it happens
inline in the pipeline rather than as a staged job.

**Commit.** The prepared chunks are diffed against what the store already
holds for that document, and the difference is applied in one transaction:
new chunks inserted, stale chunks retracted, all four indexes updated
together. Re-ingesting an unchanged document is nearly free, and re-ingesting
a corrected one leaves nothing behind.

Figures travel a parallel track over the same rendered pages, into a store
of their own:

```
  ┌────────┐  ┌────────┐  ┌────────┐
  │ detect │─▶│ embed  │─▶│ commit │─▶ figures searchable
  └────────┘  └────────┘  └────────┘
    regions     same space  a second
    on a page   as text     store
```

A layout model marks pictures, tables, and formulas, and a geometric
heuristic based on word gaps finds regions the model misses; the union is
filtered to drop regions that are mostly blank. Each region is cropped and
embedded with an image model whose text and image encoders share one vector
space — which is the whole trick, because it means a typed English query can
be compared directly against a picture with no words anywhere near it.

Two structural notes. First, ingestion is split into a **prepare** phase
that touches no store and a **commit** phase that holds it briefly: all the
expensive work happens without the lock, so the background worker and the
app never fight over it. If the store is taken when a document finishes, the
prepared records are written to disk for whoever holds the lock to commit —
nothing is recomputed. Second, a Markdown edition of every document is
written alongside the indexes; that reading-order text is what the chat
agent quotes from.

## Answering a query

Search runs on every keystroke. The budget is therefore a single-digit
number of milliseconds, and the entire design of the query path follows from
that.

```
  ┌────────┐  ┌── lexical ──┐  ┌──────┐  ┌───────────┐  ┌───────┐
  │ expand │─▶┤             ├─▶│ fuse │─▶│ diversify │─▶│ shape │─▶ results
  └────────┘  └── semantic ─┘  └──────┘  └───────────┘  └───────┘
    typeahead   run together     combined  drop near-     snippet,
    and fuzzy   on one snapshot  by rank   duplicates     word boxes,
    correction                                            crop rect
```

**Expand.** A query typed live is usually incomplete, and a query against
OCR is often misspelled on one side or the other. Both are handled against a
dictionary of the terms that actually exist in the corpus, maintained
incrementally like every other index. The word being typed is extended by
prefix, so `micro` finds `microscope` before you finish it. Words that
appear nowhere in the corpus get bounded edit-distance corrections. Terms
are only ever *added* to the query, never substituted, so a correctly spelled
query is passed through untouched.

**The two tracks.** The keyword ranker scores documents by term overlap,
weighted so that rare words count more than common ones and long chunks
don't win on length alone. The semantic ranker embeds the query and walks
the vector graph for nearest neighbours. They answer genuinely different
questions — one finds the word you typed, the other finds the idea you meant
— and they fail in different places: keyword search is helpless against
vocabulary you didn't guess, and vector search is vague about exact strings.
Both read from the same pinned snapshot.

**Fuse.** The two result lists are combined by **rank, not by score**. This
is the important detail: a keyword relevance score and a vector distance are
not comparable quantities, and any attempt to blend them numerically means
inventing a conversion and then tuning it forever. Reciprocal rank fusion
sidesteps this by discarding the scores entirely and using only each item's
position in its own list, so a result that both rankers liked rises above
one that only a single ranker loved.

**Diversify.** A book repeats itself, and the top of a fused list is often
ten near-identical passages from adjacent pages. A diversity pass trades a
little relevance for coverage, penalizing candidates that are too similar to
what has already been chosen. It runs only on full library-wide searches;
inside a single document, where the user is looking for every occurrence,
suppressing near-duplicates is exactly wrong.

**Shape.** Finally each hit is turned into something renderable: a snippet
windowed around the first matched word, the bounding boxes of the matched
words so they can be highlighted on the page, and a crop rectangle that
zooms past the scan's margins to the text that matters.

Figures are searched at the same time, on their own thread:

```
  ┌────────┐  ┌────────────┐
  │ embed  │─▶│ neighbours │─▶ dealt into the same list
  └────────┘  └────────────┘
    the query   nearest
    a vector    figures
```

Because the image track shares nothing with the text track but the query
itself, it runs concurrently and disappears entirely under the text search
rather than adding to it. Merging is positional rather than score-based, for
the same reason fusion is: an image similarity and a fused text rank have no
common scale. Figures are instead dealt into the stream at a steady cadence,
with each one's exact slot decided by a hash of its identity. This keeps the
order stable — extending the text results never shuffles the figures already
on screen, which is what makes endless scrolling work without items jumping
around.

One consequence of the per-keystroke budget worth calling out: results are
sent for every keystroke with no debouncing, and superseded answers are
dropped on arrival rather than rendered. Waiting to see if the user has
stopped typing costs more latency than simply answering, and rendering
answers that are already stale starves the input box of the main thread.

## Benchmarks

Everything below was measured on a **base Apple M3 — 8 cores, 8 GB of RAM**,
macOS 26.3. That is a deliberately ordinary machine; the point of this
section is what the architecture makes possible on hardware people actually
own, not what it can reach on a workstation.

The corpus these numbers describe is 61 documents, 22,843 rendered pages, a
1.9 GB text store and a 580 MB figure store.

### Vector search

`anny` on SIFT10K (10,000 base vectors, 128 dimensions, ef_search 64),
measuring **recall@10 = 0.990** — latency without a recall figure next to it
is meaningless, so the harness reports both.

| operation            | time    | throughput      |
| -------------------- | ------- | --------------- |
| build (10k vectors)  | 940 ms  | 10.6K vectors/s |
| query                | 20.8 µs | 48.2K queries/s |
| delete (1k vectors)  | 159 ms  | 6.3K removals/s |

The deletion number is the interesting one, because it is doing real work:
unlinking the node and repairing its neighbourhood, not marking a tombstone
and moving on. Paying 159 µs per removal is what buys an index whose recall
does not rot as the collection churns.

### Text embedding

`ese`, 512 dimensions, weights quantized to one byte.

| workload                   | time    | throughput       |
| -------------------------- | ------- | ---------------- |
| batched (100k sentences)   | 192 ms  | 520K sentences/s |
| single sentence, 10 chars  | 507 ns  | —                |
| single sentence, 50 chars  | 1.33 µs | —                |
| single sentence, 100 chars | 2.96 µs | —                |

Half a million sentences per second on eight cores, and roughly a
microsecond for a typical query. This is the number that makes the "no
forward pass" claim concrete: embedding is not a stage you schedule around,
it is cheaper than the storage read that follows it.

### Ingestion

Measured across the 2,064 pages in this corpus whose ingest runs actually
did work; documents whose recorded run was a no-op resweep are excluded,
since averaging real work over skipped pages would flatter the numbers.

| stage                       | ms/page |
| --------------------------- | ------- |
| figure detection            | 235     |
| reading (text layer or OCR) | 133     |
| figure embedding            | 29      |
| text embedding              | 14      |
| commit — text               | 12      |
| commit — figures            | 4       |
| **total**                   | **429** |

Two things stand out. Reading is strongly bimodal: pages with a usable text
layer cost around 78 ms, while pages that need OCR cost around 507 ms — a
6× difference, and the single best reason to prefer a born-digital PDF over
a scan of the same book. And **indexing is a rounding error**. Embedding and
committing to all four indexes together come to under 30 ms per page, under
7% of the total. Almost the entire cost of ingestion is recovering content
from pixels; maintaining the indexes incrementally is nearly free, which is
what makes it viable to do on every write instead of in a nightly job.

### A note on end-to-end search latency

Not tabulated above, because it needs a running server against a live store
rather than a bench harness. The one recorded measurement comes from
[an investigation into the keyword ranker][rca], which found it was fetching
document lengths as hundreds of scattered point reads per query — one per
matched document, on every keystroke. Replacing that with a single warm-once
sequential scan took the instant-search round trip from **~300–900 ms to
~8–40 ms**. That is the thesis of this whole design in one measurement: the
work was not too expensive, it was merely happening at the wrong time.

[rca]: docs/rca-bm25-doclen-point-reads.md

## Working on this

[`AGENTS.md`](AGENTS.md) covers building, testing, and the conventions this
repository follows.

`examples/` holds standalone databases built on `fold`, `ese`, and `anny`
alone, with no trace of The Library in them — a persistent counter, an
incrementally-aggregated time series, a websocket-backed chat, and text
search three ways over one document stream.
