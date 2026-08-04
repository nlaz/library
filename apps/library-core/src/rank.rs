//! Hybrid search: lexical + semantic fused by normalized-score blend, plus
//! MMR diversity.

use fold::pipeline::Scored;
use fold::stream::Readable;
use fxhash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::records::is_reserved;
use crate::text::tokenize;
use crate::{ChunkKey, ChunkRec, Emb, FxHashSet, Readers, Word, dot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub score: f32,
    /// BM25 score relative to the query's best lexical hit (1.0 = top).
    /// Keys with no lexical evidence (semantic-only) get 1.0 — the semantic
    /// list is count-bounded upstream, so it never carries a noise tail.
    #[serde(default)]
    pub rel: f32,
    /// Raw BM25 score (0.0 for semantic-only hits). Unlike `rel`, this is
    /// an absolute signal: a weak query's top hit has a *low* raw score but
    /// still rel = 1.0, so agents gating on "did we really find anything?"
    /// must look here.
    #[serde(default)]
    pub bm25: f32,
    /// 0-based rank in the lexical (BM25) list; `None` = semantic-only —
    /// exactly the hits that take the `rel = 1.0` default above. (The
    /// [`MIN_REL`] cutoff exempts every semantic-list member, not just
    /// these — see its doc.)
    #[serde(default)]
    pub lex_rank: Option<u32>,
    /// 0-based rank in the semantic (HNSW) list; `None` = lexical-only.
    #[serde(default)]
    pub sem_rank: Option<u32>,
    /// Cosine distance from the vector index (lower = closer).
    #[serde(default)]
    pub sem_dist: Option<f32>,
    pub key: ChunkKey,
    pub words: Vec<Word>,
}

/// Pre-fusion ranker list sizes and per-phase timings, reported through the
/// `stats` out-param of [`search`] — the fused hit list alone can't
/// reconstruct them. The timing fields become the perf view's sub-stages of
/// the text search (formerly one opaque "lex+rrf" span).
#[derive(Debug, Clone, Copy, Default)]
pub struct RankerStats {
    pub lex_n: usize,
    pub sem_n: usize,
    /// µs: tokenization + typeahead completion + fuzzy vocabulary correction.
    pub term_expand_us: u64,
    /// µs: BM25 postings search (plus relevance-map assembly).
    pub lex_search_us: u64,
    /// µs: HNSW vector search (0 when the query has no embedding).
    pub vec_search_us: u64,
    /// µs: score fusion + MMR re-rank + hit resolution (primary-table
    /// point-reads).
    pub fuse_us: u64,
}

/// Hits scoring below this fraction of the query's top BM25 hit are noise;
/// the paginated result stream ends here. Tuning knob — the perf view's
/// provenance table is the place to eyeball rel distributions. Callers
/// apply it only to hits *without* semantic-list membership: the semantic
/// list is count-bounded (HNSW K), so its members are never a noise tail,
/// and gating them on weak *lexical* evidence would rank them below
/// semantic-only hits that carry none at all.
pub const MIN_REL: f32 = 0.25;

/// Lexical share of the fused score: `w·lex_rel + (1−w)·sem_rel`, each
/// component normalized to its own per-query top. Chosen by the
/// tools/retrieval-eval fusion sweep (2026-07): 0.5 is the only setting
/// that beats rank-only RRF on every metric across the paraphrase,
/// known-item, and GooAQ question workloads — lower favors paraphrase but
/// regresses known-item recall, higher the reverse.
pub(crate) const FUSE_LEX_WEIGHT: f32 = 0.5;

/// Fused-score multiplier for a note card. A card is prose the reader wrote
/// by hand about this library — a far stronger statement of "this matters to
/// me" than a page the ingester happened to OCR — so it should outrank page
/// chunks carrying comparable textual evidence. Unlike [`FUSE_LEX_WEIGHT`]
/// this is not a sweep result: the retrieval-eval gold set is books only, so
/// there is nothing to measure it against. Deliberately modest — a card
/// still has to earn a place in the fused list before the boost can lift it.
pub(crate) const NOTE_BOOST: f32 = 1.4;

/// How deep to fetch from the lexical ranker regardless of `k`. Pinning the
/// depth pins the lexical list — and therefore the fusion input and the
/// final order — so paginated slices of the same query tile without drift
/// (a growing fetch would change the top-hit normalization and membership
/// and shift ranks between page requests). Also caps stable pagination
/// depth at ~LEX_FETCH lexical + TOP_K semantic hits. BM25 cost is
/// limit-independent (full postings scan, truncate at end), so the extra
/// depth is nearly free.
pub const LEX_FETCH: usize = 512;

