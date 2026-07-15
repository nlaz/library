//! The smallest possible fold database: a persistent count of entries,
//! plus a bag so you can see what's inside.
//!
//! A fold `Stream` takes inserts and removes, pushes them through a
//! pipeline, and keeps every materialized view (here: a count and a bag)
//! durable and consistent — no rebuilds, ever.

use fold::pipeline::terminal;
use fold::stream::Stream;

fn main() {
    // reopening the same path with the same pipeline resumes prior state,
    // so a fresh temp dir keeps this demo deterministic
    let db_path = std::env::temp_dir().join("the-library-starter.db");
    let _ = std::fs::remove_dir_all(&db_path);

    // the pipeline: fan every entry out to two views.
    // tuples broadcast, terminal nodes persist.
    let mut st = Stream::new(
        &db_path,
        (
            terminal::Count::new("total"),
            terminal::Bag::<String>::new("entries"),
        ),
    );

    // writes are transactional: everything in one wtx commits atomically
    st.wtx(|tx| {
        tx.insert(&"peat".to_string());
        tx.insert(&"moss".to_string());
        tx.insert(&"moss".to_string()); // bags count duplicates
    });

    // reads see one consistent snapshot across all views
    st.rtx(|(count, entries)| {
        println!("total entries: {}", count.get());
        for (entry, multiplicity) in entries.iter() {
            let entry: String = entry;
            println!("  {entry} x{multiplicity}");
        }
    });

    // removing retracts the entry from every view
    st.wtx(|tx| tx.remove(&"peat".to_string()));

    st.rtx(|(count, entries)| {
        println!("after removing peat: {} entries", count.get());
        assert!(!entries.contains(&"peat".to_string()));
    });

    println!(
        "state lives in {} — rerun with the cleanup removed to see persistence",
        db_path.display()
    );
}
