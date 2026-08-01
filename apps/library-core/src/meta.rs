//! The metadata store: one SQLite database for everything the user owns
//! that isn't a document's content.
//!
//! Titles, collections, note cards, per-document ingest status and app
//! settings used to be JSON sidecars scattered through the data dir, each
//! rewritten whole on every change and each readable by exactly one writer
//! at a time. Three processes touch that state — the app, the
//! `library-ingest` CLI, and `library-server` — and the fjall stores they
//! also share refuse a second opener, so the sidecars were the only medium
//! all three could use. That made "whole-file rewrite" the concurrency
//! model: two writers meant a lost update, and a crash mid-write meant a
//! torn file (which [`crate::sidecar`] worked around with tmp+rename).
//!
//! SQLite in WAL mode is the honest version of what those files were
//! approximating: multi-process, row-level, atomic, and crash-safe without
//! rewriting a megabyte to flip one boolean. The fjall stores keep the
//! indexes; this keeps the metadata.
//!
//! Readers here stay forgiving in the way the sidecar readers were — a
//! query that fails reads as "nothing" rather than propagating, because
//! every caller's fallback (an untitled doc, an empty shelf) is a better
//! outcome than a failed search. Writers return [`io::Result`] so callers
//! keep the signatures they had.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::notes::{CardLink, CardRec, QuoteAnchor};
use crate::wire::Collections;

/// Bump when `MIGRATIONS` grows. Each entry runs once, in order.
const MIGRATIONS: &[&str] = &[
    include_str!("meta/0001_initial.sql"),
    include_str!("meta/0002_roots.sql"),
];

/// Filename under the app-support dir.
pub const META_DB: &str = "meta.db";

fn sql_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(format!("meta.db: {e}"))
}

fn json_str(s: String) -> serde_json::Value {
    serde_json::Value::String(s)
}

/// A linked folder.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RootRec {
    pub id: String,
    pub path: std::path::PathBuf,
    /// The drop target for files added through the app. Exactly one.
    pub is_default: bool,
    /// `watching` | `unavailable` — the second means we could not read it
    /// last time we looked, which is emphatically not "it is empty".
    pub state: String,
    pub added_at: u64,
    pub last_scan_at: u64,
}

/// A library's two halves: the app-support directory holding the caches
/// (page renders, OCR, markdown, models, `run/`) and the metadata database
/// describing them.
///
/// Functions that need both used to take `data: &Path` and reach for a
/// sidecar; they take a `&Ctx` now. Cheap to clone — the database handle is
/// shared, not reopened.
#[derive(Clone)]
pub struct Ctx {
    pub data: std::path::PathBuf,
    pub meta: std::sync::Arc<Meta>,
}

impl Ctx {
    /// Open the metadata database under `data`, creating it if needed.
    pub fn open(data: impl Into<std::path::PathBuf>) -> io::Result<Ctx> {
        let data = data.into();
        let meta = Meta::open(&data)?;
        Ok(Ctx {
            data,
            meta: std::sync::Arc::new(meta),
        })
    }

    /// A `Ctx` over an existing handle, for hosts that opened it themselves.
    pub fn new(data: impl Into<std::path::PathBuf>, meta: std::sync::Arc<Meta>) -> Ctx {
        Ctx {
            data: data.into(),
            meta,
        }
    }

    /// An in-memory database over a real cache directory, for tests.
    pub fn in_memory(data: impl Into<std::path::PathBuf>) -> io::Result<Ctx> {
        Ok(Ctx {
            data: data.into(),
            meta: std::sync::Arc::new(Meta::open_in_memory()?),
        })
    }
}

impl std::ops::Deref for Ctx {
    type Target = Meta;

    /// So `ctx.titles()` works without spelling out `ctx.meta`. The data
    /// path stays an explicit field — reaching for it should look like
    /// touching the filesystem, because it is.
    fn deref(&self) -> &Meta {
        &self.meta
    }
}

