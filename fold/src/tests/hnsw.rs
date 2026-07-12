use crate::{pipeline::*, stream::*, tests::fresh_db};
use anny::metric::L2;

type Sink = terminal::search::Hnsw<u32, f32, L2, 4>;

fn ids(hits: &[Scored<f32, u32>]) -> Vec<u32> {
    hits.iter().map(|h| h.val).collect()
}

#[test]
fn hnsw_nearest_upsert_retract_recover() {
    let path = fresh_db("hnsw.db");
    let mut st = Stream::new(&path, Sink::new("vecs", L2, 42));

    st.wtx(|tx| {
        tx.insert(&Keyed::new(1, [0.0, 0.0, 0.0, 0.0]));
        tx.insert(&Keyed::new(2, [1.0, 0.0, 0.0, 0.0]));
        tx.insert(&Keyed::new(3, [10.0, 10.0, 10.0, 10.0]));
    });

    st.rtx(|idx| {
        assert_eq!(idx.len(), 3);
        let hits = idx.search(&[0.1, 0.0, 0.0, 0.0]);
        assert_eq!(ids(&hits), vec![1, 2, 3]);
        assert!(hits.windows(2).all(|w| w[0].score <= w[1].score));
        assert_eq!(ids(&idx.search(&[9.0, 9.0, 9.0, 9.0]))[0], 3);
    });

    // upsert moves key 1 across the space; it must leave its old spot
    st.wtx(|tx| tx.insert(&Keyed::new(1, [20.0, 20.0, 20.0, 20.0])));
    st.rtx(|idx| {
        assert_eq!(idx.len(), 3);
        assert_eq!(ids(&idx.search(&[0.1, 0.0, 0.0, 0.0]))[0], 2);
        assert_eq!(ids(&idx.search(&[20.0, 20.0, 20.0, 20.0]))[0], 1);
    });

    // retraction removes the node; the neighborhood heals
    st.wtx(|tx| tx.remove(&Keyed::new(2, [1.0, 0.0, 0.0, 0.0])));
    st.rtx(|idx| {
        assert_eq!(idx.len(), 2);
        assert_eq!(ids(&idx.search(&[0.1, 0.0, 0.0, 0.0])), vec![3, 1]);
    });

    // insert + retract within one tx nets out before touching the graph
    st.wtx(|tx| {
        let d = Keyed::new(4, [5.0, 5.0, 5.0, 5.0]);
        tx.insert(&d);
        tx.remove(&d);
    });
    st.rtx(|idx| assert_eq!(idx.len(), 2));

    // a panicking tx after a mid-tx flush marks the graph stale; the next
    // read rebuilds it from committed state
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.wtx(|tx| {
            tx.insert(&Keyed::new(9, [5.0, 5.0, 5.0, 5.0]));
            tx.rtx(|idx| assert_eq!(ids(&idx.search(&[5.0, 5.0, 5.0, 5.0]))[0], 9));
            panic!("abort");
        });
    }));
    assert!(r.is_err());
    st.rtx(|idx| {
        assert_eq!(idx.len(), 2);
        assert!(
            ids(&idx.search(&[5.0, 5.0, 5.0, 5.0]))
                .iter()
                .all(|&k| k != 9)
        );
    });

    // reopening rebuilds the graph from the persisted vectors
    drop(st);
    let st = Stream::new(&path, Sink::new("vecs", L2, 42));
    st.rtx(|idx| {
        assert_eq!(idx.len(), 2);
        assert_eq!(ids(&idx.search(&[20.0, 20.0, 20.0, 20.0]))[0], 1);
        assert_eq!(ids(&idx.search(&[9.0, 9.0, 9.0, 9.0]))[0], 3);
    });
}

type StrSink = terminal::search::Hnsw<String, f32, L2, 4>;

fn v(x: f32) -> [f32; 4] {
    [x, 0.0, 0.0, 0.0]
}

