use crate::{pipeline::*, stream::*, tests::fresh_db};

#[test]
fn rtx_in_wtx() {
    let mut st = Stream::new(
        fresh_db("rtx.db"),
        Distinct::new(
            "distinct",
            (terminal::Count::new("count"), terminal::Bag::new("bag")),
        ),
    );

    st.wtx(|tx| {
        tx.insert(&1u32);
        tx.insert(&1u32); // duplicate, buffered in Distinct
        tx.insert(&2u32);

        // mid-tx read observes everything pushed so far
        tx.rtx(|(count, bag)| {
            assert_eq!(count.get(), 2);
            assert!(bag.contains(&1));
            assert!(bag.contains(&2));
            assert!(!bag.contains(&3));
        });

        // pushes resume after the read
        tx.insert(&3u32);
        tx.remove(&1u32); // one of two copies: still distinct-present

        tx.rtx(|(count, bag)| {
            assert_eq!(count.get(), 3);
            assert!(bag.contains(&1));
            assert!(bag.contains(&3));
        });
    });

    // committed state matches the last mid-tx view
    st.rtx(|(count, bag)| {
        assert_eq!(count.get(), 3);
        assert!(bag.contains(&1));
        assert!(bag.contains(&2));
        assert!(bag.contains(&3));
    });

    // a panicking tx rolls back everything a mid-tx read observed
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.wtx(|tx| {
            tx.insert(&4u32);
            tx.rtx(|(count, _)| assert_eq!(count.get(), 4));
            panic!("abort");
        });
    }));
    assert!(r.is_err());
    st.rtx(|(count, bag)| {
        assert_eq!(count.get(), 3);
        assert!(!bag.contains(&4));
    });
}
