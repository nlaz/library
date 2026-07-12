use std::marker::PhantomData;

use fxhash::FxHashMap;
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    pipeline::{Keyed, Push},
    stream::{PipelineInitCtx, Readable, WriteTx},
};

/// Attaches a key computed from each datum, forwarding
/// [`Keyed`]`{ key_fn(v), v }`.
///
/// Stateless; deltas pass through unchanged. `key_fn` must be deterministic
/// so retractions land on the same key. Entry point to keyed operators like
/// [`Aggregate`].
pub struct KeyBy<F, G, K, V> {
    pub key_fn: F,
    pub next: G,
    _p: PhantomData<(K, V)>,
}
impl<K: Clone, V: Clone, F: Fn(&V) -> K, G: Push<Keyed<K, V>>> KeyBy<F, G, K, V> {
    pub fn new(key_fn: F, next: G) -> Self {
        KeyBy {
            key_fn,
            next,
            _p: PhantomData,
        }
    }
}
impl<K: Clone, V: Clone, F: Fn(&V) -> K, G: Push<Keyed<K, V>>> Push<V> for KeyBy<F, G, K, V> {
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;
    #[inline]
    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.next.init(init)
    }
    #[inline]
    fn push(&mut self, tx: &mut WriteTx<'_>, data: &V, delta: isize) {
        let keyed = Keyed::new((self.key_fn)(data), data.clone());
        self.next.push(tx, &keyed, delta);
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
    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        self.next.reader(tx)
    }
}

/// Discards the key of a [`Keyed`] stream, forwarding the bare value.
///
/// Stateless; deltas pass through unchanged. Inverse of [`KeyBy`], typically
/// used after a keyed operator when downstream no longer cares about
/// grouping.
pub struct Unkey<G, K, V> {
    pub next: G,
    _p: PhantomData<(K, V)>,
}
impl<K: Clone, V: Clone, G: Push<V>> Unkey<G, K, V> {
    pub fn new(next: G) -> Self {
        Unkey {
            next,
            _p: PhantomData,
        }
    }
}
impl<K: Clone, V: Clone, G: Push<V>> Push<Keyed<K, V>> for Unkey<G, K, V> {
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;
    #[inline]
    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.next.init(init)
    }
    #[inline]
    fn push(&mut self, tx: &mut WriteTx<'_>, data: &Keyed<K, V>, delta: isize) {
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
    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        self.next.reader(tx)
    }
}

/// Per-key incremental aggregation over `Keyed<K, V>` streams.
///
/// The step function receives the delta and must handle retractions, i.e. the
/// accumulator update must be invertible w.r.t. negative deltas:
///
/// ```no_run
/// use fold::pipeline::{Aggregate, KeyBy, Unkey, terminal};
/// use fold::stream::Stream;
///
/// // sum of u64 values, grouped by residue mod 3
/// let _st = Stream::new(
///     "sums.db",
///     KeyBy::new(
///         |v: &u64| v % 3,
///         Aggregate::new(
///             "sums",
///             |acc: &mut i64, v: &u64, d| *acc += *v as i64 * d as i64,
///             Unkey::new(terminal::Bag::new("sum_bag")),
///         ),
///     ),
/// );
/// ```
///
/// Persists `(record_count, acc)` per key. Downstream sees the aggregate as a
/// changelog: `Keyed { key, old_acc } @ -1` then `Keyed { key, new_acc } @ +1`.
/// A key's aggregate is removed (retraction only, no re-insert) when its
/// record count reaches 0.
pub struct Aggregate<K, V, A, F, G> {
    name: String,
    ks: Option<fjall::SingleWriterTxKeyspace>,
    pub step: F,
    // encoded key -> (decoded key, buffered (value, delta) updates this tx)
    pending: FxHashMap<Vec<u8>, (K, Vec<(V, isize)>)>,
    pub next: G,
    _p: PhantomData<A>,
}
impl<K, V, A, F, G> Aggregate<K, V, A, F, G> {
    /// `name` identifies this node's keyspace and must be unique among all
    /// named nodes in the pipeline. `step` folds one `(value, delta)` update
    /// into the accumulator.
    pub fn new(name: impl Into<String>, step: F, next: G) -> Self {
        Aggregate {
            name: name.into(),
            ks: None,
            step,
            pending: FxHashMap::default(),
            next,
            _p: PhantomData,
        }
    }
}
impl<K, V, A, F, G> Push<Keyed<K, V>> for Aggregate<K, V, A, F, G>
where
    K: Clone + Serialize,
    V: Clone,
    A: Clone + Default + Serialize + DeserializeOwned,
    F: Fn(&mut A, &V, isize),
    G: Push<Keyed<K, A>>,
{
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.ks = Some(init.keyspace(&self.name));
        self.next.init(init);
    }

    fn push(&mut self, tx: &mut WriteTx<'_>, data: &Keyed<K, V>, delta: isize) {
        tx.buf.clear();
        postcard::to_io(&data.key, &mut tx.buf).unwrap();
        self.pending
            .entry(tx.buf.clone())
            .or_insert_with(|| (data.key.clone(), Vec::new()))
            .1
            .push((data.val.clone(), delta));
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        let ks = self.ks.clone().unwrap();
        for (enc_key, (key, updates)) in self.pending.drain() {
            let (mut count, mut acc): (i64, A) = tx
                .get(&ks, &enc_key)
                .map(|v| postcard::from_bytes(&v).unwrap())
                .unwrap_or((0, A::default()));

            // retract previous aggregate if the key existed
            if count > 0 {
                self.next
                    .push(tx, &Keyed::new(key.clone(), acc.clone()), -1);
            }

            for (val, delta) in &updates {
                count += *delta as i64;
                (self.step)(&mut acc, val, *delta);
            }
            debug_assert!(count >= 0, "Aggregate record count went negative");

            if count > 0 {
                self.next.push(tx, &Keyed::new(key, acc.clone()), 1);
                tx.buf.clear();
                postcard::to_io(&(count, &acc), &mut tx.buf).unwrap();
                let v = std::mem::take(&mut tx.buf);
                tx.insert(&ks, &enc_key, &v);
                tx.buf = v;
            } else {
                tx.remove(&ks, &enc_key);
            }
        }
        self.next.commit(tx);
    }

    fn abort(&mut self) {
        self.pending.clear();
        self.next.abort();
    }

    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        self.next.reader(tx)
    }
}
