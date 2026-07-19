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

#[test]
fn doc_len_tracks_updates_and_removals() {
    let mut st = Stream::new(
        fresh_db("bm25_doclen.db"),
        terminal::search::Bm25::new("bm25_idx"),
    );

    // doc 2 is a fixed control: never touched, never matches "zeta", but
    // its length pulls on avgdl so doc 1's length normalization is
    // observable
    let control = Keyed::new(2u32, "pad pad pad pad pad pad pad".to_string());
    let short = Keyed::new(1u32, "zeta zeta zeta".to_string());

    st.wtx(|tx| {
        tx.insert(&short);
        tx.insert(&control);
    });

    let short_score = st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 2);
        let hits = idx.search("zeta", 10);
        assert_eq!(ids(&hits), vec![1]);
        hits[0].score
    });

    // update doc 1 in place: retract the old text, then insert new text
    // with the same term frequency but more padding tokens. Per the
    // set-semantic update contract, this must happen across separate write
    // transactions -- retracting and inserting different content within one
    // tx would sum the deltas instead of replacing the document.
    let long = Keyed::new(
        1u32,
        "zeta zeta zeta filler filler filler filler filler filler filler".to_string(),
    );
    st.wtx(|tx| tx.remove(&short));
    st.wtx(|tx| tx.insert(&long));

    let long_score = st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 2);
        let hits = idx.search("zeta", 10);
        assert_eq!(ids(&hits), vec![1]);
        hits[0].score
    });

    // same term frequency, but doc 1 is now much longer -- BM25 length
    // normalization penalizes it relative to the pre-update score, proving
    // the update's new length (not the stale one) fed the scoring
    assert!(long_score < short_score, "{long_score} !< {short_score}");

    // remove doc 1 entirely
    st.wtx(|tx| tx.remove(&long));
    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 1);
        assert!(idx.search("zeta", 10).is_empty());
        // the control doc was never touched and is still findable
        assert_eq!(ids(&idx.search("pad", 10)), vec![2]);
    });
}

#[test]
fn idf_decreases_with_document_frequency() {
    let docs: &[(u32, &str)] = &[
        (1, "rare common pad pad pad"),
        (2, "common pad pad pad pad"),
        (3, "common pad pad pad pad"),
        (4, "common pad pad pad pad"),
    ];

    let mut st = Stream::new(
        fresh_db("bm25_idf.db"),
        terminal::search::Bm25::new("bm25_idx"),
    );
    st.wtx(|tx| {
        for (id, text) in docs {
            tx.insert(&Keyed::new(*id, text.to_string()));
        }
    });

    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 4);

        // "rare" occurs in 1 of 4 docs, "common" in all 4 -- and doc 1
        // contains both exactly once, with every doc the same length, so tf
        // and length normalization are identical between the two queries on
        // doc 1. Only the IDF term differs, isolating its contribution.
        let rare_hits = idx.search("rare", 10);
        assert_eq!(ids(&rare_hits), vec![1]);
        let rare_score = rare_hits[0].score;

        let common_hits = idx.search("common", 10);
        assert_eq!(common_hits.len(), 4);
        let common_score_doc1 = common_hits
            .iter()
            .find(|h| h.val == 1)
            .expect("doc 1 matches 'common'")
            .score;

        assert!(
            rare_score > common_score_doc1,
            "{rare_score} !> {common_score_doc1}"
        );
    });
}

#[test]
fn bm25_score_orders_tf_over_length() {
    let docs: &[(u32, &str)] = &[
        (1, "gamma gamma"),
        (2, "gamma pad pad pad pad pad pad pad pad pad"),
    ];

    let mut st = Stream::new(
        fresh_db("bm25_tf_len.db"),
        terminal::search::Bm25::new("bm25_idx"),
    );
    st.wtx(|tx| {
        for (id, text) in docs {
            tx.insert(&Keyed::new(*id, text.to_string()));
        }
    });

    st.rtx(|idx| {
        // doc 1: "gamma" twice in a two-token doc; doc 2: "gamma" once in a
        // ten-token doc -- the higher term frequency in the shorter
        // document outranks a single occurrence diluted by a much longer
        // document
        let hits = idx.search("gamma", 10);
        assert_eq!(ids(&hits), vec![1, 2]);
        assert!(hits[0].score > hits[1].score);
    });
}
