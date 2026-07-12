use crate::{pipeline::*, stream::*, tests::fresh_db};
use std::ops::Bound;

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn table_materializes_aggregates() {
    let mut st = Stream::new(
        fresh_db("table.db"),
        KeyBy::new(
            |(k, _): &(u32, i64)| *k,
            Aggregate::new(
                "table_sums",
                |acc: &mut i64, (_, v): &(u32, i64), d| *acc += v * d as i64,
                terminal::Table::new("table"),
            ),
        ),
    );

    st.wtx(|tx| {
        tx.insert(&(1, 10));
        tx.insert(&(1, 5));
        tx.insert(&(2, 7));
    });
    st.rtx(|c| {
        assert_eq!(c.get(&1), Some(15));
        assert_eq!(c.get(&2), Some(7));
        assert_eq!(c.get(&3), None);
        assert_eq!(c.iter().count(), 2);
    });

    // retraction re-aggregates and overwrites the register
    st.wtx(|tx| tx.remove(&(1, 5)));
    st.rtx(|c| assert_eq!(c.get(&1), Some(10)));

    // a key's last record leaving deletes its register
    st.wtx(|tx| {
        tx.remove(&(1, 10));
        tx.remove(&(2, 7));
    });
    st.rtx(|c| {
        assert!(!c.contains(&1));
        assert_eq!(c.iter().count(), 0);
    });
}

#[test]
fn ranked_orders_scores() {
    let mut st = Stream::new(fresh_db("ranked.db"), terminal::Ranked::new("ranked"));

    let rec = |s: u64, v: &str| Scored::new(s, v.to_string());
    st.wtx(|tx| {
        tx.insert(&rec(1, "a"));
        tx.insert(&rec(2, "b"));
        tx.insert(&rec(2, "b")); // multiplicity 2
        tx.insert(&rec(3, "c"));
    });

    st.rtx(|r| {
        assert_eq!(r.min().unwrap().val, "a");
        assert_eq!(r.max().unwrap().val, "c");

        let top: Vec<_> = r.top(2).into_iter().map(|s| s.val).collect();
        assert_eq!(top, vec!["c", "b"]);
        assert_eq!(r.top(10).len(), 4); // copies count
        assert_eq!(r.bottom(1)[0].val, "a");
        assert!(r.top(0).is_empty());

        let in_range: Vec<_> = r.range(2..).map(|(s, n)| (s.val, n)).collect();
        assert_eq!(in_range, vec![("b".to_string(), 2), ("c".to_string(), 1)]);
        assert_eq!(r.range(..2).count(), 1);
        assert_eq!(r.range((Bound::Excluded(2), Bound::Unbounded)).count(), 1);
        let desc: Vec<_> = r.range(1..=2).rev().map(|(s, _)| s.score).collect();
        assert_eq!(desc, vec![2, 1]);
    });

    // retracting the max reveals the runner-up
    st.wtx(|tx| tx.remove(&rec(3, "c")));
    st.rtx(|r| assert_eq!(r.max().unwrap().val, "b"));

    st.wtx(|tx| {
        tx.remove(&rec(1, "a"));
        tx.remove(&rec(2, "b"));
        tx.remove(&rec(2, "b"));
    });
    st.rtx(|r| {
        assert!(r.min().is_none());
        assert_eq!(r.iter().count(), 0);
    });
}