/// Nearest real terms substituted per unknown query word.
pub(crate) const FUZZ_CANDIDATES: usize = 3;

/// How many top fused hits the MaxSim late-interaction re-rank rescores.
/// Chosen by the 2026-07-29 pool-depth sweep (raw top-100 dump, pools
/// 10..100): quality peaks at 30 (library gold 0.815/r@1 0.74) and
/// *declines* past 50 — deeper candidates add false-positive surface
/// faster than they add reachable gold. Cost is linear in pool size and
/// still ~ms at 30.
pub(crate) const RERANK_POOL: usize = 30;
/// MaxSim share of the reranked score: `w·maxsim + (1−w)·fused`, both
/// min-max normalized over the pool. Chosen by the 2026-07 reranker spike
/// (tools/retrieval-eval README): 0.7 scored 0.813 NDCG@10 / recall@1 0.73
/// on the library gold set vs 0.768/0.68 unreranked, with every setting in
/// 0.3–0.9 an improvement.
pub(crate) const RERANK_WEIGHT: f32 = 0.7;
/// Query-token cap for the re-rank — keeps agent-length queries bounded.
const RERANK_MAX_QUERY_TOKENS: usize = 32;

/// Normalize in place; returns the original norm (0.0 leaves `v` untouched).
fn normalize(v: &mut Emb) -> f32 {
    let n = dot(v, v).sqrt();
    if n > 0.0 {
        // reciprocal multiply: f32 division has several times the latency of
        // a multiply and does not pipeline, and this runs once per document
        // token vector on every search
        let inv = 1.0 / n;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
    n
}

/// The query's wordpiece tokens as (unit direction, weight). The baked
/// table pre-scales each row by its SIF rarity weight, so the row's norm
/// *is* the token's importance and its direction carries the meaning.
fn query_token_dirs(query: &str) -> Vec<(Emb, f32)> {
    let mut out: Vec<(Emb, f32)> = Vec::new();
    ese::for_each_token_vector(query, |v| {
        if out.len() >= RERANK_MAX_QUERY_TOKENS {
            return;
        }
        let mut dir = *v;
        let w = normalize(&mut dir);
        if w > 0.0 {
            out.push((dir, w));
        }
    });
    out
}

/// MaxSim late-interaction re-rank of the fused top [`RERANK_POOL`]: each
/// query token takes its best cosine against the chunk's token vectors
/// (word-level interaction the pooled bi-encoder averages away), the
/// rarity-weighted mean of those best matches is blended with the fused
/// score, and the pool is re-sorted. Hits past the pool keep fused order,
/// and reranked scores are rescaled into the pool's original score range so
/// the full list stays monotonic.
///
/// Static-table arithmetic only, but not free: a 30-chunk pool of 200-word
/// chunks against a 17-token query is ~95k dot products over EMB_DIM, and
/// the token vectors are rebuilt from the table on every search. Measured
/// ~50ms/call before the 2026-08 pass that vectorized [`dot`] and made the
/// scoring single-pass (an earlier version of this comment claimed
/// "sub-millisecond", which was never true); it is the most expensive stage
/// of a search, so re-measure before growing [`RERANK_POOL`] or
/// [`RERANK_MAX_QUERY_TOKENS`].
pub(crate) fn maxsim_rerank(
    mut fused: Vec<(f32, ChunkKey)>,
    query: &str,
    resolve: &impl Fn(&ChunkKey) -> Option<ChunkRec>,
) -> Vec<(f32, ChunkKey)> {
    let pool = fused.len().min(RERANK_POOL);
    if pool <= 1 {
        return fused;
    }
    let qtok = query_token_dirs(query);
    let wsum: f32 = qtok.iter().map(|(_, w)| w).sum();
    if qtok.is_empty() || wsum <= 0.0 {
        return fused;
    }

    // A word's best dot against each query token depends only on the word and
    // the query, so it is computed once for the whole pool rather than once
    // per chunk that contains the word — and prose repeats heavily, both
    // within a 200-word chunk and across the ~30 chunks of a pool.
    //
    // What is memoized is the *scores* (one f32 per query token), not the
    // token vectors: a word's directions run ~1-2 x EMB_DIM floats, so
    // caching those would cost megabytes per search, while this is ~17
    // floats per distinct word. An empty entry marks a word whose pieces all
    // dequantized to a zero vector.
    let mut memo: FxHashMap<String, Vec<f32>> = FxHashMap::default();
    let maxsim: Vec<f32> = fused[..pool]
        .iter()
        .map(|(_, key)| {
            let Some(rec) = resolve(key) else {
                return 0.0;
            };
            let mut seen: FxHashSet<&str> = FxHashSet::default();
            let mut best = vec![-1.0f32; qtok.len()];
            let mut any = false;
            for w in &rec.words {
                if !seen.insert(w.t.as_str()) {
                    continue;
                }
                if !memo.contains_key(w.t.as_str()) {
                    // Each direction is scored against every query token the
                    // moment it is built, rather than collected into a Vec
                    // that is then re-scanned once per query token — same
                    // maxima (max is order-independent), but `dir` stays in
                    // L1 across the inner loop and the set is never
                    // materialized.
                    let mut m = vec![f32::NEG_INFINITY; qtok.len()];
                    let mut usable = false;
                    ese::for_each_token_vector(&w.t, |v| {
                        let mut dir = *v;
                        if normalize(&mut dir) > 0.0 {
                            usable = true;
                            for (b, (qd, _)) in m.iter_mut().zip(qtok.iter()) {
                                *b = b.max(dot(qd, &dir));
                            }
                        }
                    });
                    memo.insert(w.t.clone(), if usable { m } else { Vec::new() });
                }
                let per_word = &memo[w.t.as_str()];
                if !per_word.is_empty() {
                    any = true;
                    for (b, m) in best.iter_mut().zip(per_word.iter()) {
                        *b = b.max(*m);
                    }
                }
            }
            if !any {
                return 0.0;
            }
            let mut acc = 0.0f32;
            for (b, (_, w)) in best.iter().zip(qtok.iter()) {
                acc += w * b;
            }
            acc / wsum
        })
        .collect();

    // min-max both signals over the pool, blend, and rescale into the
    // pool's fused-score range (keeps the boundary to the un-reranked tail
    // monotonic). Ties keep fused order via the original-index tiebreak.
    let minmax = |xs: &[f32]| -> (f32, f32) {
        xs.iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &x| {
                (lo.min(x), hi.max(x))
            })
    };
    let mm = |x: f32, (lo, hi): (f32, f32)| if hi > lo { (x - lo) / (hi - lo) } else { 0.0 };
    let ms_range = minmax(&maxsim);
    let fused_scores: Vec<f32> = fused[..pool].iter().map(|(s, _)| *s).collect();
    let f_range = minmax(&fused_scores);
    let mut order: Vec<usize> = (0..pool).collect();
    let blended: Vec<f32> = (0..pool)
        .map(|i| {
            RERANK_WEIGHT * mm(maxsim[i], ms_range)
                + (1.0 - RERANK_WEIGHT) * mm(fused_scores[i], f_range)
        })
        .collect();
    order.sort_by(|&a, &b| blended[b].total_cmp(&blended[a]).then(a.cmp(&b)));

    let (lo_f, hi_f) = f_range;
    let reranked: Vec<(f32, ChunkKey)> = order
        .into_iter()
        .map(|i| {
            let score = if hi_f > lo_f {
                lo_f + blended[i] * (hi_f - lo_f)
            } else {
                fused[i].0
            };
            (score, fused[i].1.clone())
        })
        .collect();
    fused.splice(..pool, reranked);
    fused
}

