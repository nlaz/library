use crate::{pipeline::*, stream::*};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
struct Document {
    id: usize,
    title: String,
    body: String,
    label: u32,
}

fn parse_samples(data: &str) -> Vec<Document> {
    let mut lines = data.lines();
    let mut samples = Vec::new();
    let mut i = 0;
    while let Some(title) = lines.next() {
        let body = lines.next().expect("missing description").to_string();
        let label = lines
            .next()
            .expect("missing label")
            .parse()
            .expect("invalid label");
        samples.push(Document {
            id: i,
            title: title.to_string(),
            body,
            label,
        });
        i += 1;
    }
    samples
}

#[test]
fn agnews_bm25() {
    let data = include_str!("testdata/agnews.txt");
    let samples = parse_samples(data);
    let iters = samples.len();
    let total_bytes: usize = samples.iter().map(|d| d.body.len()).sum();

    let open_start = Instant::now();
    let mut st = Stream::new(
        "agnews.db",
        Map::new(
            |d: &Document| Keyed::new(d.id, d.body.clone()),
            terminal::search::Bm25::new("agnews_bm25"),
        ),
    );
    let open_dur = open_start.elapsed();

    let start = Instant::now();
    for chunk in samples.chunks(5_000) {
        st.wtx(|tx| {
            for doc in chunk {
                tx.insert(doc);
            }
        });
    }
    let ingest = start.elapsed();

    let queries = [
        "the",
        "government shutdown",
        "microsoft windows security",
        "oil prices rise",
        "zzzzzzzzzzzzzz",
        "olympic gold medal",
        "china trade",
    ];
    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), iters as i64);
        for q in queries {
            let start = Instant::now();
            let hits = idx.search(q, 10);
            let dur = start.elapsed();
            assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
            let top: Vec<_> = hits
                .iter()
                .take(3)
                .map(|h| (h.val, (h.score * 100.0).round() / 100.0))
                .collect();
            println!("search {q:?}: {} hits in {dur:?}, top {top:?}", hits.len());
            if q != "zzzzzzzzzzzzzz" {
                assert!(!hits.is_empty());
            } else {
                assert!(hits.is_empty());
            }
        }
    });

    let start = Instant::now();
    for chunk in samples.chunks(5_000) {
        st.wtx(|tx| {
            for doc in chunk {
                tx.remove(doc);
            }
        });
    }
    let del = start.elapsed();

    st.rtx(|idx| {
        assert_eq!(idx.doc_count(), 0);
        for q in queries {
            assert!(idx.search(q, 10).is_empty());
        }
    });

    println!("open/recover: {open_dur:?}");
    println!(
        "ingest: {iters} docs ({:.1} MB) in {ingest:?} (avg {:?}/doc, {:.1} MB/s)",
        total_bytes as f64 / 1e6,
        ingest.div_f64(iters as f64),
        total_bytes as f64 / 1e6 / ingest.as_secs_f64()
    );
    println!(
        "delete: {iters} docs in {del:?} (avg {:?}/doc)",
        del.div_f64(iters as f64)
    );
}
