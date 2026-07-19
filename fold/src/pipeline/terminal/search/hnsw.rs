use std::sync::{Arc, RwLock};

use anny::metric::{Metric, Scalar};
use fjall::Readable;
use fxhash::{FxHashMap, FxHashSet};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    pipeline::{Keyed, Push, Scored},
    stream::{PipelineInitCtx, WriteTx},
};

fn decode_vector<T: DeserializeOwned + Copy, const DIM: usize>(bytes: &[u8]) -> [T; DIM] {
    let v: Vec<T> =
        postcard::from_bytes(bytes).expect("corrupt hnsw vector: postcard decode failed");
    std::array::from_fn(|i| v[i])
}

// graph-keyspace row keys
const GEN_KEY: &[u8] = b"g";
const HDR_KEY: &[u8] = b"h"; // postcard((gen: u64, n_chunks: u32))
const KEYS_KEY: &[u8] = b"k"; // postcard(Vec<(u32, K)>)
const CHUNK_PREFIX: u8 = b'c'; // 'c' + u32 BE -> blob chunk
const CHUNK_BYTES: usize = 1 << 20;

fn chunk_key(i: u32) -> [u8; 5] {
    let b = i.to_be_bytes();
    [CHUNK_PREFIX, b[0], b[1], b[2], b[3]]
}

// The in-memory side of the sink, shared with readers. `ids`/`keys` tie the
// persisted rows to anny's ephemeral node ids; `stale` marks the graph as
// diverged from the store (an aborted transaction cannot un-mutate it), to
// be rebuilt from the persisted vectors on next use. Arc<RwLock> rather
// than Rc<RefCell>: hosts share streams across threads (e.g. a server
// answering searches from a blocking pool), and reads only contend when a
// stale graph must be rebuilt.
struct State<
    K,
    T,
    M: Metric<T>,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> {
    index: anny::hnsw::Hnsw<T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>,
    ids: FxHashMap<Vec<u8>, u32>, // postcard(K) -> node id
    keys: FxHashMap<u32, K>,      // node id -> key
    stale: bool,
}

impl<
    K,
    T,
    M,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> State<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>
where
    K: DeserializeOwned,
    T: Scalar + DeserializeOwned,
    M: Metric<T> + Copy,
{
    fn upsert(&mut self, kenc: Vec<u8>, key: K, vec: [T; DIM]) {
        if let Some(old) = self.ids.remove(&kenc) {
            self.index.remove(old);
            self.keys.remove(&old);
        }
        let id = self.index.insert(vec);
        self.ids.insert(kenc, id);
        self.keys.insert(id, key);
    }

    fn remove(&mut self, kenc: &[u8]) -> bool {
        match self.ids.remove(kenc) {
            Some(old) => {
                self.index.remove(old);
                self.keys.remove(&old);
                true
            }
            None => false,
        }
    }

    // reconstruct the graph from the persisted `postcard(K) -> vector` rows
    fn rebuild(&mut self, metric: M, seed: u64, entries: impl Iterator<Item = (Vec<u8>, Vec<u8>)>) {
        self.index = anny::hnsw::Hnsw::new(metric, seed);
        self.ids.clear();
        self.keys.clear();
        for (kenc, venc) in entries {
            let key: K =
                postcard::from_bytes(&kenc).expect("corrupt hnsw key: postcard decode failed");
            self.upsert(kenc, key, decode_vector::<T, DIM>(&venc));
        }
        self.stale = false;
    }
}