/// How many top fused hits the MMR diversity re-rank considers. Fixed
/// (independent of `k`/`offset`) so greedy selection is deterministic and
/// pagination stays prefix-stable; hits past the pool keep their fused order.
pub(crate) const MMR_POOL: usize = 100;
/// MMR relevance/diversity mix: `score = λ·relevance − (1−λ)·max_similarity`.
/// 1.0 = pure relevance (today); lower demotes near-duplicates harder.
pub(crate) const MMR_LAMBDA: f32 = 0.7;

/// Update each unpicked pool item's running max similarity against one newly
/// selected item — the trick that keeps [`mmr_rerank`] O(pool²) overall.
///
/// `embs` must hold **unit** vectors, with degenerate embeddings stored as
/// `None`; similarity is then a bare [`dot`]. [`mmr_rerank`] normalizes the
/// pool once up front, which turns what was an O(pool²) recomputation of
/// both norms — the same ‖e‖ recomputed on every one of the ~5000 pairs a
/// full pool visits — into O(pool).
pub(crate) fn bump_sim(max_sim: &mut [f32], picked: &[bool], embs: &[Option<Emb>], sel: usize) {
    let Some(se) = &embs[sel] else { return };
    for (i, mi) in max_sim.iter_mut().enumerate() {
        if !picked[i]
            && let Some(e) = &embs[i]
        {
            *mi = mi.max(dot(e, se));
        }
    }
}

