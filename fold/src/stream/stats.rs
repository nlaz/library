//! Memory and disk statistics for a stream's backing store.
//!
//! Fjall keeps real RAM outside any sink's view: the block cache, the
//! active and sealed memtables, and bloom filters / index blocks pinned
//! per keyspace. [`DbStats`] surfaces those numbers (plus disk usage) so
//! hosts can account for the store in a memory breakdown without fold
//! exposing the database handle itself.

use serde::Serialize;

/// Store-wide snapshot: cache and memtable RAM, journal/disk footprint,
/// and one [`KeyspaceStats`] per keyspace — including keyspaces no live
/// sink opened, so orphans left behind by older pipeline shapes show up.
#[derive(Debug, Clone, Serialize)]
pub struct DbStats {
    pub disk_bytes: u64,
    pub journal_count: usize,
    /// Active + sealed memtable bytes (uncapped under default config).
    pub write_buffer_bytes: u64,
    pub block_cache_capacity: u64,
    pub block_cache_bytes: u64,
    pub keyspaces: Vec<KeyspaceStats>,
}

/// Per-keyspace footprint. The pinned filter/index bytes are resident RAM
/// held *outside* the block cache — easy to miss and significant on large
/// trees.
#[derive(Debug, Clone, Serialize)]
pub struct KeyspaceStats {
    pub name: String,
    pub disk_bytes: u64,
    /// Item count including tombstones (cheap, unlike an exact len).
    pub approx_len: usize,
    pub sealed_memtables: usize,
    pub pinned_filter_bytes: u64,
    pub pinned_index_bytes: u64,
}

pub(crate) fn db_stats(db: &fjall::SingleWriterTxDatabase) -> DbStats {
    use fjall::AbstractTree;
    let keyspaces = db
        .list_keyspace_names()
        .into_iter()
        .filter_map(|name| {
            let ks = db
                .keyspace(&name, fjall::KeyspaceCreateOptions::default)
                .ok()?;
            let inner = ks.inner();
            Some(KeyspaceStats {
                name: name.to_string(),
                disk_bytes: inner.disk_space(),
                approx_len: ks.approximate_len(),
                sealed_memtables: inner.sealed_memtable_count(),
                pinned_filter_bytes: inner.tree.pinned_filter_size() as u64,
                pinned_index_bytes: inner.tree.pinned_block_index_size() as u64,
            })
        })
        .collect();
    DbStats {
        disk_bytes: db.disk_space().unwrap_or(0),
        journal_count: db.journal_count(),
        write_buffer_bytes: db.write_buffer_size(),
        block_cache_capacity: db.inner().cache_capacity(),
        block_cache_bytes: db.inner().cache_size(),
        keyspaces,
    }
}