/// Persistent approximate-nearest-neighbor index over [`Keyed`]`<K, [T;
/// DIM]>` embeddings, backed by [anny](anny)'s retractable HNSW graph.
///
/// Accepts `Keyed { key: document, val: embedding }` and maintains two
/// coupled structures: the vectors persist in this sink's keyspace (so the
/// index recovers on reopen), and an in-memory HNSW graph mirrors them for
/// sub-linear search. [`HnswReader::search`] returns the approximately
/// nearest keys ascending by distance under the metric `M` (see
/// [`anny::metric`] — smaller is closer).
///
/// Like the posting sinks, documents are set-semantic per key: within a
/// transaction deltas accumulate and the net sign decides — positive
/// (re)indexes the key under its latest embedding, non-positive deletes it —
/// with no read of prior state. Retraction genuinely removes the node from
/// the graph (anny repairs the neighborhood), so recall does not decay
/// under churn the way tombstoning indexes do.
///
/// The graph lives in memory: it is rebuilt from the persisted vectors when
/// the stream opens, and again if a transaction aborts after a mid-tx flush
/// (a panic cannot un-mutate the graph, so it is marked stale and rebuilt
/// from committed state on next use). Under fold's single-writer discipline
/// readers otherwise always observe a graph consistent with their snapshot.
///
/// [`Stream::checkpoint`](crate::stream::Stream::checkpoint) persists the
/// graph itself as a blob in a sibling `{name}_graph` keyspace; when the
/// stored blob matches the vectors' generation, the next open loads it
/// directly instead of re-inserting every vector.
///
/// Tuning lives in the const parameters (`M0`, `TOP_K`, `EF_SEARCH`,
/// `EF_BUILD`, `MAX_LEVEL`), with usable defaults; `TOP_K` fixes the number
/// of results per search at compile time.
///
/// ```no_run
/// use anny::metric::L2;
/// use fold::pipeline::{Keyed, terminal::search::Hnsw};
/// use fold::stream::Stream;
///
/// let mut st = Stream::new("vecs.db", Hnsw::<u32, f32, L2, 4>::new("vecs", L2, 42));
/// st.wtx(|tx| tx.insert(&Keyed::new(7, [0.1, 0.2, 0.3, 0.4])));
/// st.rtx(|idx| {
///     for hit in idx.search(&[0.1, 0.2, 0.3, 0.4]) {
///         println!("{}: {}", hit.val, hit.score);
///     }
/// });
/// ```
pub struct Hnsw<
    K,
    T,
    M: Metric<T>,
    const DIM: usize,
    const M0: usize = 32,
    const TOP_K: usize = 10,
    const EF_SEARCH: usize = 40,
    const EF_BUILD: usize = 80,
    const MAX_LEVEL: usize = 16,
> {
    name: String,
    ks: Option<fjall::SingleWriterTxKeyspace>,
    graph_ks: Option<fjall::SingleWriterTxKeyspace>,
    metric: M,
    seed: u64,
    // generation counts mutating commits; saved_generation is the generation
    // of the last-written graph blob (u64::MAX: no blob matches the graph)
    generation: u64,
    saved_generation: u64,
    state: Arc<RwLock<State<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>>>,
    // encoded key -> (key, latest embedding, net delta this tx)
    pending: FxHashMap<Vec<u8>, (K, [T; DIM], i64)>,
    vec_buf: Vec<u8>,
}

impl<
    K,
    T,
    M,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> Hnsw<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>
where
    T: Scalar,
    M: Metric<T>,
{
    /// `name` identifies this sink's keyspace and must be unique among all
    /// named nodes in the pipeline. `seed` fixes the graph's level
    /// randomness, making builds deterministic.
    pub fn new(name: impl Into<String>, metric: M, seed: u64) -> Self
    where
        M: Copy,
    {
        Hnsw {
            name: name.into(),
            ks: None,
            graph_ks: None,
            metric,
            seed,
            generation: 0,
            saved_generation: 0,
            state: Arc::new(RwLock::new(State {
                index: anny::hnsw::Hnsw::new(metric, seed),
                ids: FxHashMap::default(),
                keys: FxHashMap::default(),
                stale: false,
            })),
            pending: FxHashMap::default(),
            vec_buf: Default::default(),
        }
    }
}

impl<
    K,
    T,
    M,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> Hnsw<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>
