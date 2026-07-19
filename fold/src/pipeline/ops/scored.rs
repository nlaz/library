use std::marker::PhantomData;

use fxhash::FxHashMap;
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    pipeline::{Push, Scored},
    stream::{PipelineInitCtx, Readable, WriteTx},
};

/// A totally ordered value with an order-preserving byte encoding:
/// lexicographic comparison of encodings matches comparison of the values.
///
/// This is what lets scored operators keep their state sorted by score in
/// the LSM store and find boundaries with a range scan. The usual `postcard`
/// encoding does *not* have this property (varints of different lengths
/// don't compare lexicographically), hence a separate trait.
///
/// Provided implementations cover the fixed-width integers (big-endian;
/// signed types flip the sign bit), floats (IEEE total order), and tuples
/// of scores (component-wise concatenation). Custom implementations must
/// keep every encoding of a given type the same length, or tuple
/// composition stops preserving order.
pub trait Score: Clone {
    /// Append the order-preserving encoding of `self` to `buf`.
    fn encode(&self, buf: &mut Vec<u8>);

    /// Decode a score from the front of `bytes`, returning it and the
    /// remaining suffix.
    fn decode(bytes: &[u8]) -> (Self, &[u8]);
}

macro_rules! impl_score_uint {
    ($($t:ty),+) => {$(
        impl Score for $t {
            #[inline]
            fn encode(&self, buf: &mut Vec<u8>) {
                buf.extend_from_slice(&self.to_be_bytes());
            }
            #[inline]
            fn decode(bytes: &[u8]) -> (Self, &[u8]) {
                let (head, rest) = bytes.split_at(size_of::<$t>());
                // invariant: split_at yielded exactly size_of::<$t>() bytes
                (<$t>::from_be_bytes(head.try_into().unwrap()), rest)
            }
        }
    )+};
}
impl_score_uint!(u8, u16, u32, u64, u128);

macro_rules! impl_score_int {
    ($($t:ty),+) => {$(
        impl Score for $t {
            // flipping the sign bit maps the signed range onto the unsigned
            // range order-preservingly: MIN -> 0, -1 -> MAX/2, MAX -> MAX
            #[inline]
            fn encode(&self, buf: &mut Vec<u8>) {
                buf.extend_from_slice(&(*self ^ <$t>::MIN).to_be_bytes());
            }
            #[inline]
            fn decode(bytes: &[u8]) -> (Self, &[u8]) {
                let (head, rest) = bytes.split_at(size_of::<$t>());
                // invariant: split_at yielded exactly size_of::<$t>() bytes
                (<$t>::from_be_bytes(head.try_into().unwrap()) ^ <$t>::MIN, rest)
            }
        }
    )+};
}
impl_score_int!(i8, i16, i32, i64, i128);

macro_rules! impl_score_float {
    ($($t:ty => $b:ty),+) => {$(
        impl Score for $t {
            // IEEE total order: negatives flip all bits (reversing their
            // magnitude order), non-negatives flip just the sign bit
            #[inline]
            fn encode(&self, buf: &mut Vec<u8>) {
                let bits = self.to_bits();
                let bits = if bits >> (<$b>::BITS - 1) == 1 {
                    !bits
                } else {
                    bits ^ (1 << (<$b>::BITS - 1))
                };
                buf.extend_from_slice(&bits.to_be_bytes());
            }
            #[inline]
            fn decode(bytes: &[u8]) -> (Self, &[u8]) {
                let (head, rest) = bytes.split_at(size_of::<$t>());
                // invariant: split_at yielded exactly size_of::<$t>() bytes
                let bits = <$b>::from_be_bytes(head.try_into().unwrap());
                let bits = if bits >> (<$b>::BITS - 1) == 1 {
                    bits ^ (1 << (<$b>::BITS - 1))
                } else {
                    !bits
                };
                (<$t>::from_bits(bits), rest)
            }
        }
    )+};
}
impl_score_float!(f32 => u32, f64 => u64);

macro_rules! impl_score_tuple {
    ($($name:ident $var:ident $idx:tt),+) => {
        impl<$($name: Score),+> Score for ($($name,)+) {
            #[inline]
            fn encode(&self, buf: &mut Vec<u8>) {
                $(self.$idx.encode(buf);)+
            }
            #[inline]
            fn decode(bytes: &[u8]) -> (Self, &[u8]) {
                let rest = bytes;
                $(let ($var, rest) = <$name>::decode(rest);)+
                (($($var,)+), rest)
            }
        }
    };
}
impl_score_tuple!(A a 0);
impl_score_tuple!(A a 0, B b 1);
impl_score_tuple!(A a 0, B b 1, C c 2);

