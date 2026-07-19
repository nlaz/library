//! Text search over a fold database: BM25 keyword search and HNSW semantic
//! search, maintained incrementally from one stream of documents.
//!
//! The stream is a `KeyedStream`: a primary-key table (id -> text) fronting
//! the pipeline. `upsert` retracts a key's old record before inserting the
//! new one and `remove` retracts by key alone — so editing or forgetting a
//! memory updates every index automatically, and callers never reproduce a
//! record to delete it.
//!
//! Each `Keyed { key: id, val: text }` delta fans out to three views:
//!
//!   - a BM25 full-text index (`terminal::search::Bm25`)
//!   - an HNSW vector index over ese embeddings (`terminal::search::Hnsw`)
//!   - a doc table for id -> text lookup (`terminal::Table`)
//!
//! The embedding is computed *inside* the pipeline by a `Map`, so a single
//! upsert (re)indexes the document everywhere and a retraction removes it
//! everywhere — including genuinely deleting the node from the HNSW graph
//! (no tombstones, recall doesn't decay under churn).
//!
//! This is a good skeleton for agent memory: upsert new or corrected facts,
//! remove stale ones, hybrid search to recall. All durable, all
//! transactional.
//!
//! Run `cargo run -p search` for a scripted demo; it drops into an
//! interactive loop afterwards if you're on a terminal.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};

use anny::metric::Cosine;
use fold::pipeline::{Keyed, Map, Scored, terminal};
use fold::stream::KeyedStream;

const DIM: usize = ese::DIMENSIONS;

/// Reciprocal-rank-fusion constant: dampens the head so one list can't
/// dominate. 60 is the value from the original RRF paper.
const RRF_K: f64 = 60.0;

// The pipeline type contains closures and so can't be written down, which
// means helpers that read the stream can't be ordinary functions taking
// `&KeyedStream<...>` — they're macros instead, expanded where the
// concrete type is known.

/// Search all three ways and print the top hits.
macro_rules! demo {
    ($st:expr, $title:expr, $query:expr) => {{
        let (title, query) = ($title, $query);
        println!("== {title}: {query:?} ==");
        $st.rtx(|(bm25, vecs, docs)| {
            let get = |id: u64| docs.get(&id).unwrap_or_default();

            println!("  bm25:");
            for hit in bm25.search(query, 3) {
                println!("    {:>6.3}  [{}] {}", hit.score, hit.val, get(hit.val));
            }

            println!("  hnsw (cosine distance, smaller = closer):");
            for hit in vecs.search(&ese::encode_single(query)).into_iter().take(3) {
                println!("    {:>6.3}  [{}] {}", hit.score, hit.val, get(hit.val));
            }

            println!("  hybrid:");
            let fused = hybrid(
                &bm25.search(query, 10),
                &vecs.search(&ese::encode_single(query)),
            );
            for (id, score) in fused {
                println!("    {score:>6.3}  [{id}] {}", get(id));
            }
        });
        println!();
    }};
}