#[test]
fn keyed_ranked_per_key_extremes() {
    let mut st = Stream::new(
        fresh_db("keyed_ranked.db"),
        terminal::KeyedRanked::new("kr"),
    );

    let rec = |k: u32, s: u64, v: &str| Keyed::new(k, Scored::new(s, v.to_string()));
    st.wtx(|tx| {
        tx.insert(&rec(1, 5, "a"));
        tx.insert(&rec(1, 9, "b"));
        tx.insert(&rec(2, 3, "c"));
    });

    st.rtx(|r| {
        assert_eq!(r.min(&1).unwrap().val, "a");
        assert_eq!(r.max(&1).unwrap().val, "b");
        assert_eq!(r.max(&2).unwrap().val, "c");
        assert!(r.min(&3).is_none());

        let top: Vec<_> = r.top(&1, 5).into_iter().map(|s| s.val).collect();
        assert_eq!(top, vec!["b", "a"]);
        assert_eq!(r.range(&1, 6..).count(), 1);
        assert_eq!(r.iter(&2).count(), 1);
    });

    // per-key retraction reveals the runner-up, other keys untouched
    st.wtx(|tx| tx.remove(&rec(1, 9, "b")));
    st.rtx(|r| {
        assert_eq!(r.max(&1).unwrap().val, "a");
        assert_eq!(r.max(&2).unwrap().val, "c");
    });
}

#[test]
fn histogram_quantiles() {
    let mut st = Stream::new(
        fresh_db("histogram.db"),
        terminal::Histogram::new("hist", |s: &u64| s / 10),
    );

    st.wtx(|tx| {
        for s in [5u64, 15, 25, 25] {
            tx.insert(&Scored::new(s, ()));
        }
    });
    st.rtx(|h| {
        assert_eq!(h.total(), 4);
        assert_eq!(h.count(&2), 2);
        assert_eq!(h.iter().count(), 3);
        assert_eq!(h.quantile(0.0), Some(0));
        assert_eq!(h.quantile(0.5), Some(1));
        assert_eq!(h.quantile(1.0), Some(2));
    });

    st.wtx(|tx| {
        tx.remove(&Scored::new(25u64, ()));
        tx.remove(&Scored::new(25u64, ()));
    });
    st.rtx(|h| {
        assert_eq!(h.total(), 2);
        assert_eq!(h.count(&2), 0);
        assert_eq!(h.quantile(1.0), Some(1));
    });

    st.wtx(|tx| {
        tx.remove(&Scored::new(5u64, ()));
        tx.remove(&Scored::new(15u64, ()));
    });
    st.rtx(|h| {
        assert_eq!(h.total(), 0);
        assert_eq!(h.quantile(0.5), None);
    });
}

#[test]
fn stats_moments() {
    let mut st = Stream::new(
        fresh_db("stats.db"),
        terminal::Stats::new("stats", |v: &f64| *v),
    );

    st.wtx(|tx| {
        tx.insert(&1.0);
        tx.insert(&2.0);
        tx.insert(&3.0);
    });
    st.rtx(|s| {
        assert_eq!(s.count(), 3);
        assert!(approx(s.sum(), 6.0));
        assert!(approx(s.mean().unwrap(), 2.0));
        assert!(approx(s.variance().unwrap(), 2.0 / 3.0));
    });

    st.wtx(|tx| tx.remove(&2.0));
    st.rtx(|s| {
        assert_eq!(s.count(), 2);
        assert!(approx(s.mean().unwrap(), 2.0));
        assert!(approx(s.variance().unwrap(), 1.0));
        assert!(approx(s.stddev().unwrap(), 1.0));
    });

    st.wtx(|tx| {
        tx.remove(&1.0);
        tx.remove(&3.0);
    });
    st.rtx(|s| {
        assert_eq!(s.count(), 0);
        assert!(s.mean().is_none());
    });
}

#[test]
fn multimap_forward_index() {
    let mut st = Stream::new(fresh_db("multimap.db"), terminal::Multimap::new("mm"));

    let rec = |k: u32, v: &str| Keyed::new(k, v.to_string());
    st.wtx(|tx| {
        tx.insert(&rec(1, "a"));
        tx.insert(&rec(1, "b"));
        tx.insert(&rec(2, "c"));
    });
    st.rtx(|mm| {
        assert_eq!(mm.get(&1), vec!["a", "b"]);
        assert_eq!(mm.get(&2), vec!["c"]);
        assert!(mm.get(&3).is_empty());
    });

    st.wtx(|tx| tx.remove(&rec(1, "a")));
    st.rtx(|mm| assert_eq!(mm.get(&1), vec!["b"]));
}