/// Attaches a score computed from each datum, forwarding
/// [`Scored`]`{ score_fn(v), v }`.
///
/// Stateless; deltas pass through unchanged. `score_fn` must be
/// deterministic so retractions land on the same score. Entry point to
/// scored operators like [`TopK`].
pub struct ScoreBy<F, G, S, V> {
    pub score_fn: F,
    pub next: G,
    _p: PhantomData<(S, V)>,
}
impl<S: Clone, V: Clone, F: Fn(&V) -> S, G: Push<Scored<S, V>>> ScoreBy<F, G, S, V> {
    pub fn new(score_fn: F, next: G) -> Self {
        ScoreBy {
            score_fn,
            next,
            _p: PhantomData,
        }
    }
}
impl<S: Clone, V: Clone, F: Fn(&V) -> S, G: Push<Scored<S, V>>> Push<V> for ScoreBy<F, G, S, V> {
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;
    #[inline]
    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.next.init(init)
    }
    #[inline]
    fn push(&mut self, tx: &mut WriteTx<'_>, data: &V, delta: isize) {
        let scored = Scored::new((self.score_fn)(data), data.clone());
        self.next.push(tx, &scored, delta);
    }
    #[inline]
    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        self.next.commit(tx)
    }
    #[inline]
    fn abort(&mut self) {
        self.next.abort()
    }
    #[inline]
    fn checkpoint(&mut self, tx: &mut WriteTx<'_>) {
        self.next.checkpoint(tx)
    }
    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        self.next.reader(tx)
    }
}

/// Discards the score of a [`Scored`] stream, forwarding the bare value.
///
/// Stateless; deltas pass through unchanged. Inverse of [`ScoreBy`],
/// typically used after a scored operator when downstream no longer cares
/// about ordering.
pub struct Unscore<G, S, V> {
    pub next: G,
    _p: PhantomData<(S, V)>,
}
impl<S: Clone, V: Clone, G: Push<V>> Unscore<G, S, V> {
    pub fn new(next: G) -> Self {
        Unscore {
            next,
            _p: PhantomData,
        }
    }
}
impl<S: Clone, V: Clone, G: Push<V>> Push<Scored<S, V>> for Unscore<G, S, V> {
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;
    #[inline]
    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.next.init(init)
    }
    #[inline]
    fn push(&mut self, tx: &mut WriteTx<'_>, data: &Scored<S, V>, delta: isize) {
        self.next.push(tx, &data.val, delta);
    }
    #[inline]
    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        self.next.commit(tx)
    }
    #[inline]
    fn abort(&mut self) {
        self.next.abort()
    }
    #[inline]
    fn checkpoint(&mut self, tx: &mut WriteTx<'_>) {
        self.next.checkpoint(tx)
    }
    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        self.next.reader(tx)
    }
}

/// Forwards only the `k` highest-scored records of a [`Scored`] stream,
/// retracting records as they fall out of the top k.
///
/// Persists the full scored multiset in its own keyspace, ordered by
/// [`Score`] encoding (ties broken by the value's `postcard` encoding).
/// The top `k` copies form a view that is diffed at commit: records
/// entering the view are pushed downstream with a positive delta, records
/// leaving it with a negative one. A retraction of an in-view record
/// promotes the runner-up; an insertion above the boundary evicts the
/// current k-th. Downstream state is thus always consistent with "the
/// current top k". Multiplicity counts against `k` per copy, so a record
/// straddling the boundary is forwarded with its in-view multiplicity.
///
/// Scoring by event timestamp makes this a sliding count window: downstream
/// sinks and aggregates are maintained over the k most recent records, with
/// older data retracted automatically as newer data arrives.
///
/// ```no_run
/// use fold::pipeline::{Aggregate, KeyBy, ScoreBy, TopK, Unkey, Unscore, terminal};
/// use fold::stream::Stream;
///
/// // per-sensor sums over the 100 most recent readings
/// let _st = Stream::new(
///     "readings.db",
///     ScoreBy::new(
///         |r: &(u64, u32, i64)| r.0, // event timestamp
///         TopK::new(
///             "recent",
///             100,
///             Unscore::new(KeyBy::new(
///                 |r: &(u64, u32, i64)| r.1, // sensor id
///                 Aggregate::new(
///                     "sums",
///                     |acc: &mut i64, r: &(u64, u32, i64), d| *acc += r.2 * d as i64,
///                     Unkey::new(terminal::Bag::new("sum_bag")),
///                 ),
///             )),
///         ),
///     ),
/// );
/// ```
pub struct TopK<S, V, G> {
    name: String,
    k: usize,
    ks: Option<fjall::SingleWriterTxKeyspace>,
    // encoded (score, val) -> accumulated delta this tx
    pending: FxHashMap<Vec<u8>, i64>,
    pub next: G,
    _p: PhantomData<(S, V)>,
}
impl<S, V, G> TopK<S, V, G> {
    /// `name` identifies this node's keyspace and must be unique among all
    /// named nodes in the pipeline. `k` is the number of record copies
    /// forwarded downstream.
    pub fn new(name: impl Into<String>, k: usize, next: G) -> Self {
        TopK {
            name: name.into(),
            k,
            ks: None,
            pending: FxHashMap::default(),
            next,
            _p: PhantomData,
        }
    }