where
    K: Serialize + DeserializeOwned,
    T: Scalar + DeserializeOwned,
    M: Metric<T> + Copy,
{
    /// Fast path: load the checkpointed graph blob if it matches the current
    /// generation. Returns false (and leaves state untouched) on staleness or
    /// any decode failure — the caller replays from the vector rows instead.
    fn try_load_blob(
        &mut self,
        rtx: &fjall::Snapshot,
        gks: &fjall::SingleWriterTxKeyspace,
    ) -> bool {
        let Ok(Some(hdr)) = rtx.get(gks, HDR_KEY) else {
            return false;
        };
        let Ok((blob_gen, n_chunks)) = postcard::from_bytes::<(u64, u32)>(&hdr) else {
            return false;
        };
        if blob_gen != self.generation {
            return false; // graph changed since the blob was written
        }
        let mut blob = Vec::new();
        for i in 0..n_chunks {
            match rtx.get(gks, chunk_key(i)) {
                Ok(Some(c)) => blob.extend_from_slice(&c),
                _ => return false,
            }
        }
        let Ok(index) = anny::hnsw::Hnsw::from_bytes(self.metric, &blob) else {
            return false;
        };
        let Ok(Some(kb)) = rtx.get(gks, KEYS_KEY) else {
            return false;
        };
        let Ok(pairs) = postcard::from_bytes::<Vec<(u32, K)>>(&kb) else {
            return false;
        };
        if index.len() != pairs.len() {
            return false;
        }
        let mut state = self
            .state
            .write()
            .expect("hnsw index lock poisoned by an earlier panic; reopen the store");
        state.ids = pairs
            .iter()
            .map(|(id, k)| {
                (
                    postcard::to_stdvec(k).expect("postcard encode of hnsw key failed"),
                    *id,
                )
            })
            .collect();
        state.keys = pairs.into_iter().collect();
        state.index = index;
        state.stale = false;
        true
    }
}

impl<
    K,
    T,
    M,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> Push<Keyed<K, [T; DIM]>> for Hnsw<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>