/// Maximal Marginal Relevance re-rank of the fused list: greedily reorders the
/// top [`MMR_POOL`] hits to demote near-duplicates (same book/edition — common
/// in scanned corpora), then appends the remainder in fused order. Similarity
/// is cosine between chunk embeddings fetched via `resolve`. Deterministic over
/// the (pinned) fused list, so paginated slices still tile.
pub(crate) fn mmr_rerank(
    fused: Vec<(f32, ChunkKey)>,
    resolve: &impl Fn(&ChunkKey) -> Option<ChunkRec>,
) -> Vec<(f32, ChunkKey)> {
    let pool_n = fused.len().min(MMR_POOL);
    if pool_n <= 1 {
        return fused;
    }
    // normalize once so [`bump_sim`] compares unit vectors with a bare dot.
    // A degenerate embedding becomes `None`: [`cosine`] defined it as 0.0
    // similarity against everything, which is exactly how `None` already
    // behaves here (never bumps, stays maximally novel).
    let embs: Vec<Option<Emb>> = fused[..pool_n]
        .iter()
        .map(|(_, k)| {
            let mut e = resolve(k)?.emb;
            (normalize(&mut e) > 0.0).then_some(e)
        })
        .collect();
    let top = fused[0].0;
    let norm = |s: f32| if top > 0.0 { s / top } else { 1.0 };

    let mut max_sim = vec![0.0f32; pool_n];
    let mut picked = vec![false; pool_n];
    let mut order = Vec::with_capacity(pool_n);
    // seed with the most relevant (fused is already best-first)
    picked[0] = true;
    order.push(0usize);
    bump_sim(&mut max_sim, &picked, &embs, 0);
    for _ in 1..pool_n {
        let mut best = usize::MAX;
        let mut best_score = f32::NEG_INFINITY;
        for i in 0..pool_n {
            if picked[i] {
                continue;
            }
            let score = MMR_LAMBDA * norm(fused[i].0) - (1.0 - MMR_LAMBDA) * max_sim[i];
            if score > best_score {
                best_score = score;
                best = i;
            }
        }
        picked[best] = true;
        order.push(best);
        bump_sim(&mut max_sim, &picked, &embs, best);
    }

    let mut out: Vec<(f32, ChunkKey)> = order.into_iter().map(|i| fused[i].clone()).collect();
    out.extend_from_slice(&fused[pool_n..]);
    out
}

/// Score-aware fusion: `FUSE_LEX_WEIGHT·lex + (1−FUSE_LEX_WEIGHT)·sem`,
/// where both inputs are already normalized to their per-query top (lex is
/// `Hit::rel`, sem is similarity / top similarity). Replaced rank-only RRF,
/// whose fixed 1/(60+rank) votes let incidental word overlap outvote a
/// confident semantic #1 (and vice versa): a doc mediocre in both lists
/// could beat one excellent in one. Blending confidence instead of rank
/// improved every workload in the retrieval-eval fusion sweep.
///
/// Note cards leave with their fused score scaled by [`NOTE_BOOST`], which
/// is what makes them place above equally-supported page chunks — and, since
/// the boost lands before the re-rank pools are cut, is also what gets a
/// borderline card *into* those pools at all.
pub(crate) fn fuse(lex: &[(ChunkKey, f32)], sem: &[(ChunkKey, f32)]) -> Vec<(f32, ChunkKey)> {
    let mut scores: FxHashMap<&ChunkKey, f32> = FxHashMap::default();
    for (key, rel) in lex {
        *scores.entry(key).or_insert(0.0) += FUSE_LEX_WEIGHT * rel;
    }
    for (key, sim) in sem {
        *scores.entry(key).or_insert(0.0) += (1.0 - FUSE_LEX_WEIGHT) * sim;
    }
    let mut out: Vec<(f32, ChunkKey)> = scores
        .into_iter()
        .map(|(k, s)| {
            let s = if is_reserved(&k.doc) {
                s * NOTE_BOOST
            } else {
                s
            };
            (s, k.clone())
        })
        .collect();
    out.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out
}