    // The current view: the top `k` record copies in descending key order,
    // as encoded key -> number of copies inside the view. O(k) per call.
    fn view(
        &self,
        tx: &WriteTx<'_>,
        ks: &fjall::SingleWriterTxKeyspace,
    ) -> FxHashMap<Vec<u8>, i64> {
        let mut out = FxHashMap::default();
        let mut budget = self.k as i64;
        for kv in tx.iter(ks).rev() {
            if budget == 0 {
                break;
            }
            let (key, val) = kv.into_inner().expect("topk scan failed");
            let n = i64::from_be_bytes(
                *val.as_array::<8>()
                    .expect("corrupt topk count: not 8 bytes"),
            )
            .min(budget);
            budget -= n;
            out.insert(key.to_vec(), n);
        }
        out
    }
}
impl<S, V, G> Push<Scored<S, V>> for TopK<S, V, G>
where
    S: Score,
    V: Clone + Serialize + DeserializeOwned,
    G: Push<Scored<S, V>>,
{
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.ks = Some(init.keyspace(&self.name));
        self.next.init(init);
    }

    fn push(&mut self, tx: &mut WriteTx<'_>, data: &Scored<S, V>, delta: isize) {
        tx.buf.clear();
        data.score.encode(&mut tx.buf);
        postcard::to_io(&data.val, &mut tx.buf).expect("postcard encode of topk value failed");
        *self.pending.entry(tx.buf.clone()).or_insert(0) += delta as i64;
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        if self.pending.is_empty() {
            self.next.commit(tx);
            return;
        }
        let ks = self.ks.clone().expect("sink used before init()");

        let old = self.view(tx, &ks);

        // fold pending deltas into the stored multiset, Bag-style
        for (key, delta) in self.pending.drain() {
            if delta == 0 {
                continue;
            }
            let cur = tx
                .get(&ks, &key)
                .map(|v| {
                    i64::from_be_bytes(
                        v.as_ref()
                            .try_into()
                            .expect("corrupt topk count: not 8 bytes"),
                    )
                })
                .unwrap_or(0);
            let new = cur + delta;
            debug_assert!(new >= 0, "TopK multiplicity went negative");
            if new > 0 {
                tx.insert(&ks, &key, new.to_be_bytes());
            } else {
                tx.remove(&ks, &key);
            }
        }

        // emit the view diff: entries whose in-view multiplicity changed
        let new = self.view(tx, &ks);
        let decode = |key: &[u8]| {
            let (score, rest) = S::decode(key);
            Scored::new(
                score,
                postcard::from_bytes::<V>(rest)
                    .expect("corrupt topk value: postcard decode failed"),
            )
        };
        for (key, &new_n) in &new {
            let d = new_n - old.get(key).copied().unwrap_or(0);
            if d != 0 {
                self.next.push(tx, &decode(key), d as isize);
            }
        }
        for (key, &old_n) in &old {
            if !new.contains_key(key) {
                self.next.push(tx, &decode(key), -old_n as isize);
            }
        }
        self.next.commit(tx);
    }

    fn abort(&mut self) {
        self.pending.clear();
        self.next.abort();
    }

    fn checkpoint(&mut self, tx: &mut WriteTx<'_>) {
        self.next.checkpoint(tx)
    }
    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        self.next.reader(tx)
    }
}
