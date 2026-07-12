use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fxhash::FxHashMap;
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    pipeline::{Push, Score},
    stream::{PipelineInitCtx, Readable, WriteTx},
};

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Forwards records downstream, then retracts each one automatically once
/// its age exceeds `horizon` — a processing-time sliding window.
///
/// Records are stamped with the wall-clock time of the transaction that
/// commits them; the record itself carries no timestamp. Every commit first
/// expires all buffered records older than `horizon`, pushing each
/// downstream with the opposite delta, so downstream sinks and aggregates
/// always reflect "records seen within the last `horizon`". An empty write
/// transaction is enough to advance the clock and trigger expiry:
///
/// ```no_run
/// use std::time::Duration;
/// use fold::pipeline::{Retain, terminal};
/// use fold::stream::Stream;
///
/// // how many events arrived in the last hour?
/// let mut st = Stream::new(
///     "events.db",
///     Retain::new("hour", Duration::from_secs(3600), terminal::Count::new("count")),
/// );
/// st.wtx(|tx| tx.insert(&"click".to_string()));
/// // ... later: expire aged-out records without inserting anything
/// st.wtx(|_| {});
/// ```
///
/// Upstream retractions cancel buffered copies oldest-first. Retracting a
/// record whose copies have already expired is absorbed rather than
/// forwarded — the expiry already retracted it downstream, so each copy
/// yields exactly one downstream retraction.
///
/// The buffer is persistent: records survive reopening the stream and keep
/// expiring on schedule. Time defaults to the system clock ([`SystemTime`];
/// a monotonic `Instant` cannot be persisted), so horizons are best-effort
/// across clock jumps. [`Retain::with_clock`] substitutes a custom clock
/// for tests or simulated time.
pub struct Retain<V, G, C = fn() -> u64> {
    name: String,
    horizon_ms: u64,
    clock: C,
    // next arrival sequence number; disambiguates same-millisecond stamps
    seq: u64,
    // (ms, seq) -> (copies, record): the buffer, in arrival order
    ks_buf: Option<fjall::SingleWriterTxKeyspace>,
    // postcard(record) ++ (ms, seq) -> (): locates a record's buffer entries
    ks_idx: Option<fjall::SingleWriterTxKeyspace>,
    // encoded record -> (decoded record, net delta this tx)
    pending: FxHashMap<Vec<u8>, (V, i64)>,
    scratch: Vec<u8>,
    pub next: G,
}

impl<V, G> Retain<V, G> {
    /// `name` identifies this node's keyspaces and must be unique among all
    /// named nodes in the pipeline. Records are retracted once they have
    /// been buffered for `horizon` (millisecond granularity).
    pub fn new(name: impl Into<String>, horizon: Duration, next: G) -> Self {
        Self::with_clock(name, horizon, wall_clock_ms, next)
    }
}

impl<V, G, C: Fn() -> u64> Retain<V, G, C> {
    /// Like [`new`](Retain::new), but reads the current time in unix
    /// milliseconds from `clock` instead of the system clock.
    pub fn with_clock(name: impl Into<String>, horizon: Duration, clock: C, next: G) -> Self {
        Retain {
            name: name.into(),
            horizon_ms: horizon.as_millis() as u64,
            clock,
            seq: 0,
            ks_buf: None,
            ks_idx: None,
            pending: FxHashMap::default(),
            scratch: Default::default(),
            next,
        }
    }
}

impl<V, G, C> Push<V> for Retain<V, G, C>
where
    V: Clone + Serialize + DeserializeOwned,
    G: Push<V>,
    C: Fn() -> u64,
{
    type Reader<'tx, R: Readable + 'tx> = G::Reader<'tx, R>;

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        let ks_buf = init.keyspace(&format!("{}_buf", self.name));
        self.ks_idx = Some(init.keyspace(&format!("{}_idx", self.name)));
        // resume the arrival sequence past any persisted buffer entries
        if let Some(last) = init.snapshot().last_key_value(&ks_buf) {
            let ((_, seq), _) = <(u64, u64)>::decode(&last.key().unwrap());
            self.seq = seq + 1;
        }
        self.ks_buf = Some(ks_buf);
        self.next.init(init);
    }

    fn push(&mut self, tx: &mut WriteTx<'_>, data: &V, delta: isize) {
        tx.buf.clear();
        postcard::to_io(data, &mut tx.buf).unwrap();
        self.pending
            .entry(tx.buf.clone())
            .or_insert_with(|| (data.clone(), 0))
            .1 += delta as i64;
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        let ks_buf = self.ks_buf.clone().unwrap();
        let ks_idx = self.ks_idx.clone().unwrap();
        let now = (self.clock)();

        // expire everything stamped before the horizon; the buffer is in
        // arrival order, so stop at the first live entry
        let cutoff = now.saturating_sub(self.horizon_ms);
        let mut expired = Vec::new();
        for kv in tx.iter(&ks_buf) {
            let (key, value) = kv.into_inner().unwrap();
            let ((ms, _), _) = <(u64, u64)>::decode(&key);
            if ms >= cutoff {
                break;
            }
            expired.push((key.to_vec(), value.to_vec()));
        }
        for (key, value) in expired {
            let (count, val): (i64, V) = postcard::from_bytes(&value).unwrap();

            self.scratch.clear();
            postcard::to_io(&val, &mut self.scratch).unwrap();

            let idx_key = &mut self.scratch;
            idx_key.extend_from_slice(&key);
            tx.remove(&ks_buf, &key);
            tx.remove(&ks_idx, &idx_key);
            self.next.push(tx, &val, -(count as isize));
        }

        // apply this transaction's net deltas
        for (enc_val, (val, delta)) in self.pending.drain() {
            if delta == 0 {
                continue;
            }
            if delta > 0 {
                // stamp and buffer, then forward
                let mut key = Vec::with_capacity(16);
                (now, self.seq).encode(&mut key);
                self.seq += 1;

                self.scratch.clear();
                postcard::to_io(&(delta, &val), &mut self.scratch).unwrap();

                tx.insert(&ks_buf, &key, &self.scratch);
                let mut idx_key = enc_val;
                idx_key.extend_from_slice(&key);
                tx.insert(&ks_idx, &idx_key, []);
                self.next.push(tx, &val, delta as isize);
            } else {
                // cancel buffered copies oldest-first; postcard encodings of
                // one type are prefix-free, so the prefix scan only matches
                // this record's entries
                let mut idx_keys = Vec::new();
                for kv in tx.prefix(&ks_idx, &enc_val) {
                    idx_keys.push(kv.key().unwrap().to_vec());
                }
                let mut need = -delta;
                let mut matched = 0;
                for idx_key in idx_keys {
                    if need == 0 {
                        break;
                    }
                    let key = &idx_key[enc_val.len()..];
                    let value = tx.get(&ks_buf, key).unwrap();
                    let (count, _): (i64, V) = postcard::from_bytes(&value).unwrap();
                    let take = count.min(need);
                    if take == count {
                        tx.remove(&ks_buf, key);
                        tx.remove(&ks_idx, &idx_key);
                    } else {
                        self.scratch.clear();
                        postcard::to_io(&(count - take, &val), &mut self.scratch).unwrap();
                        tx.insert(&ks_buf, key, &self.scratch);
                    }
                    need -= take;
                    matched += take;
                }
                // copies not found have already expired: the expiry
                // retracted them downstream, so absorb the difference
                if matched > 0 {
                    self.next.push(tx, &val, -(matched as isize));
                }
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
