//! Hybrid search: lexical + semantic fused with RRF, plus MMR diversity.

use fold::pipeline::Scored;
use fold::stream::Readable;
use fxhash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::text::tokenize;
use crate::{ChunkKey, ChunkRec, EMB_DIM, Emb, FxHashSet, Readers, Word};

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
    /// exactly the hits that take the `rel = 1.0` default above and so
    /// bypass the [`MIN_REL`] cutoff.
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
    /// µs: RRF fusion + MMR re-rank + hit resolution (primary-table
    /// point-reads).
    pub fuse_us: u64,
}

/// Hits scoring below this fraction of the query's top BM25 hit are noise;
/// the paginated result stream ends here. Tuning knob — the perf view's
/// provenance table is the place to eyeball rel distributions.
pub const MIN_REL: f32 = 0.25;

/// How deep to fetch from the lexical ranker regardless of `k`. Pinning the
/// depth pins the lexical list — and therefore the RRF input and the final
/// order — so paginated slices of the same query tile without drift (a
/// growing fetch would add RRF terms to dual-membership keys and shift
/// ranks between page requests). Also caps stable pagination depth at
/// ~LEX_FETCH lexical + TOP_K semantic hits. BM25 cost is limit-independent
/// (full postings scan, truncate at end), so the extra depth is nearly free.
pub const LEX_FETCH: usize = 512;

/// Nearest real terms substituted per unknown query word.
pub(crate) const FUZZ_CANDIDATES: usize = 3;

/// Cosine similarity of two embeddings (0 if either is degenerate).
pub(crate) fn cosine(a: &Emb, b: &Emb) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..EMB_DIM {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
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
pub(crate) fn bump_sim(max_sim: &mut [f32], picked: &[bool], embs: &[Option<Emb>], sel: usize) {
    let Some(se) = &embs[sel] else { return };
    for (i, mi) in max_sim.iter_mut().enumerate() {
        if !picked[i]
            && let Some(e) = &embs[i]
        {
            *mi = mi.max(cosine(e, se));
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
    let embs: Vec<Option<Emb>> = fused[..pool_n]
        .iter()
        .map(|(_, k)| resolve(k).map(|r| r.emb))
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

/// Reciprocal rank fusion: score(k) = sum over lists of 1/(60 + rank).
pub(crate) fn rrf(lists: &[Vec<ChunkKey>]) -> Vec<(f32, ChunkKey)> {
    let mut scores: FxHashMap<&ChunkKey, f32> = FxHashMap::default();
    for list in lists {
        for (rank, key) in list.iter().enumerate() {
            *scores.entry(key).or_insert(0.0) += 1.0 / (60.0 + rank as f32);
        }
    }
    let mut out: Vec<(f32, ChunkKey)> = scores.into_iter().map(|(k, s)| (s, k.clone())).collect();
    out.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out
}

/// Lexical + optional semantic, RRF-fused, metadata resolved via `resolve`
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

    // give RRF headroom beyond the final k (and keep the list pinned — see LEX_FETCH)
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
    let semantic: Vec<ChunkKey> = sem_scored.into_iter().map(|h| h.val).collect();
    st.vec_search_us = t.elapsed().as_micros() as u64;
    st.lex_n = lexical.len();
    st.sem_n = semantic.len();

    let t = std::time::Instant::now();
    let fused = rrf(&[lexical, semantic]);
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
    fn rrf_fuses_by_reciprocal_rank() {
        let (a, b, c) = (key("a"), key("b"), key("c"));
        // b is mid-rank in both lists and must beat the two single-list tops
        let fused = rrf(&[vec![a.clone(), b.clone()], vec![b.clone(), c.clone()]]);
        let order: Vec<&str> = fused.iter().map(|(_, k)| k.doc.as_str()).collect();
        assert_eq!(order, vec!["b", "a", "c"]);
        assert!((fused[0].0 - (1.0 / 61.0 + 1.0 / 60.0)).abs() < 1e-6);
        assert!((fused[1].0 - 1.0 / 60.0).abs() < 1e-6);
    }

    #[test]
    fn rrf_single_list_and_empty() {
        assert!(rrf(&[]).is_empty());
        assert!(rrf(&[vec![], vec![]]).is_empty());
        let order: Vec<String> = rrf(&[vec![key("a"), key("b"), key("c")]])
            .into_iter()
            .map(|(_, k)| k.doc)
            .collect();
        assert_eq!(order, vec!["a", "b", "c"]); // single list: order preserved
        // equal scores tie-break on key order for determinism
        let tied = rrf(&[vec![key("b")], vec![key("a")]]);
        assert_eq!(tied[0].1.doc, "a");
    }

    #[test]
    fn cosine_identical_is_one_orthogonal_is_zero() {
        let (e0, e1) = (one_hot(0), one_hot(1));
        assert!((cosine(&e0, &e0) - 1.0).abs() < 1e-6);
        assert!(cosine(&e0, &e1).abs() < 1e-6);
        // degenerate (zero) vectors are defined as 0, not NaN
        let z = [0.0f32; EMB_DIM];
        assert_eq!(cosine(&z, &e0), 0.0);
        assert_eq!(cosine(&z, &z), 0.0);
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
