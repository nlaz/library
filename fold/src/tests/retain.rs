use std::{cell::Cell, rc::Rc, time::Duration};

use super::fresh_db;
use crate::{pipeline::*, stream::*};

#[test]
fn retain_expires_by_horizon() {
    let clock = Rc::new(Cell::new(1_000u64));
    let c = clock.clone();
    let mut st = Stream::new(
        fresh_db("fold_retain.db"),
        Retain::with_clock(
            "recent",
            Duration::from_millis(100),
            move || c.get(),
            (terminal::Count::new("n"), terminal::Bag::new("bag")),
        ),
    );

    st.wtx(|tx| {
        tx.insert(&"a".to_string());
        tx.insert(&"b".to_string());
    });
    st.rtx(|(n, _)| assert_eq!(n.get(), 2));

    clock.set(1_050);
    st.wtx(|tx| tx.insert(&"c".to_string()));
    st.rtx(|(n, _)| assert_eq!(n.get(), 3));

    // a and b age past the horizon; an empty wtx is enough to expire them
    clock.set(1_150);
    st.wtx(|_| {});
    st.rtx(|(n, bag)| {
        assert_eq!(n.get(), 1);
        assert!(bag.contains(&"c".to_string()));
        assert!(!bag.contains(&"a".to_string()));
    });

    clock.set(1_200);
    st.wtx(|_| {});
    st.rtx(|(n, bag)| {
        assert_eq!(n.get(), 0);
        assert_eq!(bag.iter().count(), 0);
    });
}

#[test]
fn retain_retraction_interplay() {
    let clock = Rc::new(Cell::new(1_000u64));
    let c = clock.clone();
    let mut st = Stream::new(
        fresh_db("fold_retain_retract.db"),
        Retain::with_clock(
            "recent",
            Duration::from_millis(100),
            move || c.get(),
            terminal::Count::new("n"),
        ),
    );

    let x = "x".to_string();
    st.wtx(|tx| tx.push(&x, 3));
    st.rtx(|n| assert_eq!(n.get(), 3));

    // upstream retraction cancels one buffered copy
    clock.set(1_020);
    st.wtx(|tx| tx.remove(&x));
    st.rtx(|n| assert_eq!(n.get(), 2));

    // the remaining copies expire
    clock.set(1_200);
    st.wtx(|_| {});
    st.rtx(|n| assert_eq!(n.get(), 0));

    // retracting an already-expired record is absorbed, not forwarded:
    // expiry has already retracted it downstream
    st.wtx(|tx| tx.remove(&x));
    st.rtx(|n| assert_eq!(n.get(), 0));

    // and the buffer is empty, so a fresh insert behaves normally
    st.wtx(|tx| tx.insert(&x));
    st.rtx(|n| assert_eq!(n.get(), 1));
}

#[test]
fn retain_survives_reopen() {
    let path = fresh_db("fold_retain_reopen.db");
    let clock = Rc::new(Cell::new(1_000u64));
    let pipeline = |c: Rc<Cell<u64>>| {
        Retain::with_clock(
            "recent",
            Duration::from_millis(100),
            move || c.get(),
            terminal::Count::new("n"),
        )
    };

    {
        let mut st = Stream::new(&path, pipeline(clock.clone()));
        st.wtx(|tx| tx.insert(&7u32));
    }

    clock.set(1_050);
    let mut st = Stream::new(&path, pipeline(clock.clone()));
    st.rtx(|n| assert_eq!(n.get(), 1));
    // new inserts continue the arrival sequence past the persisted buffer
    st.wtx(|tx| tx.insert(&8u32));
    st.rtx(|n| assert_eq!(n.get(), 2));

    // the pre-reopen record expires on schedule, the newer one survives
    clock.set(1_120);
    st.wtx(|_| {});
    st.rtx(|n| assert_eq!(n.get(), 1));
}
