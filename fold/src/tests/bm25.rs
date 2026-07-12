use crate::{pipeline::*, stream::*, tests::fresh_db};

fn ids(hits: &[Scored<f64, u32>]) -> Vec<u32> {
    hits.iter().map(|h| h.val).collect()
}

#[test]
fn bm25_rank_and_retract() {
    let docs: &[(u32, &str)] = &[
        (1, "the quick brown fox jumps over the lazy dog"),
        (2, "The Quick Brown Fox!"),
        (3, "rust is a systems programming language rust rust"),
    ];

    let mut st = Stream::new(fresh_db("bm25.db"), terminal::search::Bm25::new("bm25_idx"));

    st.wtx(|tx| {
        for (id, text) in docs {
            tx.insert(&Keyed::new(*id, text.to_string()));
        }
    });

    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 3);

        // both fox docs match; the shorter doc 2 outranks doc 1
        let hits = idx.search("fox", 10);
        assert_eq!(ids(&hits), vec![2, 1]);
        assert!(hits[0].score > hits[1].score);

        // query tokenization matches ingest tokenization
        assert_eq!(ids(&idx.search("FOX!!", 10)), vec![2, 1]);

        assert_eq!(ids(&idx.search("rust", 10)), vec![3]);
        assert!(idx.search("zzzzzz", 10).is_empty());
        assert!(idx.search("", 10).is_empty());

        // rarer term with higher tf dominates: doc 3 tops a mixed query
        let hits = idx.search("rust fox", 10);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].val, 3);

        assert_eq!(idx.search("the quick", 1).len(), 1);
    });

    // insert + remove within one tx cancels before hitting the store
    st.wtx(|tx| {
        let d = Keyed::new(4u32, "ephemeral fox".to_string());
        tx.insert(&d);
        tx.remove(&d);
    });
    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 3);
        assert_eq!(ids(&idx.search("fox", 10)), vec![2, 1]);
    });

    // retracting a doc removes it from results and corpus stats
    st.wtx(|tx| tx.remove(&Keyed::new(2u32, docs[1].1.to_string())));
    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 2);
        assert_eq!(ids(&idx.search("fox", 10)), vec![1]);
    });

    st.wtx(|tx| {
        tx.remove(&Keyed::new(1u32, docs[0].1.to_string()));
        tx.remove(&Keyed::new(3u32, docs[2].1.to_string()));
    });
    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 0);
        assert!(idx.search("fox", 10).is_empty());
        assert!(idx.search("rust", 10).is_empty());
    });
}

#[test]
fn bm25_search_filtered() {
    let mut st = Stream::new(
        fresh_db("bm25_filtered.db"),
        terminal::search::Bm25::<String, String>::new("bm25_idx"),
    );

    st.wtx(|tx| {
        for i in 0..50 {
            let group = if i % 2 == 0 { "even" } else { "odd" };
            // vary length so scores differ deterministically
            tx.insert(&Keyed::new(
                format!("{group}{i}"),
                format!("fox {}", "pad ".repeat(i)),
            ));
        }
    });

    st.rtx(|idx| {
        let all = idx.search("fox", 50);
        assert_eq!(all.len(), 50);

        // filtered matches the full ranking restricted to the predicate:
        // the filter applies before limit-truncation, so hits don't starve
        let filt = idx.search_filtered("fox", 10, |k| k.starts_with("odd"));
        assert_eq!(filt.len(), 10);
        assert!(filt.iter().all(|h| h.val.starts_with("odd")));
        let expect: Vec<_> = all
            .iter()
            .filter(|h| h.val.starts_with("odd"))
            .take(10)
            .collect();
        for (a, b) in filt.iter().zip(expect) {
            assert_eq!(a.val, b.val);
            assert_eq!(a.score, b.score);
        }

        // reject-everything filter returns nothing
        assert!(idx.search_filtered("fox", 10, |_| false).is_empty());
    });
}