/// A handle on the metadata database.
///
/// Cheap to clone-by-reference and safe to share across threads; the
/// `Mutex` serializes this process's own access, while WAL plus
/// `busy_timeout` handles the other processes.
pub struct Meta {
    conn: Mutex<Connection>,
}

impl Meta {
    /// Open (creating if needed) the database at `dir/meta.db`.
    pub fn open(dir: &Path) -> io::Result<Meta> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join(META_DB)).map_err(sql_err)?;
        Meta::prepare(conn)
    }

    /// An empty in-memory database, for tests.
    pub fn open_in_memory() -> io::Result<Meta> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        Meta::prepare(conn)
    }

    fn prepare(conn: Connection) -> io::Result<Meta> {
        // WAL is what makes a reader (the server, the CLI) coexist with a
        // writer (the app) instead of blocking it. NORMAL sync is the
        // standard WAL pairing: a crash can lose the last transaction, and
        // every table here is either re-derivable or a user edit that was
        // just confirmed on screen.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_err)?;
        // another process mid-commit is expected, not exceptional
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(sql_err)?;
        let meta = Meta {
            conn: Mutex::new(conn),
        };
        meta.migrate()?;
        Ok(meta)
    }

    fn migrate(&self) -> io::Result<()> {
        self.write(|c| {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
            )?;
            let at: i64 = c
                .query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0))
                .optional()?
                .flatten()
                .unwrap_or(0);
            for (i, sql) in MIGRATIONS.iter().enumerate() {
                let v = i as i64 + 1;
                if v > at {
                    c.execute_batch(sql)?;
                    c.execute("INSERT INTO schema_version (version) VALUES (?1)", [v])?;
                }
            }
            Ok(())
        })
    }

    /// Run a closure against the connection, mapping failure to `io::Error`.
    /// Crates outside `library-core` that own their own tables (ingest
    /// status) use this rather than reaching for a second handle.
    pub fn write<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> io::Result<R> {
        let conn = self.conn.lock().expect("meta lock poisoned");
        f(&conn).map_err(sql_err)
    }

    /// Like [`Meta::write`] but for readers that would rather see nothing
    /// than an error — the shape every JSON sidecar reader had.
    ///
    /// Forgiveness is for *data* problems (a corrupt blob, a row a newer
    /// version wrote). A malformed statement is a bug, and returning the
    /// default for one is indistinguishable from an empty library — so
    /// debug builds panic instead, which is what tests and `cargo tauri
    /// dev` run.
    pub fn read<R: Default>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> R {
        let conn = self.conn.lock().expect("meta lock poisoned");
        match f(&conn) {
            Ok(v) => v,
            Err(e) => {
                debug_assert!(false, "meta.db read failed: {e}");
                eprintln!("meta.db read failed, reading as empty: {e}");
                R::default()
            }
        }
    }

    // --- roots --------------------------------------------------------------

    /// Every linked folder, default first then oldest first.
    pub fn roots(&self) -> Vec<RootRec> {
        self.read(|c| {
            let mut q = c.prepare(
                "SELECT id, path, is_default, state, added_at, last_scan_at
                 FROM roots ORDER BY is_default DESC, added_at, rowid",
            )?;
            let rows = q.query_map([], |r| {
                Ok(RootRec {
                    id: r.get(0)?,
                    path: std::path::PathBuf::from(r.get::<_, String>(1)?),
                    is_default: r.get(2)?,
                    state: r.get(3)?,
                    added_at: r.get::<_, i64>(4)? as u64,
                    last_scan_at: r.get::<_, i64>(5)? as u64,
                })
            })?;
            rows.collect()
        })
    }

    /// Link a folder, or return the existing root if it is already linked.
    /// The first root linked becomes the default by construction — a
    /// library with folders but no drop target has nowhere to put a drop.
    pub fn add_root(&self, path: &Path, now: u64) -> io::Result<RootRec> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let text = canonical.to_string_lossy().to_string();
        if let Some(existing) = self.roots().into_iter().find(|r| r.path == canonical) {
            return Ok(existing);
        }
        let id = crate::notes::mint_id('r');
        let first = self.roots().is_empty();
        self.write(|c| {
            c.execute(
                "INSERT INTO roots (id, path, is_default, state, added_at, last_scan_at)
                 VALUES (?1, ?2, ?3, 'watching', ?4, 0)",
                params![id, text, first, now as i64],
            )?;
            Ok(())
        })?;
        Ok(RootRec {
            id,
            path: canonical,
            is_default: first,
            state: "watching".into(),
            added_at: now,
            last_scan_at: 0,
        })
    }

    /// Unlink a folder. The files rows cascade; the *documents* do not —
    /// unlinking routes through the same `missing` path a deletion does,
    /// so the caller retracts them deliberately and the notes survive.
    pub fn remove_root(&self, id: &str) -> io::Result<()> {
        self.write(|c| {
            c.execute("DELETE FROM roots WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// Make one root the drop target, demoting whichever held it.
    pub fn set_default_root(&self, id: &str) -> io::Result<()> {
        self.write(|c| {
            c.execute("UPDATE roots SET is_default = (id = ?1)", [id])?;
            Ok(())
        })
    }

    pub fn set_root_state(&self, id: &str, state: &str, scanned_at: Option<u64>) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "UPDATE roots SET state = ?2,
                   last_scan_at = coalesce(?3, last_scan_at) WHERE id = ?1",
                params![id, state, scanned_at.map(|t| t as i64)],
            )?;
            Ok(())
        })
    }

    /// The drop target for files added through the app.
    pub fn default_root(&self) -> Option<RootRec> {
        self.roots().into_iter().find(|r| r.is_default)
    }

    // --- files --------------------------------------------------------------

    /// Everything we have indexed under one root, as the scanner wants it.
    ///
    /// Missing files are included. They have to be: a file that comes back
    /// must be recognised as the one that left, and a row we hid from the
    /// scanner would read as a brand-new file — minting a second document
    /// and orphaning the first one's page renders and notes.
    pub fn files_in_root(&self, root_id: &str) -> Vec<crate::roots::Known> {
        self.read(|c| {
            let mut q = c.prepare(
                "SELECT doc_id, relpath, inode, size, mtime, content_hash, state
                 FROM files WHERE root_id = ?1 ORDER BY relpath",
            )?;
            let rows = q.query_map([root_id], |r| {
                Ok(crate::roots::Known {
                    doc: r.get(0)?,
                    relpath: r.get(1)?,
                    inode: r.get::<_, i64>(2)? as u64,
                    size: r.get::<_, i64>(3)? as u64,
                    mtime: r.get(4)?,
                    content_hash: r.get(5)?,
                    missing: r.get::<_, String>(6)? == "missing",
                })
            })?;
            rows.collect()
        })
    }

    /// Files under a root that are actually there — what the Settings page
    /// counts, and what "N documents" means to a user.
    pub fn present_files_in_root(&self, root_id: &str) -> usize {
        self.read(|c| {
            c.query_row(
                "SELECT count(*) FROM files WHERE root_id = ?1 AND state != 'missing'",
                [root_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
        })
    }

    /// Record a file and the document it belongs to. Upserts on
    /// `(root, relpath)`, so a rescan of an unchanged file is idempotent.
    #[allow(clippy::too_many_arguments)]
    pub fn put_file(
        &self,
        root_id: &str,
        relpath: &str,
        doc: &str,
        stat: &crate::roots::Stat,
        hash: Option<&str>,
        now: u64,
    ) -> io::Result<()> {
        let id = crate::notes::mint_id('f');
        self.write(|c| {
            c.execute(
                "INSERT INTO files
                   (id, root_id, relpath, inode, size, mtime, content_hash, doc_id,
                    state, first_seen_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?9)
                 ON CONFLICT(root_id, relpath) DO UPDATE SET
                   inode = excluded.inode, size = excluded.size,
                   mtime = excluded.mtime,
                   content_hash = coalesce(excluded.content_hash, files.content_hash),
                   doc_id = excluded.doc_id, state = 'present',
                   last_seen_at = excluded.last_seen_at",
                params![
                    id,
                    root_id,
                    relpath,
                    stat.inode as i64,
                    stat.size as i64,
                    stat.mtime,
                    hash,
                    doc,
                    now as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// Move a file's row to a new path, keeping its document.
    pub fn move_file(
        &self,
        root_id: &str,
        from: &str,
        to: &str,
        stat: &crate::roots::Stat,
        now: u64,
    ) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "UPDATE files SET relpath = ?3, inode = ?4, size = ?5, mtime = ?6,
                                  state = 'present', last_seen_at = ?7
                 WHERE root_id = ?1 AND relpath = ?2",
                params![
                    root_id,
                    from,
                    to,
                    stat.inode as i64,
                    stat.size as i64,
                    stat.mtime,
                    now as i64
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_file_state(&self, root_id: &str, relpath: &str, state: &str) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "UPDATE files SET state = ?3 WHERE root_id = ?1 AND relpath = ?2",
                params![root_id, relpath, state],
            )?;
            Ok(())
        })
    }

    /// The document a set of bytes already belongs to, if any — the dedup
    /// probe for a file that appeared with a fresh inode.
    pub fn doc_with_hash(&self, hash: &str) -> Option<String> {
        self.read(|c| {
            c.query_row(
                "SELECT doc_id FROM files WHERE content_hash = ?1 LIMIT 1",
                [hash],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
    }

    /// Where a document's file lives now: `(root path, relpath)`.
    pub fn doc_path(&self, doc: &str) -> Option<std::path::PathBuf> {
        self.read(|c| {
            c.query_row(
                "SELECT roots.path, files.relpath FROM files
                 JOIN roots ON roots.id = files.root_id
                 WHERE files.doc_id = ?1 AND files.state != 'missing' LIMIT 1",
                [doc],
                |r| {
                    Ok(std::path::PathBuf::from(r.get::<_, String>(0)?)
                        .join(r.get::<_, String>(1)?))
                },
            )
            .optional()
        })
    }

    /// Record what kind of file a document is and which shelf it sits on.
    pub fn set_doc_placement(&self, doc: &str, kind: &str, shelf: Option<&str>) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "INSERT INTO docs (id, kind, shelf) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, shelf = excluded.shelf",
                params![doc, kind, shelf],
            )?;
            Ok(())
        })
    }

    // --- titles -------------------------------------------------------------

    /// `doc id -> display title`, for docs that have one.
    pub fn titles(&self) -> BTreeMap<String, String> {
        self.read(|c| {
            let mut q = c.prepare("SELECT id, title FROM docs WHERE title IS NOT NULL")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect()
        })
    }

    /// Set a display title, or clear it with `None`. The row is created if
    /// this is the first thing we've recorded about the doc.
    pub fn set_title(&self, doc: &str, title: Option<&str>) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "INSERT INTO docs (id, title) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET title = excluded.title",
                params![doc, title],
            )?;
            Ok(())
        })
    }

    // --- collections --------------------------------------------------------

    /// `collection name -> doc ids`, in insertion order within each name.
    pub fn collections(&self) -> Collections {
        self.read(|c| {
            let mut q = c.prepare("SELECT name, doc FROM collections ORDER BY name, ord, rowid")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Collections::new();
            for row in rows {
                let (name, doc) = row?;
                out.entry(name).or_default().push(doc);
            }
            Ok(out)
        })
    }

    /// Add `doc` to `collection`, creating it if needed. Idempotent.
    pub fn collect(&self, collection: &str, doc: &str) -> io::Result<()> {
        self.write(|c| {
            let ord: i64 = c
                .query_row(
                    "SELECT coalesce(max(ord), -1) + 1 FROM collections WHERE name = ?1",
                    [collection],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            c.execute(
                "INSERT OR IGNORE INTO collections (name, doc, ord) VALUES (?1, ?2, ?3)",
                params![collection, doc, ord],
            )?;
            Ok(())
        })
    }

    /// Replace `doc`'s collection membership wholesale. Collections left
    /// empty disappear on their own — the row *is* the membership, so
    /// there is nothing left to prune.
    pub fn set_collections(&self, doc: &str, cols: &[String]) -> io::Result<()> {
        self.write(|c| {
            c.execute("DELETE FROM collections WHERE doc = ?1", [doc])?;
            for name in cols {
                let ord: i64 = c
                    .query_row(
                        "SELECT coalesce(max(ord), -1) + 1 FROM collections WHERE name = ?1",
                        [name],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                c.execute(
                    "INSERT OR IGNORE INTO collections (name, doc, ord) VALUES (?1, ?2, ?3)",
                    params![name, doc, ord],
                )?;
            }
            Ok(())
        })
    }

    // --- cards --------------------------------------------------------------

    /// Every card, oldest first. Order is explicit (`ord`) because the
    /// annotation migration mints cards in reading order and the ledger
    /// shows them in the order they were made, which `created` — a
    /// one-second-resolution stamp — cannot distinguish.
    pub fn cards(&self) -> Vec<CardRec> {
        self.read(|c| {
            let mut q = c.prepare(
                "SELECT id, title, body, created, modified, filed, split_hinted, evidence, links
                 FROM cards ORDER BY ord, rowid",
            )?;
            let rows = q.query_map([], |r| {
                let evidence: String = r.get(7)?;
                let links: String = r.get(8)?;
                Ok(CardRec {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    body: r.get(2)?,
                    created: r.get::<_, i64>(3)? as u64,
                    modified: r.get::<_, i64>(4)? as u64,
                    filed: r.get(5)?,
                    split_hinted: r.get(6)?,
                    // the anchor enum is serde-tagged and versioned by its
                    // own shape; a column per variant would freeze it
                    evidence: serde_json::from_str::<Vec<QuoteAnchor>>(&evidence)
                        .unwrap_or_default(),
                    links: serde_json::from_str::<Vec<CardLink>>(&links).unwrap_or_default(),
                })
            })?;
            rows.collect()
        })
    }

    /// One card by id.
    pub fn card(&self, id: &str) -> Option<CardRec> {
        self.cards().into_iter().find(|c| c.id == id)
    }

    /// Insert or replace one card. Unlike the sidecar this rewrites one
    /// row, so two surfaces editing different cards can no longer clobber
    /// each other.
    pub fn put_card(&self, card: &CardRec) -> io::Result<()> {
        let evidence = serde_json::to_string(&card.evidence).map_err(io::Error::other)?;
        let links = serde_json::to_string(&card.links).map_err(io::Error::other)?;
        self.write(|c| {
            let ord: i64 = c
                .query_row("SELECT coalesce(max(ord), -1) + 1 FROM cards", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            c.execute(
                "INSERT INTO cards
                   (id, title, body, created, modified, filed, split_hinted, evidence, links, ord)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET
                   title = excluded.title, body = excluded.body,
                   modified = excluded.modified, filed = excluded.filed,
                   split_hinted = excluded.split_hinted,
                   evidence = excluded.evidence, links = excluded.links",
                params![
                    card.id,
                    card.title,
                    card.body,
                    card.created as i64,
                    card.modified as i64,
                    card.filed,
                    card.split_hinted,
                    evidence,
                    links,
                    ord,
                ],
            )?;
            Ok(())
        })
    }

    /// Replace the whole card set, preserving the given order. Used by the
    /// migration importer; ordinary edits go through [`Meta::put_card`].
    pub fn put_cards(&self, cards: &[CardRec]) -> io::Result<()> {
        self.write(|c| {
            c.execute("DELETE FROM cards", [])?;
            Ok(())
        })?;
        for card in cards {
            self.put_card(card)?;
        }
        Ok(())
    }

    // --- doc status (as JSON) -----------------------------------------------

    /// The ingest-status columns shaped as JSON, for the surfaces that pass
    /// status straight through to a client (the server's `/api/doc`, the
    /// perf view's rows).
    ///
    /// The typed owner of these columns is `library_ingest::status`, which
    /// this crate can't depend on — `status_json_matches_docstatus` over
    /// there pins the two representations together.
    pub fn doc_status_json(&self, doc: &str) -> serde_json::Value {
        self.doc_status_rows()
            .remove(doc)
            .unwrap_or(serde_json::Value::Null)
    }

    /// Every doc with an ingest state, as JSON. One query.
    pub fn doc_status_rows(&self) -> BTreeMap<String, serde_json::Value> {
        self.read(|c| {
            let mut q = c.prepare(
                "SELECT id, state, stage, done, total, error, metrics, updated_at
                 FROM docs WHERE state IS NOT NULL",
            )?;
            let rows = q.query_map([], |r| {
                let mut v = serde_json::Map::new();
                v.insert("state".into(), json_str(r.get::<_, String>(1)?));
                if let Some(stage) = r.get::<_, Option<String>>(2)? {
                    v.insert("stage".into(), json_str(stage));
                }
                v.insert("done".into(), r.get::<_, i64>(3)?.into());
                v.insert("total".into(), r.get::<_, i64>(4)?.into());
                if let Some(err) = r.get::<_, Option<String>>(5)? {
                    v.insert("error".into(), json_str(err));
                }
                if let Some(m) = r.get::<_, Option<String>>(6)?
                    && let Ok(m) = serde_json::from_str(&m)
                {
                    v.insert("metrics".into(), m);
                }
                v.insert("updated".into(), r.get::<_, i64>(7)?.into());
                Ok((r.get::<_, String>(0)?, serde_json::Value::Object(v)))
            })?;
            rows.collect()
        })
    }

    // --- settings -----------------------------------------------------------

    pub fn setting(&self, key: &str) -> Option<String> {
        self.read(|c| {
            c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{AnchorKind, LinkKind};

    fn card(id: &str, title: &str) -> CardRec {
        CardRec {
            id: id.to_string(),
            title: title.to_string(),
            body: String::new(),
            evidence: vec![],
            links: vec![],
            created: 100,
            modified: 100,
            filed: false,
            split_hinted: false,
        }
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = std::env::temp_dir().join(format!("meta-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let m = Meta::open(&dir).unwrap();
            m.set_title("kant", Some("Critique")).unwrap();
        }
        // reopening runs migrate() again against a populated db
        let m = Meta::open(&dir).unwrap();
        assert_eq!(m.titles().get("kant").map(String::as_str), Some("Critique"));
        let applied: i64 = m
            .write(|c| c.query_row("SELECT count(*) FROM schema_version", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn titles_set_and_clear() {
        let m = Meta::open_in_memory().unwrap();
        assert!(m.titles().is_empty());

        m.set_title("kant", Some("Critique of Pure Reason"))
            .unwrap();
        m.set_title("hume", Some("Enquiry")).unwrap();
        assert_eq!(m.titles().len(), 2);

        // clearing removes it from the map but keeps the doc row
        m.set_title("hume", None).unwrap();
        let t = m.titles();
        assert_eq!(t.len(), 1);
        assert!(t.contains_key("kant"));
    }

    #[test]
    fn collections_round_trip_and_prune_themselves() {
        let m = Meta::open_in_memory().unwrap();
        m.collect("cookbooks", "artusi").unwrap();
        m.collect("cookbooks", "escoffier").unwrap();
        m.collect("software", "sicp").unwrap();

        let cols = m.collections();
        assert_eq!(cols["cookbooks"], vec!["artusi", "escoffier"]);
        assert_eq!(cols["software"], vec!["sicp"]);

        // idempotent: adding twice doesn't duplicate
        m.collect("cookbooks", "artusi").unwrap();
        assert_eq!(m.collections()["cookbooks"].len(), 2);

        // wholesale replace moves a doc and empties the shelf it left
        m.set_collections("sicp", &["cookbooks".to_string()])
            .unwrap();
        let cols = m.collections();
        assert!(!cols.contains_key("software"), "empty shelf must vanish");
        assert_eq!(cols["cookbooks"].len(), 3);

        // clearing removes it everywhere
        m.set_collections("artusi", &[]).unwrap();
        assert_eq!(m.collections()["cookbooks"], vec!["escoffier", "sicp"]);
    }

    #[test]
    fn cards_round_trip_with_evidence_and_links() {
        let m = Meta::open_in_memory().unwrap();
        let mut a = card("c1", "first claim");
        a.evidence = vec![QuoteAnchor {
            doc: "artusi".into(),
            page: 12,
            kind: AnchorKind::Text {
                w0: 3,
                w1: 9,
                text: "sfoglia".into(),
                boxes: vec![[0.1, 0.2, 0.3, 0.04]],
            },
        }];
        a.links = vec![CardLink {
            to: "c2".into(),
            kind: LinkKind::Continues,
        }];
        m.put_card(&a).unwrap();
        m.put_card(&card("c2", "second claim")).unwrap();

        let back = m.cards();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], a, "evidence and links survive the round trip");
        assert_eq!(back[1].id, "c2");
    }

    #[test]
    fn put_card_updates_in_place_and_keeps_order() {
        let m = Meta::open_in_memory().unwrap();
        m.put_card(&card("c1", "one")).unwrap();
        m.put_card(&card("c2", "two")).unwrap();

        let mut edited = card("c1", "one, revised");
        edited.modified = 500;
        edited.filed = true;
        m.put_card(&edited).unwrap();

        let back = m.cards();
        assert_eq!(back.len(), 2, "update must not insert a duplicate");
        assert_eq!(back[0].title, "one, revised");
        assert_eq!(back[0].created, 100, "created is immutable");
        assert_eq!(back[0].modified, 500);
        assert!(back[0].filed);
        assert_eq!(back[1].id, "c2", "order is preserved across an update");
    }

    #[test]
    fn put_cards_replaces_the_set_in_the_given_order() {
        let m = Meta::open_in_memory().unwrap();
        m.put_card(&card("old", "gone")).unwrap();
        m.put_cards(&[card("c3", "third"), card("c1", "first")])
            .unwrap();

        let ids: Vec<String> = m.cards().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["c3", "c1"], "import order, not id order");
    }

    #[test]
    fn settings_round_trip() {
        let m = Meta::open_in_memory().unwrap();
        assert_eq!(m.setting("width"), None);
        m.set_setting("width", "1600").unwrap();
        m.set_setting("width", "1200").unwrap();
        assert_eq!(m.setting("width").as_deref(), Some("1200"));
    }

    #[test]
    fn a_corrupt_evidence_blob_reads_as_no_evidence() {
        // forgiving in the way the sidecar readers were: one bad row must
        // not take out the ledger
        let m = Meta::open_in_memory().unwrap();
        m.put_card(&card("c1", "claim")).unwrap();
        m.write(|c| {
            c.execute(
                "UPDATE cards SET evidence = '{not json' WHERE id = 'c1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let back = m.cards();
        assert_eq!(back.len(), 1);
        assert!(back[0].evidence.is_empty());
    }
}