where
    K: Clone + Serialize + DeserializeOwned,
    T: Scalar + Serialize + DeserializeOwned,
    M: Metric<T> + Copy,
{
    type Reader<'tx, R: Readable + 'tx> =
        HnswReader<'tx, R, K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>;

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        let ks = init.keyspace(&self.name);
        let graph_ks = init.keyspace(&format!("{}_graph", self.name));
        let rtx = init.snapshot();

        self.generation = rtx
            .get(&graph_ks, GEN_KEY)
            .expect("hnsw generation read failed")
            .map(|v| {
                u64::from_le_bytes(
                    v.as_ref()
                        .try_into()
                        .expect("corrupt hnsw generation: not 8 bytes"),
                )
            })
            .unwrap_or(0);

        if self.try_load_blob(&rtx, &graph_ks) {
            self.saved_generation = self.generation;
        } else {
            // no blob / stale blob: recover the graph from the vectors
            // persisted by earlier runs
            self.state
                .write()
                .expect("hnsw index lock poisoned by an earlier panic; reopen the store")
                .rebuild(
                    self.metric,
                    self.seed,
                    rtx.iter(&ks).map(|kv| {
                        let (k, v) = kv.into_inner().expect("hnsw vector scan failed");
                        (k.to_vec(), v.to_vec())
                    }),
                );
            // sentinel: no blob matches the live graph, so the next
            // checkpoint always writes one
            self.saved_generation = u64::MAX;
        }
        self.ks = Some(ks);
        self.graph_ks = Some(graph_ks);
    }

    fn push(&mut self, tx: &mut WriteTx<'_>, data: &Keyed<K, [T; DIM]>, delta: isize) {
        tx.buf.clear();
        postcard::to_io(&data.key, &mut tx.buf).expect("postcard encode of hnsw key failed");
        let e = self
            .pending
            .entry(tx.buf.clone())
            .or_insert_with(|| (data.key.clone(), data.val, 0));
        e.1 = data.val;
        e.2 += delta as i64;
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        if self.pending.is_empty() {
            return;
        }
        let ks = self.ks.clone().expect("sink used before init()");
        let mut state = self
            .state
            .write()
            .expect("hnsw index lock poisoned by an earlier panic; reopen the store");
        if state.stale {
            // the previous transaction aborted: this one sees clean
            // committed state, so resync the graph before applying
            let entries = tx.iter(&ks).map(|kv| {
                let (k, v) = kv.into_inner().expect("hnsw vector scan failed");
                (k.to_vec(), v.to_vec())
            });
            let (metric, seed) = (self.metric, self.seed);
            state.rebuild(metric, seed, entries);
        }
        let mut mutated = false;
        for (kenc, (key, vec, delta)) in self.pending.drain() {
            match delta {
                1.. => {
                    self.vec_buf.clear();
                    postcard::to_io(&vec[..], &mut self.vec_buf)
                        .expect("postcard encode of hnsw vector failed");

                    tx.insert(&ks, &kenc, &self.vec_buf);
                    state.upsert(kenc, key, vec);
                    mutated = true;
                }
                0 => {}
                _ => {
                    if state.remove(&kenc) {
                        tx.remove(&ks, &kenc);
                        mutated = true;
                    }
                }
            }
        }
        if mutated {
            // the durable counter follows the graph; commit can run several
            // times per transaction, monotonicity is all that matters
            self.generation += 1;
            let gks = self.graph_ks.clone().expect("sink used before init()");
            tx.insert(&gks, GEN_KEY, self.generation.to_le_bytes());
        }
    }

    fn abort(&mut self) {
        self.pending.clear();
        // graph mutations from any mid-tx flush cannot be undone in place
        self.state
            .write()
            .expect("hnsw index lock poisoned by an earlier panic; reopen the store")
            .stale = true;
        // the tx's GEN_KEY writes roll back while the in-memory counter
        // stays bumped, and the post-abort rebuild matches no stored blob:
        // force the next checkpoint to rewrite
        self.saved_generation = u64::MAX;
    }

    /// Persist the in-memory graph so the next open loads it instead of
    /// re-inserting every vector. Skipped when the last-written blob already
    /// matches the live graph.
    fn checkpoint(&mut self, tx: &mut WriteTx<'_>) {
        if self.generation == self.saved_generation {
            return;
        }
        let ks = self.ks.clone().expect("sink used before init()");
        let gks = self.graph_ks.clone().expect("sink used before init()");
        let mut state = self
            .state
            .write()
            .expect("hnsw index lock poisoned by an earlier panic; reopen the store");
        if state.stale {
            // never checkpoint a stale graph: resync from committed rows
            let entries = tx.iter(&ks).map(|kv| {
                let (k, v) = kv.into_inner().expect("hnsw vector scan failed");
                (k.to_vec(), v.to_vec())
            });
            let (metric, seed) = (self.metric, self.seed);
            state.rebuild(metric, seed, entries);
        }
        let blob = state.index.to_bytes();
        let pairs: Vec<(u32, &K)> = state.keys.iter().map(|(id, k)| (*id, k)).collect();
        let keys_blob =
            postcard::to_stdvec(&pairs).expect("postcard encode of hnsw key table failed");
        drop(state);

        let old_chunks: u32 = tx
            .get(&gks, HDR_KEY)
            .map(|v| {
                postcard::from_bytes::<(u64, u32)>(&v)
                    .expect("corrupt hnsw graph header: postcard decode failed")
                    .1
            })
            .unwrap_or(0);
        let n_chunks = blob.len().div_ceil(CHUNK_BYTES) as u32;
        for (i, chunk) in blob.chunks(CHUNK_BYTES).enumerate() {
            tx.insert(&gks, chunk_key(i as u32), chunk);
        }
        for i in n_chunks..old_chunks {
            tx.remove(&gks, chunk_key(i));
        }
        // also heal the durable counter: after an abort it lags the
        // in-memory generation this blob is written under
        tx.insert(&gks, GEN_KEY, self.generation.to_le_bytes());
        tx.insert(&gks, KEYS_KEY, keys_blob);
        tx.insert(
            &gks,
            HDR_KEY,
            postcard::to_stdvec(&(self.generation, n_chunks))
                .expect("postcard encode of hnsw graph header failed"),
        );
        self.saved_generation = self.generation;
    }

    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        HnswReader {
            tx,
            ks: self.ks.clone().expect("sink used before init()"),
            metric: self.metric,
            seed: self.seed,
            state: Arc::clone(&self.state),
        }
    }
}

