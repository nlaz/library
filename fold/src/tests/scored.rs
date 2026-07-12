use crate::{pipeline::*, stream::*};

fn enc<S: Score>(s: S) -> Vec<u8> {
    let mut buf = Vec::new();
    s.encode(&mut buf);
    buf
}

#[test]
fn score_encoding_preserves_order() {
    // varint pitfall cases: postcard would sort 16384 before 300
    let uints: [u64; 6] = [0, 127, 128, 300, 16384, u64::MAX];
    assert!(uints.windows(2).all(|w| enc(w[0]) < enc(w[1])));

    let ints: [i64; 8] = [i64::MIN, -16384, -300, -1, 0, 1, 300, i64::MAX];
    assert!(ints.windows(2).all(|w| enc(w[0]) < enc(w[1])));

    let floats: [f64; 8] = [
        f64::NEG_INFINITY,
        -1e9,
        -1.5,
        -0.0,
        0.0,
        1e-9,
        2.5,
        f64::INFINITY,
    ];
    assert!(floats.windows(2).all(|w| enc(w[0]) < enc(w[1])));

    let pairs: [(u64, i32); 4] = [(1, -5), (1, 3), (2, i32::MIN), (2, 0)];
    assert!(pairs.windows(2).all(|w| enc(w[0]) < enc(w[1])));
}

#[test]
fn score_encoding_roundtrips() {
    for i in [i64::MIN, -300, -1, 0, 1, 300, i64::MAX] {
        let bytes = enc(i);
        let (back, rest) = i64::decode(&bytes);
        assert_eq!(back, i);
        assert!(rest.is_empty());
    }
    for f in [f64::NEG_INFINITY, -1.5, 0.0, 2.5, f64::INFINITY] {
        let bytes = enc(f);
        let (back, rest) = f64::decode(&bytes);
        assert_eq!(back, f);
        assert!(rest.is_empty());
    }
    let bytes = enc((7u64, -9i32));
    let (back, rest) = <(u64, i32)>::decode(&bytes);
    assert_eq!(back, (7, -9));
    assert!(rest.is_empty());
}

use super::fresh_db;

#[test]
fn topk_sliding_window() {
    let mut st = Stream::new(
        fresh_db("fold_topk.db"),
        ScoreBy::new(
            |r: &(u64, String)| r.0,
            TopK::new("recent", 3, Unscore::new(terminal::Bag::new("window"))),
        ),
    );

    let rec = |t: u64| (t, format!("r{t}"));
    macro_rules! window {
        () => {
            st.rtx(|bag| {
                bag.iter()
                    .map(|(r, n): ((u64, String), i64)| (r.0, n))
                    .collect::<Vec<_>>()
            })
        };
    }

    st.wtx(|tx| {
        for t in 1..=5 {
            tx.insert(&rec(t));
        }
    });
    assert_eq!(window!(), vec![(3, 1), (4, 1), (5, 1)]);

    // a new maximum evicts the oldest in-window record
    st.wtx(|tx| tx.insert(&rec(6)));
    assert_eq!(window!(), vec![(4, 1), (5, 1), (6, 1)]);

    // retracting the maximum promotes the runner-up
    st.wtx(|tx| tx.remove(&rec(6)));
    assert_eq!(window!(), vec![(3, 1), (4, 1), (5, 1)]);

    // late data below the boundary leaves the window untouched
    st.wtx(|tx| tx.insert(&rec(2)));
    assert_eq!(window!(), vec![(3, 1), (4, 1), (5, 1)]);

    // multiplicity counts against k: two copies of 5 + one of 4
    st.wtx(|tx| tx.insert(&rec(5)));
    assert_eq!(window!(), vec![(4, 1), (5, 2)]);

    st.wtx(|tx| tx.remove(&rec(5)));
    assert_eq!(window!(), vec![(3, 1), (4, 1), (5, 1)]);

    // drain everything: the window empties without going negative
    st.wtx(|tx| {
        for t in 1..=5 {
            tx.remove(&rec(t));
        }
        tx.remove(&rec(2));
    });
    assert_eq!(window!(), vec![]);
}

#[test]
fn topk_feeds_aggregate() {
    // sum over the 3 most recent readings, maintained incrementally
    let mut st = Stream::new(
        fresh_db("fold_topk_agg.db"),
        ScoreBy::new(
            |r: &(u64, i64)| r.0,
            TopK::new(
                "recent",
                3,
                Unscore::new(KeyBy::new(
                    // single group; postcard encodes `()` to zero bytes, which
                    // the store rejects as a key, so use a u8
                    |_: &(u64, i64)| 0u8,
                    Aggregate::new(
                        "sum",
                        |acc: &mut i64, r: &(u64, i64), d| *acc += r.1 * d as i64,
                        Unkey::new(terminal::Bag::new("sums")),
                    ),
                )),
            ),
        ),
    );

    macro_rules! sum {
        () => {
            st.rtx(|bag| bag.iter().map(|(s, _): (i64, i64)| s).next().unwrap_or(0))
        };
    }

    st.wtx(|tx| {
        tx.insert(&(1, 10));
        tx.insert(&(2, 20));
        tx.insert(&(3, 30));
    });
    assert_eq!(sum!(), 60);

    // t=4 enters the window, t=1 falls out: 20 + 30 + 40
    st.wtx(|tx| tx.insert(&(4, 40)));
    assert_eq!(sum!(), 90);

    // retracting t=4 promotes t=1 back in: 10 + 20 + 30
    st.wtx(|tx| tx.remove(&(4, 40)));
    assert_eq!(sum!(), 60);
}