#[test]
fn hnsw_checkpoint_blob_reopen() {
    let path = fresh_db("hnsw_ckpt.db");
    let mk = || StrSink::new("vecs", L2, 42);

    // build + checkpoint: blob written
    let mut st = Stream::new(&path, mk());
    st.wtx(|tx| {
        for i in 0..200 {
            tx.insert(&Keyed::new(format!("k{i}"), v(i as f32)));
        }
    });
    let baseline: Vec<String> = st.rtx(|ix| ix.search(&v(42.3)).into_iter().map(|h| h.val).collect());
    st.checkpoint();
    drop(st);

    // reopen: fast path must load the blob and give identical results
    let mut st = Stream::new(&path, mk());
    st.rtx(|ix| {
        assert_eq!(ix.len(), 200);
        let got: Vec<String> = ix.search(&v(42.3)).into_iter().map(|h| h.val).collect();
        assert_eq!(got, baseline, "blob-loaded graph differs from original");
    });

    // stale blob: mutate WITHOUT checkpointing, reopen -> replay path
    st.wtx(|tx| tx.insert(&Keyed::new("fresh".to_string(), v(999.0))));
    drop(st); // no checkpoint: blob gen is now behind
    let mut st = Stream::new(&path, mk());
    st.rtx(|ix| {
        assert_eq!(ix.len(), 201);
        assert_eq!(ix.search(&v(999.0))[0].val, "fresh");
    });

    // checkpoint after replay refreshes the blob; reopen loads it
    st.checkpoint();
    drop(st);
    let mut st = Stream::new(&path, mk());
    st.rtx(|ix| {
        assert_eq!(ix.len(), 201);
        assert_eq!(ix.search(&v(999.0))[0].val, "fresh");
        assert_eq!(ix.search(&v(23.2))[0].val, "k23");
    });

    // abort after a mid-tx flush: reads resync from committed rows, and the
    // pre-abort blob still matches committed state on reopen (the aborted
    // tx's generation bump rolled back with it). The store cannot accept
    // writes after a panicked tx (fjall poisons the write lock), so
    // checkpoint-after-abort is exercised only across a reopen.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.wtx(|tx| {
            tx.insert(&Keyed::new("doomed".to_string(), v(555.0)));
            tx.rtx(|ix| assert_eq!(ix.search(&v(555.0))[0].val, "doomed"));
            panic!("abort");
        });
    }));
    assert!(r.is_err());
    st.rtx(|ix| {
        assert_eq!(ix.len(), 201);
        assert!(ix.search(&v(555.0)).iter().all(|h| h.val != "doomed"));
    });
    drop(st);
    let st = Stream::new(&path, mk());
    st.rtx(|ix| {
        assert_eq!(ix.len(), 201);
        assert!(ix.search(&v(555.0)).iter().all(|h| h.val != "doomed"));
        assert_eq!(ix.search(&v(999.0))[0].val, "fresh");
    });
}

#[test]
fn hnsw_search_filtered() {
    let path = fresh_db("hnsw_filter.db");

    let mut st = Stream::new(&path, StrSink::new("vecs", L2, 42));
    st.wtx(|tx| {
        for i in 0..300 {
            let group = if i % 3 == 0 { "a" } else { "b" };
            tx.insert(&Keyed::new(format!("{group}{i}"), v(i as f32)));
        }
    });

    st.rtx(|ix| {
        // plain search near 90 is dominated by b-keys; filtered must
        // return ONLY a-keys and still fill up to TOP_K
        let hits = ix.search_filtered(&v(90.2), |k| k.starts_with('a'));
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.val.starts_with('a')));
        assert_eq!(hits[0].val, "a90");
        assert!(hits.len() >= 5, "filtered search starved: {}", hits.len());
        // tiny predicate set -> brute path, exact
        let hits = ix.search_filtered(&v(0.0), |k| k == "a0" || k == "a3");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].val, "a0");
        // contains reads the persisted rows
        assert!(ix.contains(&"a0".to_string()));
        assert!(!ix.contains(&"nope".to_string()));
    });
}
