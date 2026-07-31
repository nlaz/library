use crate::{pipeline::*, stream::*, tests::fresh_db};
use anny::metric::L2;

type VecSink = terminal::search::Hnsw<u32, f32, L2, 4>;

#[test]
fn db_stats_covers_keyspaces() {
    let mut st = Stream::new(fresh_db("stats_db.db"), VecSink::new("vecs", L2, 42));
    st.wtx(|tx| {
        for i in 0..50u32 {
            tx.insert(&Keyed::new(i, [i as f32, 0.0, 0.0, 0.0]));
        }
    });
    st.checkpoint();

    let db = st.db_stats();
    let names: Vec<&str> = db.keyspaces.iter().map(|k| k.name.as_str()).collect();
    // covers the sink keyspace AND its graph sibling, with no sink cooperation
    assert!(names.contains(&"sink_vecs"), "keyspaces: {names:?}");
    assert!(names.contains(&"sink_vecs_graph"), "keyspaces: {names:?}");
    assert_eq!(db.block_cache_capacity, 32 * 1024 * 1024);
    assert!(db.disk_bytes > 0);
    let vecs = db.keyspaces.iter().find(|k| k.name == "sink_vecs").unwrap();
    assert!(vecs.approx_len >= 50, "approx_len: {}", vecs.approx_len);
}

#[test]
fn hnsw_reader_stats_track_shape_without_rebuild() {
    let mut st = Stream::new(fresh_db("stats_hnsw.db"), VecSink::new("vecs", L2, 42));
    st.wtx(|tx| {
        for i in 0..20u32 {
            tx.insert(&Keyed::new(i, [i as f32, 0.0, 0.0, 0.0]));
        }
    });
    st.rtx(|idx| {
        let s = idx.stats();
        assert_eq!(s.live, idx.len());
        assert_eq!((s.live, s.slots, s.free_slots), (20, 20, 0));
        assert_eq!(s.map_entries, 20);
        assert_eq!((s.dim, s.dtype_bytes, s.m0), (4, 4, 32));
        assert!(s.graph_bytes > 0 && s.map_bytes > 0);
        assert!(!s.stale);
    });

    // a net removal tombstones: high-water slots outlive the live count
    st.wtx(|tx| tx.remove(&Keyed::new(1, [1.0, 0.0, 0.0, 0.0])));
    st.rtx(|idx| {
        let s = idx.stats();
        assert_eq!((s.live, s.slots, s.free_slots), (19, 20, 1));
        assert_eq!(s.map_entries, 19);
    });

    // a panicking tx after a mid-tx flush marks the graph stale; stats must
    // report that, not trigger the corpus rebuild with_state would pay
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.wtx(|tx| {
            tx.insert(&Keyed::new(9, [5.0, 5.0, 5.0, 5.0]));
            tx.rtx(|idx| assert!(!idx.search(&[5.0, 5.0, 5.0, 5.0]).is_empty()));
            panic!("abort");
        });
    }));
    assert!(r.is_err());
    st.rtx(|idx| {
        assert!(
            idx.stats().stale,
            "stats must report staleness, not rebuild"
        );
        // a search takes the rebuild path; stats then sees a fresh graph
        assert!(!idx.search(&[0.0, 0.0, 0.0, 0.0]).is_empty());
        assert!(!idx.stats().stale);
    });
}

#[test]
fn bm25_cache_stats_cold_then_warm() {
    let path = fresh_db("stats_bm25.db");
    {
        let mut st = Stream::new(&path, terminal::search::Bm25::new("lex"));
        st.wtx(|tx| {
            tx.insert(&Keyed::new(1u32, "hello world".to_string()));
            tx.insert(&Keyed::new(2u32, "goodbye world".to_string()));
        });
    }
    // a fresh open is the truly cold state (a commit also materializes the
    // cache); the first search pays the DOCLEN scan
    let st = Stream::new(&path, terminal::search::Bm25::<u32, String>::new("lex"));
    st.rtx(|idx| {
        let cold = idx.cache_stats();
        assert!(!cold.warmed);
        assert_eq!((cold.entries, cold.bytes), (0, 0));
        // reading the stats must not itself warm the cache
        assert!(!idx.cache_stats().warmed);

        assert_eq!(idx.search("world", 10).len(), 2);
        let warm = idx.cache_stats();
        assert!(warm.warmed);
        assert_eq!(warm.entries, 2);
        assert!(warm.bytes > 0);
    });
}
