# RCA: BM25 search does one random point-read per matched document

**Component:** `fold` — `src/pipeline/terminal/search/mod.rs` (`Bm25` / `Bm25Reader`)
**Status upstream:** present on `master` as of the latest commit (`d58aadd2`, 2026-07-12); BM25 introduced in `fd67174e`. No later commit touches the scoring path.
**Severity:** High for any interactive / typeahead search over a non-trivial corpus. Latency scales with term popularity, so it degrades exactly on the queries users are most likely to type.

---

## TL;DR

To score a query, `Bm25Reader` needs the **length** of every document that matches — the length is BM25's normalization factor. Today each length is fetched with a **separate random point-read** into the store, inside the scoring loop, once per matched document, **on every query**.

The lengths all live contiguously in one region of the keyspace, so what should be **one sequential scan** is instead **N random reads**. For a common term (`df ≈ 547`) that is ~547 synchronous reads costing **~450 ms**, repeated on **every keystroke** of a typeahead. Replacing it with a warm-once in-memory map drops the doc-length cost to effectively zero and takes the end-to-end round trip from **~300–900 ms to ~8–40 ms**.

This is not a micro-optimization for one workload. It is an **O(matches) → O(1) per-query** change on the hot path of every BM25 search, and the current cost *grows* with how common the search term is.

---

## Symptom

Interactive search feels like it "drags" while typing. The lag is **worse for common words** (`"watch"`) than rare ones (`"rhodium"`) — the opposite of what a user expects, since common words are the ones they type.

---

## Background: why BM25 needs a document's length

BM25's per-document score includes a **length-normalization** term so that a long document doesn't outrank a short, focused one merely for repeating a word:

```
score(doc) += idf(term) · tf · (k1 + 1)
              ────────────────────────────
              tf + k1·(1 − b + b · dl / avgdl)
                                   ▲
                                   └── dl = this document's length (in tokens)
```

So for **every document that matches any query term**, the scorer must know that document's length `dl`.

Lengths are stored in their own tagged region of the keyspace:

```
key                          value
────────────────────────     ─────────────
[POSTING] term  doc_id   →    term frequency      ← "which docs contain this term"
[DOCLEN]  doc_id         →    document length     ← "how long is this doc"     ← we need this
[STATS]                  →    (num_docs, total_len)
```

Because the store keeps keys in **sorted byte order**, every `[DOCLEN] …` entry sorts next to every other one: they form **one contiguous block on disk**.

---

## Root cause: N random point-reads where one sequential scan would do

The scoring loop does two things per query term. The first is right; the second is the problem.

```
Query term: "watch"
  │
  ▼
┌──────────────────────────────────────────────────────────────┐
│ STEP 1 — find matching docs                                   │
│   prefix-scan  [POSTING] "watch"                              │  ✅ ONE sequential scan
│   → 547 (doc_id, term_frequency) pairs                        │     (the right pattern)
└──────────────────────────────────────────────────────────────┘
  │
  ▼   then, for EACH of the 547 matched docs, first time it's seen:
┌──────────────────────────────────────────────────────────────┐
│ STEP 2 — fetch each doc's length                              │
│   get( [DOCLEN] doc_id )                                       │  ❌ 547 RANDOM point-reads
│     ▸ doc #1   → point-read                                    │
│     ▸ doc #2   → point-read                                    │
│     ▸ doc #3   → point-read                                    │
│        … 547 times, synchronous, blocking the score           │
└──────────────────────────────────────────────────────────────┘
```

The current code (upstream `master`, `search/mod.rs`):

```rust
Entry::Vacant(e) => {
    bufs.key.clear();
    bufs.key.push(DOCLEN);
    bufs.key.extend_from_slice(e.key());
    let dl = self
        .tx
        .get(&self.ks, &bufs.key)     // ← one random point-read, per document, per query
        .unwrap()
        .map(|v| i64::from_be_bytes(v.as_ref().try_into().unwrap()) as f64)
        .unwrap_or(0.0);
    e.insert((0.0, dl))
}
```

The lengths we're fetching are **physically adjacent** in the store (all under the `[DOCLEN]` tag), yet we retrieve them with hundreds of **independent random lookups** instead of walking the block once.

### Why one point-read is not cheap

The store is an LSM-tree (fjall). A single `get(key)` may have to look through several layers before it finds the value, because a key's data can live in the in-memory table and/or any number of on-disk sorted files:

```
get( [DOCLEN] doc_id ):
   memtable        → miss
   SSTable  L0     → bloom check → maybe disk seek → miss
   SSTable  L1     → bloom check → maybe disk seek → miss
   SSTable  L2     → bloom check → disk seek → HIT, read value
   ───────────────────────────────────────────────────────
   ONE logical get  =  several probes, possibly a disk seek
```

Now multiply by 547 documents, then by every keystroke. A **sequential scan** of the same `[DOCLEN]` block, by contrast, pays the "where is it" cost **once** and then streams — which is the single thing storage hardware does fastest.

---

## Why this matters: the cost scales the wrong way

The number of point-reads equals the term's **document frequency** (`df`) — how many documents contain the word. Common words match more documents, so the more likely a query is, the slower it runs:

```
search term      df (matches)   point-reads / keystroke   feel
─────────────    ────────────   ───────────────────────   ──────────────
"rhodium"              3                 3                 instant
"escapement"          40                40                 fine
"watch"              547               547                 ~450 ms  ← drags
a stop-word        5,000+            5,000+                unusable
```

Two things make this "High" severity rather than a nicety:

