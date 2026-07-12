use crate::{pipeline::*, stream::*, tests::fresh_db};

#[test]
fn keyed_stream_upserts_and_removes_by_key() {
    let mut st = KeyedStream::new(
        fresh_db("keyed_stream.db"),
        (
            terminal::Table::new("rows"),
            Unkey::new(terminal::Bag::new("bag")),
        ),
    );

    st.wtx(|tx| {
        assert_eq!(tx.upsert(&1u32, &"a".to_string()), None);
        assert_eq!(tx.upsert(&2, &"b".to_string()), None);
    });
    st.rtx(|(rows, bag)| {
        assert_eq!(rows.get(&1), Some("a".to_string()));
        assert!(bag.contains(&"a".to_string()));
        assert!(bag.contains(&"b".to_string()));
    });
    assert_eq!(st.get(&1), Some("a".to_string()));
    assert!(st.contains(&2));
    assert!(!st.contains(&3));

    // upsert retracts the replaced record from the graph
    st.wtx(|tx| assert_eq!(tx.upsert(&1, &"c".to_string()), Some("a".to_string())));
    st.rtx(|(rows, bag)| {
        assert_eq!(rows.get(&1), Some("c".to_string()));
        assert!(!bag.contains(&"a".to_string()));
        assert!(bag.contains(&"c".to_string()));
    });

    // unchanged upsert: replaced record returned, no graph churn
    st.wtx(|tx| assert_eq!(tx.upsert(&1, &"c".to_string()), Some("c".to_string())));
    st.rtx(|(rows, bag)| {
        assert_eq!(rows.get(&1), Some("c".to_string()));
        assert_eq!(bag.iter().filter(|(v, _)| v == "c").count(), 1);
    });

    // removal is by key: the stored record is retracted for us
    st.wtx(|tx| {
        assert_eq!(tx.remove(&2), Some("b".to_string()));
        assert_eq!(tx.remove(&2), None); // already gone
        assert_eq!(tx.remove(&9), None); // never existed
    });
    st.rtx(|(rows, bag)| {
        assert_eq!(rows.get(&2), None);
        assert!(!bag.contains(&"b".to_string()));
    });
    assert!(!st.contains(&2));

    // same-tx upserts chain; in-tx get and rtx see uncommitted state
    st.wtx(|tx| {
        tx.upsert(&4, &"x".to_string());
        assert_eq!(tx.get(&4), Some("x".to_string()));
        assert_eq!(tx.upsert(&4, &"y".to_string()), Some("x".to_string()));
        tx.rtx(|(rows, bag)| {
            assert_eq!(rows.get(&4), Some("y".to_string()));
            assert!(!bag.contains(&"x".to_string()));
        });
        assert!(tx.contains(&4));
    });
    assert_eq!(st.get(&4), Some("y".to_string()));

    // a panicking tx rolls back table and graph together
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        st.wtx(|tx| {
            tx.upsert(&5, &"z".to_string());
            panic!("abort");
        });
    }));
    assert!(r.is_err());
    assert!(!st.contains(&5));
    st.rtx(|(_, bag)| assert!(!bag.contains(&"z".to_string())));
}