/// Lexical + optional semantic, score-fused, metadata resolved via `resolve`
/// (see below) — all under the one snapshot `r` was taken from. `filter`,
/// when set, restricts every ranker to the given doc ids *inside* the
/// search (filtering after truncation would starve results).
///
/// `complete` expands the trailing token via the term dictionary — right
/// for type-ahead (a human mid-word), wrong for programmatic callers whose
/// queries are complete words ("micro" must not match "microscope").
///
/// `resolve` fetches a chunk's words given its key; callers should back it
/// with [`Library::get`] (a cheap primary-table point-read) rather than the
/// `meta` sink's reverse index — `meta` stores each chunk's full `Vec<Word>`
/// as part of its fjall *key* (needed to answer "what key maps to this
/// value"), so looking words up through it means every hit pays for
/// comparing against huge keys. `Library::get` reads the same words back out
/// of a value instead, which is what point-reads are fast at.
// The one search entry point takes orthogonal, individually-documented knobs;
// bundling them into a params struct would churn every caller for no clarity
// gain (audited under the behavior-preserving lint uplift).
#[expect(clippy::too_many_arguments)]
pub fn search<R: Readable>(
    r: &Readers<'_, R>,
    query: &str,
    qemb: Option<&Emb>,
    k: usize,
    filter: Option<&FxHashSet<String>>,
    complete: bool,
    fuzzy: bool,
    diversify: bool,
    resolve: impl Fn(&ChunkKey) -> Option<ChunkRec>,
    stats: Option<&mut RankerStats>,
) -> Vec<Hit> {
    let ((lex, vec), (_, terms)) = r;

    let mut st = RankerStats::default();
    let t = std::time::Instant::now();
    let orig = tokenize(query);
    let mut toks = orig.clone();
    if complete && let Some(last) = toks.last().cloned() {
        for t in terms.complete(&last, 5) {
            if !toks.contains(&t) {
                toks.push(t);
            }
        }
    }
    // fuzzy correction (full queries only): replace each unknown query word
    // with its nearest real vocabulary words, which then feed the exact
    // lexical index. Exact words expand nothing, so clean queries are
    // unchanged. Bounded to FUZZ_CANDIDATES per token.
    if fuzzy {
        for tok in &orig {
            if terms.contains(tok) {
                continue;
            }
            for t in terms.correct(tok, FUZZ_CANDIDATES) {
                if !toks.contains(&t) {
                    toks.push(t);
                }
            }
        }
    }
    st.term_expand_us = t.elapsed().as_micros() as u64;
    if toks.is_empty() {
        if let Some(s) = stats {
            *s = st;
        }
        return Vec::new();
    }

    // the expanded tokens are already normalized, so re-tokenizing the
    // joined query inside Bm25 is a no-op
    let expanded = toks.join(" ");

    // give fusion headroom beyond the final k (and keep the list pinned — see LEX_FETCH)
    let fetch = k.max(LEX_FETCH);
    let t = std::time::Instant::now();
    let scored = match filter {
        Some(f) => lex.search_filtered(&expanded, fetch, |key: &ChunkKey| f.contains(&key.doc)),
        None => lex.search(&expanded, fetch),
    };
    let top = scored.first().map(|h| h.score).unwrap_or(0.0);
    let rel: FxHashMap<ChunkKey, (f32, f32)> = scored
        .iter()
        .map(|h| {
            let r = if top > 0.0 {
                (h.score / top) as f32
            } else {
                1.0
            };
            (h.val.clone(), (r, h.score as f32))
        })
        .collect();
    let lexical: Vec<ChunkKey> = scored.into_iter().map(|h| h.val).collect();
    let lex_rank: FxHashMap<ChunkKey, u32> = lexical
        .iter()
        .enumerate()
        .map(|(i, k)| (k.clone(), i as u32))
        .collect();
    st.lex_search_us = t.elapsed().as_micros() as u64;
    let t = std::time::Instant::now();
    let sem_scored: Vec<Scored<f32, ChunkKey>> = match (qemb, filter) {
        (Some(e), Some(f)) => vec.search_filtered(e, |key: &ChunkKey| f.contains(&key.doc)),
        (Some(e), None) => vec.search(e),
        (None, _) => Vec::new(),
    };
    let sem_rank: FxHashMap<ChunkKey, (u32, f32)> = sem_scored
        .iter()
        .enumerate()
        .map(|(i, h)| (h.val.clone(), (i as u32, h.score)))
        .collect();
    st.vec_search_us = t.elapsed().as_micros() as u64;
    st.lex_n = lexical.len();
    st.sem_n = sem_scored.len();

    let t = std::time::Instant::now();
    // fusion inputs: each list normalized to its own top. Lexical reuses
    // the rel map; semantic converts cosine distance to similarity (HNSW
    // returns distance, best first) and normalizes by the top hit's.
    let lex_list: Vec<(ChunkKey, f32)> = lexical
        .iter()
        .map(|k| (k.clone(), rel.get(k).map_or(1.0, |&(r, _)| r)))
        .collect();
    let top_sim = sem_scored.first().map_or(0.0, |h| 1.0 - h.score);
    let sem_list: Vec<(ChunkKey, f32)> = sem_scored
        .into_iter()
        .map(|h| {
            let sim = (1.0 - h.score).max(0.0);
            let norm = if top_sim > 0.0 { sim / top_sim } else { 0.0 };
            (h.val, norm)
        })
        .collect();
    let fused = fuse(&lex_list, &sem_list);
    // late-interaction re-rank of the top pool: word-level matching the
    // pooled embeddings can't see. Cheap enough for every keystroke.
    let fused = maxsim_rerank(fused, query, &resolve);
    // diversity: demote near-duplicates (same book/edition) among the top
    // hits. Full queries only — the per-keystroke path can't afford the
    // embedding reads, and doc-scoped browser-find must keep full coverage.
    let ordered = if diversify {
        mmr_rerank(fused, &resolve)
    } else {
        fused
    };
    let hits: Vec<Hit> = ordered
        .into_iter()
        .take(k)
        .filter_map(|(score, key)| {
            let rec = resolve(&key)?;
            let (r, bm25) = rel.get(&key).copied().unwrap_or((1.0, 0.0));
            let (sem_rank, sem_dist) = match sem_rank.get(&key) {
                Some(&(rank, dist)) => (Some(rank), Some(dist)),
                None => (None, None),
            };
            Some(Hit {
                score,
                rel: r,
                bm25,
                lex_rank: lex_rank.get(&key).copied(),
                sem_rank,
                sem_dist,
                key,
                words: rec.words,
            })
        })
        .collect();
    st.fuse_us = t.elapsed().as_micros() as u64;
    if let Some(s) = stats {
        *s = st;
    }
    hits
}