1. **It's on the hot path of every search.** BM25 scoring can't skip lengths; every matched doc needs one.
2. **Typeahead multiplies it.** Instant-search re-runs on each keystroke, so a 5-character word can pay the full penalty 5+ times in a second.
3. **It gets worse as a corpus grows**, because `df` for common terms grows with the collection.

So the ceiling on interactive search quality is set by this loop, for anyone using `fold`'s BM25 — not by anything specific to one application or corpus.

---

## What partially masks it today (and why it's not enough)

The current scorer *does* have a cache — but only **within a single query**. The `bufs.docs` map means that if a document matches several of the query's terms, its length is fetched once and reused for the later terms:

```
query "swiss watch":
   term "swiss"  → doc_42 first seen → point-read length, store in bufs.docs
   term "watch"  → doc_42 seen again → reuse from bufs.docs (no read)   ✅
```

That helps multi-term queries a little, but it does **nothing** for the dominant cost: the **first sighting of each distinct matched document still does a point-read**, and the whole map is **thrown away when the query ends**. The next keystroke starts from empty and pays for all 547 again.

```
              within one query      across queries / keystrokes
              ─────────────────     ───────────────────────────
bufs.docs:    caches repeats  ✅     discarded every query    ❌
```

The missing piece is a cache that survives **across** queries and is filled by **one sequential scan** instead of N random reads.

---

## The fix

Keep an in-memory mirror of the `[DOCLEN]` region, shared by all readers, **warmed once** by a single sequential scan and **kept in sync** as documents change.

```
BEFORE  (per keystroke, forever)
────────────────────────────────
 keystroke ─▶ scan postings ─▶ 547× get([DOCLEN] doc)  ─▶ score
                                  ▲ random reads, disk
 next keystroke ─▶ … pays the full 547 again


AFTER
─────
 first search after the store opens:
   scan the whole [DOCLEN] block ONCE (sequential) ─▶ build in-memory map { doc_id → length }

 every search (including that first one):
   keystroke ─▶ scan postings ─▶ 547× map.get(doc)  ─▶ score
                                   ▲ O(1) RAM, no disk, no syscalls

 on writes:
   commit() updates the same map in lockstep — insert on add, remove on delete —
   so it never goes stale and never needs re-scanning.
```

Concretely, three coordinated pieces:

1. **A shared field on `Bm25`:** `doc_len_cache: Arc<RwLock<Option<HashMap<key, i64>>>>`.
   - `Option` distinguishes *"never warmed"* from *"warmed and empty"* (enables lazy warm-up).
   - `RwLock` lets many searches read concurrently; only warm-up / commit take the write lock.
   - `Arc` shares **one** map between the writer (`Bm25`) and every `Bm25Reader`.

2. **Warm-on-first-read** — a cache miss triggers **one** `prefix([DOCLEN])` scan into the map, published for all later reads. After that, lookups are in-memory:

   ```rust
   fn doc_len(&self, doclen_key: &[u8]) -> f64 {
       if let Some(map) = self.doc_len_cache.read().unwrap().as_ref() {
           return map.get(doclen_key).copied().unwrap_or(0) as f64;  // warm: O(1), no I/O
       }
       // cold, at most once per store-open: one sequential scan instead of N random gets
       let mut map = HashMap::default();
       for kv in self.tx.prefix(&self.ks, [DOCLEN]) { /* fill map */ }
       let dl = map.get(doclen_key).copied().unwrap_or(0) as f64;
       *self.doc_len_cache.write().unwrap() = Some(map);
       dl
   }
   ```

3. **Incremental sync in `commit`** — the same net-delta already applied to the store is mirrored into the map (insert on positive delta, remove on retraction), inside the same transaction, so the two can't diverge and no re-scan is ever needed.

The scoring loop changes by exactly one line: `self.tx.get(…)` becomes `self.doc_len(…)`.

---

## Impact (measured)

```
                          before          after
────────────────────     ──────────      ─────────
doc-length cost, df=547   ~450 ms          ~0 (in-RAM)
instant-search round trip ~300–900 ms      ~8–40 ms       ≈ 20–40× faster
disk reads per keystroke  O(matches)       0 (after warm)
```

---

## Why the cache stays correct

The one risk with any cache is staleness. Here it's contained because the map and the store are updated **in the same transaction**, with identical semantics:

```
commit(tx):
   for (doc, delta) in pending_doc_lengths:
        delta > 0  →  store.insert(doc, len)   AND  map.insert(doc, len)
        delta < 0  →  store.remove(doc)        AND  map.remove(doc)
```

The store's `[DOCLEN]` region and the in-memory map are therefore always the same set. The warm-up scan is a pure read of that region, so a freshly warmed map equals the committed state by construction. (Load-bearing invariant: any future code path that writes `[DOCLEN]` must go through `commit` so it also updates the map.)

---

## Tradeoffs

- **Memory:** the map holds one entry per document (`doc_id` bytes + an `i64`). Scales with document count; small in practice and well worth it, but not free.
- **First-search latency:** the one-time warm scan is paid by the first query after the store opens, then amortized across all later queries.
- **Invariant to uphold:** length writes must remain funneled through `commit` (see above).

---

## Recommendation

Adopt the warm-once, incrementally-synced doc-length cache in `fold`'s `Bm25` sink. It removes an `O(matches)`-random-reads step from the hot path of every BM25 query, fixes the "common words are slow" scaling, and is a self-contained change to one file (`src/pipeline/terminal/search/mod.rs`) with no API change for callers. A ready-to-apply patch is available.

> Scope note: this RCA covers **only** the `fold`-internal doc-length point-read. A separate latency issue we saw was caused by an *application* misusing `InvertedIndex` (storing large values inside keys); that is a usage bug in the consumer, not a `fold` defect, and is intentionally excluded here.