/// Read handle for [`Hnsw`], pinned to one snapshot.
pub struct HnswReader<
    'tx,
    R: Readable,
    K,
    T,
    M: Metric<T>,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> {
    tx: &'tx R,
    ks: fjall::SingleWriterTxKeyspace,
    metric: M,
    seed: u64,
    state: Arc<RwLock<State<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>>>,
}

impl<
    'tx,
    R,
    K,
    T,
    M,
    const DIM: usize,
    const M0: usize,
    const TOP_K: usize,
    const EF_SEARCH: usize,
    const EF_BUILD: usize,
    const MAX_LEVEL: usize,
> HnswReader<'tx, R, K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>
where
    R: Readable,
    K: Clone + DeserializeOwned,
    T: Scalar + DeserializeOwned,
    M: Metric<T> + Copy,
{
    fn with_state<Ret>(
        &self,
        f: impl FnOnce(&State<K, T, M, DIM, M0, TOP_K, EF_SEARCH, EF_BUILD, MAX_LEVEL>) -> Ret,
    ) -> Ret {
        // fast path: concurrent readers share the lock
        {
            let state = self
                .state
                .read()
                .expect("hnsw index lock poisoned by an earlier panic; reopen the store");
            if !state.stale {
                return f(&state);
            }
        }
        let mut state = self
            .state
            .write()
            .expect("hnsw index lock poisoned by an earlier panic; reopen the store");
        if state.stale {
            // re-check: another reader may have rebuilt while we waited
            let entries = self.tx.iter(&self.ks).map(|kv| {
                let (k, v) = kv.into_inner().expect("hnsw vector scan failed");
                (k.to_vec(), v.to_vec())
            });
            state.rebuild(self.metric, self.seed, entries);
        }
        f(&state)
    }

    /// The up-to-`TOP_K` approximately nearest keys to `q`, ascending by
    /// distance (smaller is closer).
    pub fn search(&self, q: &[T; DIM]) -> Vec<Scored<M::Out, K>> {
        self.with_state(|state| {
            state
                .index
                .search(&q[..])
                .into_iter()
                .map(|(d, id)| Scored::new(d, state.keys[&id].clone()))
                .collect()
        })
    }

    /// The up-to-`TOP_K` approximately nearest keys among those passing
    /// `pred`, ascending by distance.
    ///
    /// Small match sets are answered exactly by brute force; larger ones by
    /// a filtered graph walk (the filter applies before `TOP_K`-truncation,
    /// unlike filtering the results of [`search`](HnswReader::search), which
    /// can starve below `TOP_K`).
    pub fn search_filtered(
        &self,
        q: &[T; DIM],
        pred: impl Fn(&K) -> bool,
    ) -> Vec<Scored<M::Out, K>> {
        const BRUTE_MAX: usize = 2000;
        self.with_state(|state| {
            let allowed: FxHashSet<u32> = state
                .keys
                .iter()
                .filter(|(_, k)| pred(k))
                .map(|(id, _)| *id)
                .collect();
            let hits = if allowed.len() <= BRUTE_MAX {
                let ids: Vec<u32> = allowed.iter().copied().collect();
                state.index.search_among(&q[..], &ids)
            } else {
                state
                    .index
                    .search_filtered(&q[..], |id| allowed.contains(&id))
            };
            hits.into_iter()
                .map(|(d, id)| Scored::new(d, state.keys[&id].clone()))
                .collect()
        })
    }

    /// Whether `key` is indexed, straight from the persisted rows.
    pub fn contains(&self, key: &K) -> bool
    where
        K: Serialize,
    {
        let k = postcard::to_stdvec(key).expect("postcard encode of hnsw key failed");
        self.tx
            .get(&self.ks, k)
            .expect("hnsw vector read failed")
            .is_some()
    }

    /// The number of live embeddings.
    pub fn len(&self) -> usize {
        self.with_state(|state| state.ids.len())
    }

    /// Whether the index holds no embeddings.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