#[cfg(test)]
mod fuzzy_mmr_tests {
    use super::*;
    use crate::EMB_DIM;

    fn key(doc: &str) -> ChunkKey {
        ChunkKey {
            doc: doc.to_string(),
            page: 1,
            idx: 0,
        }
    }

    fn rec(doc: &str, hot: usize) -> ChunkRec {
        let mut emb = [0.0f32; EMB_DIM];
        emb[hot] = 1.0;
        ChunkRec {
            key: key(doc),
            words: vec![],
            emb,
        }
    }

    fn one_hot(hot: usize) -> Emb {
        let mut e = [0.0f32; EMB_DIM];
        e[hot] = 1.0;
        e
    }

    #[test]
    fn fuse_blends_normalized_scores() {
        let (a, b, c) = (key("a"), key("b"), key("c"));
        // b is strong in both lists and must beat the two single-list tops
        let fused = fuse(
            &[(a.clone(), 1.0), (b.clone(), 0.9)],
            &[(b.clone(), 1.0), (c.clone(), 0.8)],
        );
        let order: Vec<&str> = fused.iter().map(|(_, k)| k.doc.as_str()).collect();
        assert_eq!(order, vec!["b", "a", "c"]);
        assert!((fused[0].0 - (0.5 * 0.9 + 0.5 * 1.0)).abs() < 1e-6);
        assert!((fused[1].0 - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fuse_single_list_and_empty() {
        assert!(fuse(&[], &[]).is_empty());
        let order: Vec<String> = fuse(&[(key("a"), 1.0), (key("b"), 0.7), (key("c"), 0.2)], &[])
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(order, vec!["a", "b", "c"]); // single list: order preserved
        // equal scores tie-break on key order for determinism
        let tied = fuse(&[(key("b"), 1.0)], &[(key("a"), 1.0)]);
        assert_eq!(tied[0].1.doc, "a");
    }

    #[test]
    fn fuse_confidence_outvotes_incidental_overlap() {
        // the regression the fusion sweep fixed: under rank-only RRF a doc
        // ranked #2 in both lists (2·1/62) beat a doc that was semantic #1
        // but lexically deep (1/60 + 1/76) — dual mediocre membership
        // outvoted single-list excellence even when the mediocre doc's
        // *scores* were weak. Score-aware fusion keeps the vote
        // proportional to evidence strength.
        let (gold, mediocre) = (key("gold"), key("mediocre"));
        let fused = fuse(
            // gold: near-zero lexical evidence, deep in the list
            &[(mediocre.clone(), 0.3), (gold.clone(), 0.08)],
            // gold: semantic top at full confidence
            &[(gold.clone(), 1.0), (mediocre.clone(), 0.3)],
        );
        assert_eq!(fused[0].1.doc, "gold");
        // 0.5·0.08 + 0.5·1.0 = 0.54 vs 0.5·0.3 + 0.5·0.3 = 0.3
        assert!((fused[0].0 - 0.54).abs() < 1e-6);
    }

    #[test]
    fn fuse_boosts_note_cards_over_equally_supported_pages() {
        let (card, page) = (key("~card/c123"), key("moxon"));
        // the page has the *stronger* lexical evidence; the boost still has
        // to be worth more than that gap for a card to be worth boosting
        let fused = fuse(&[(page.clone(), 0.6), (card.clone(), 0.5)], &[]);
        assert_eq!(fused[0].1.doc, "~card/c123");
        assert!((fused[0].0 - 0.5 * 0.5 * NOTE_BOOST).abs() < 1e-6);
        assert!((fused[1].0 - 0.5 * 0.6).abs() < 1e-6);
        // far enough ahead and the page keeps its place — the boost is a
        // thumb on the scale, not an override
        let fused = fuse(&[(page, 1.0), (card, 0.5)], &[]);
        assert_eq!(fused[0].1.doc, "moxon");
    }

    fn word(t: &str) -> Word {
        Word {
            t: t.to_string(),
            x: 0.0,
            y: 0.0,
            w: 0.1,
            h: 0.1,
        }
    }

    fn rec_with_words(doc: &str, words: &[&str]) -> ChunkRec {
        ChunkRec {
            key: key(doc),
            words: words.iter().map(|w| word(w)).collect(),
            emb: [0.0; EMB_DIM],
        }
    }

    #[test]
    fn maxsim_promotes_the_chunk_containing_the_query_words() {
        // "match" contains the query's words verbatim; "other" is unrelated
        // prose. Fused order has "other" ahead — the reranker's word-level
        // matching must flip them (0.7·maxsim outweighs 0.3·fused).
        let fused = vec![(0.9f32, key("other")), (0.5, key("match"))];
        let resolve = |k: &ChunkKey| match k.doc.as_str() {
            "match" => Some(rec_with_words("match", &["mignonette", "sauce", "recipe"])),
            "other" => Some(rec_with_words(
                "other",
                &["quantum", "chromodynamics", "lattice"],
            )),
            _ => None,
        };
        let out: Vec<String> = maxsim_rerank(fused, "mignonette sauce", &resolve)
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(out, vec!["match", "other"]);
    }

    #[test]
    fn maxsim_degenerate_inputs_keep_fused_order() {
        // no query tokens → untouched
        let fused = vec![(0.9f32, key("a")), (0.8, key("b"))];
        let resolve = |_: &ChunkKey| -> Option<ChunkRec> { None };
        let out = maxsim_rerank(fused.clone(), "", &resolve);
        assert_eq!(out, fused);
        // unresolvable chunks / empty word lists → identical maxsim for all,
        // so fused order must survive the re-sort (index tiebreak)
        let out = maxsim_rerank(fused.clone(), "mignonette sauce", &resolve);
        assert_eq!(
            out.iter().map(|(_, k)| &k.doc).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        // single hit: nothing to rerank
        let one = vec![(0.9f32, key("a"))];
        assert_eq!(maxsim_rerank(one.clone(), "query", &resolve), one);
    }

    #[test]
    fn maxsim_scores_stay_monotonic_and_in_pool_range() {
        // reranked scores are rescaled into the pool's original score range,
        // so the head block never scores below the un-reranked tail
        let fused: Vec<(f32, ChunkKey)> = (0..25)
            .map(|i| (1.0 - i as f32 * 0.02, key(&format!("d{i:02}"))))
            .collect();
        let resolve = |k: &ChunkKey| Some(rec_with_words(&k.doc, &["filler", "words"]));
        let out = maxsim_rerank(fused, "some query text", &resolve);
        assert_eq!(out.len(), 25);
        for w in out.windows(2) {
            assert!(
                w[0].0 >= w[1].0 - 1e-6,
                "scores must stay monotonic: {} then {}",
                w[0].0,
                w[1].0
            );
        }
    }

    /// The MMR path dropped its explicit `cosine(a, b)` in favour of
    /// normalizing the pool once and taking a bare [`dot`]. This pins the
    /// identity that swap rests on — degenerate inputs included, since
    /// `cosine` defined those as 0.0 and the new path defines them as
    /// `None`.
    #[test]
    fn normalized_dot_is_cosine_similarity() {
        // the definition mmr_rerank used before the pool was pre-normalized
        let cosine = |a: &Emb, b: &Emb| {
            let (na, nb) = (dot(a, a), dot(b, b));
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                dot(a, b) / (na.sqrt() * nb.sqrt())
            }
        };
        // what mmr_rerank + bump_sim do now
        let sim = |a: &Emb, b: &Emb| {
            let unit = |v: &Emb| {
                let mut u = *v;
                (normalize(&mut u) > 0.0).then_some(u)
            };
            match (unit(a), unit(b)) {
                (Some(x), Some(y)) => dot(&x, &y),
                _ => 0.0,
            }
        };

        let (e0, e1) = (one_hot(0), one_hot(1));
        let scaled = {
            let mut s = one_hot(0);
            s.iter_mut().for_each(|x| *x *= 7.5);
            s
        };
        let mixed = {
            let mut m = one_hot(3);
            m[7] = 2.0;
            m
        };
        let z = [0.0f32; EMB_DIM];

        for (a, b) in [
            (&e0, &e0),
            (&e0, &e1),
            (&scaled, &e0),
            (&mixed, &e0),
            (&mixed, &e1),
            (&z, &e0),
            (&z, &z),
        ] {
            assert!(
                (sim(a, b) - cosine(a, b)).abs() < 1e-6,
                "normalized dot {} != cosine {}",
                sim(a, b),
                cosine(a, b)
            );
        }
        // and the values themselves, not merely that the two agree
        assert!((sim(&e0, &e0) - 1.0).abs() < 1e-6);
        assert!(sim(&e0, &e1).abs() < 1e-6);
        assert!(
            (sim(&scaled, &e0) - 1.0).abs() < 1e-6,
            "similarity is scale-invariant"
        );
        // degenerate (zero) vectors are defined as 0, not NaN
        assert_eq!(sim(&z, &e0), 0.0);
        assert_eq!(sim(&z, &z), 0.0);
    }

    #[test]
    fn bump_sim_skips_picked_and_missing_embs() {
        let embs = vec![Some(one_hot(0)), Some(one_hot(0)), None];
        let picked = vec![true, false, false];
        let mut max_sim = vec![0.0f32; 3];
        bump_sim(&mut max_sim, &picked, &embs, 0);
        assert_eq!(max_sim[0], 0.0); // picked: never updated
        assert!((max_sim[1] - 1.0).abs() < 1e-6); // duplicate of selection
        assert_eq!(max_sim[2], 0.0); // no embedding: stays novel
        // selecting an item with no embedding is a no-op
        bump_sim(&mut max_sim, &picked, &embs, 2);
        assert!((max_sim[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mmr_rerank_empty_and_single_pools_pass_through() {
        let resolve = |_: &ChunkKey| -> Option<ChunkRec> { None };
        assert!(mmr_rerank(Vec::new(), &resolve).is_empty());
        let one = vec![(0.9f32, key("a"))];
        assert_eq!(mmr_rerank(one.clone(), &resolve), one);
    }

    #[test]
    fn mmr_rerank_all_identical_keeps_relevance_order() {
        // every candidate is the same direction: nothing to diversify
        // toward, so relevance order must survive intact
        let fused = vec![(0.9f32, key("a")), (0.8, key("b")), (0.7, key("c"))];
        let resolve = |k: &ChunkKey| Some(rec(&k.doc, 0));
        let out: Vec<String> = mmr_rerank(fused, &resolve)
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn mmr_rerank_unresolvable_embeddings_keep_fused_order() {
        // resolve failing (e.g. raced deletion) must degrade to no-op, not panic
        let fused = vec![(0.9f32, key("a")), (0.8, key("b")), (0.7, key("c"))];
        let resolve = |_: &ChunkKey| -> Option<ChunkRec> { None };
        let out: Vec<String> = mmr_rerank(fused, &resolve)
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn mmr_demotes_a_near_duplicate() {
        // a and b share an embedding direction (near-duplicate); c is novel.
        // fused order by relevance is a, b, c — MMR should promote c over b.
        let fused = vec![(0.9f32, key("a")), (0.85, key("b")), (0.5, key("c"))];
        let resolve = |k: &ChunkKey| match k.doc.as_str() {
            "a" => Some(rec("a", 0)),
            "b" => Some(rec("b", 0)),
            "c" => Some(rec("c", 1)),
            _ => None,
        };
        let out: Vec<String> = mmr_rerank(fused, &resolve)
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(out, vec!["a", "c", "b"]);
    }

    #[test]
    fn mmr_preserves_order_without_duplicates() {
        // all-distinct directions: MMR must not reshuffle a diverse list
        let fused = vec![(0.9f32, key("a")), (0.8, key("b")), (0.7, key("c"))];
        let resolve = |k: &ChunkKey| match k.doc.as_str() {
            "a" => Some(rec("a", 0)),
            "b" => Some(rec("b", 1)),
            "c" => Some(rec("c", 2)),
            _ => None,
        };
        let out: Vec<String> = mmr_rerank(fused, &resolve)
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(out, vec!["a", "b", "c"]);
    }
}