fn main() {
    let db_path = std::env::temp_dir().join("the-library-search.db");
    let _ = std::fs::remove_dir_all(&db_path);

    // KeyedStream pushes Keyed<u64, String> deltas through the pipeline;
    // Bm25 and Table consume them directly, the Hnsw branch maps the text
    // to its embedding first
    let mut st = KeyedStream::new(
        &db_path,
        (
            // keyword: tokenized text, ranked by BM25 relevance
            terminal::search::Bm25::new("bm25"),
            // semantic: ese embeds the text right here in the pipeline.
            // ese is a pure function of the text, which is exactly what
            // fold requires for retraction to cancel cleanly.
            Map::new(
                |d: &Keyed<u64, String>| Keyed::new(d.key, ese::encode_single(&d.val)),
                terminal::search::Hnsw::<u64, f32, Cosine, DIM>::new("vecs", Cosine, 42),
            ),
            // id -> text, for showing hits and scanning for the next free id
            terminal::Table::new("docs"),
        ),
    );

    let memories = seed_memories();
    let count = memories.len();
    st.wtx(|tx| {
        for (id, text) in memories.into_iter().enumerate() {
            tx.upsert(&(id as u64), &text.to_string());
        }
    });

    println!("indexed {count} memories\n");

    demo!(st, "keyword search (bm25)", "kubernetes deploy");
    demo!(st, "semantic search (hnsw over ese)", "user was unhappy");
    demo!(
        st,
        "hybrid (reciprocal rank fusion)",
        "database performance"
    );

    // correcting a memory: upsert retracts the old text from every index
    // and indexes the new one, atomically
    println!("== updating memory 6 (staging moved off the pi) ==\n");
    st.wtx(|tx| {
        tx.upsert(
            &6,
            &"the staging environment moved to a cloud vm in april".to_string(),
        )
    });
    demo!(st, "search after the update", "raspberry pi");

    // forgetting: remove by key — no need to reproduce the record
    println!("== forgetting memory 2 ==\n");
    st.wtx(|tx| tx.remove(&2));
    demo!(st, "hybrid query after forgetting", "database performance");

    // a tiny interactive loop: the bones of an agent memory tool
    if std::io::stdin().is_terminal() {
        println!(
            "interactive: <query> searches, `add <text>` remembers, `rm <id>` forgets, ctrl-d quits"
        );
        let stdin = std::io::stdin();
        loop {
            print!("> ");
            std::io::stdout().flush().unwrap();
            let Some(Ok(line)) = stdin.lock().lines().next() else {
                return;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(text) = line.strip_prefix("add ") {
                let id =
                    st.rtx(|(_, _, docs)| docs.iter().map(|(id, _)| id).max().map_or(0, |m| m + 1));
                st.wtx(|tx| tx.upsert(&id, &text.to_string()));
                println!("remembered as [{id}]");
            } else if let Some(id) = line.strip_prefix("rm ") {
                match id.trim().parse::<u64>() {
                    Ok(id) => match st.wtx(|tx| tx.remove(&id)) {
                        Some(_) => println!("forgot [{id}]"),
                        None => println!("no memory with id {id}"),
                    },
                    Err(_) => println!("usage: rm <numeric id>"),
                }
            } else {
                demo!(st, "hybrid", line);
            }
        }
    }
}

/// Fuse a BM25 hit list and an HNSW hit list by reciprocal rank. Rank-based
/// fusion sidesteps the fact that the two scores live on incomparable
/// scales (BM25 relevance vs cosine distance).
fn hybrid(keyword: &[Scored<f64, u64>], semantic: &[Scored<f32, u64>]) -> Vec<(u64, f64)> {
    let mut fused: HashMap<u64, f64> = HashMap::new();
    for (rank, hit) in keyword.iter().enumerate() {
        *fused.entry(hit.val).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, hit) in semantic.iter().enumerate() {
        *fused.entry(hit.val).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    let mut fused: Vec<(u64, f64)> = fused.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1));
    fused.truncate(3);
    fused
}

fn seed_memories() -> Vec<&'static str> {
    vec![
        "the user prefers rust over python for backend services",
        "deployed the api service to the kubernetes cluster on friday",
        "the postgres database was slow because the orders table was missing an index",
        "the user's name is sam and they are hosting a hackathon this weekend",
        "customer complained that the dashboard takes ten seconds to load",
        "switched the cache from redis to an in-process lru and cut latency in half",
        "the staging environment runs on a raspberry pi under the desk",
        "user asked to always write commit messages in the imperative mood",
        "the search feature should rank recent documents higher",
        "billing bug: invoices double-counted when a plan changed mid-month",
        "the team decided to adopt fold for all incremental state at the offsite",
        "meeting notes: ship the embedding search demo before the conference",
    ]
}
