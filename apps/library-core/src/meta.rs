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
    include_str!("meta/0003_page_cache.sql"),
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

    /// Open `dir/meta.db` read-only, without creating it and **without
    /// migrating it**.
    ///
    /// [`open`](Self::open) runs [`migrate`](Self::migrate), so a tool that
    /// merely wants to *look* at a user's library still writes to it — and
    /// would silently upgrade a database belonging to a newer build of the
    /// app than the tool. Measurement harnesses want neither. The connection
    /// refuses writes at the SQLite level, so this is enforced rather than
    /// promised.
    ///
    /// The schema is whatever is on disk: callers must tolerate columns a
    /// migration would have added.
    pub fn open_readonly(dir: &Path) -> io::Result<Meta> {
        let conn = Connection::open_with_flags(
            dir.join(META_DB),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_err)?;
        // No journal_mode/synchronous pragmas: setting journal_mode is itself
        // a write. busy_timeout is not — and it is the one that matters here,
        // since the whole point is reading a database the app is using.
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(sql_err)?;
        Ok(Meta {
            conn: Mutex::new(conn),
        })
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

    /// Whether any watched folder still holds a readable file for this
    /// document.
    ///
    /// A document can be reached by more than one file — the same bytes in
    /// two folders are one book, by design. So "a file went missing" is not
    /// "the document went missing", and treating it that way retracted
    /// books the user still had: delete a spare copy of a paper and the
    /// original, sitting untouched in another folder, left the library.
    /// `dataless` counts as present — an evicted iCloud stub is a file that
    /// is there, just not downloaded.
    pub fn has_present_file(&self, doc: &str) -> bool {
        self.read(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE doc_id = ?1 AND state != 'missing')",
                [doc],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n == 1)
        })
    }

    /// Documents that exist *only* under this root — the ones unlinking it
    /// actually removes from the library.
    ///
    /// The same bytes can sit in two watched folders; the scanner spots the
    /// duplicate by content hash and points both files at one document. So
    /// "every doc under this root" is the wrong set to tombstone: unlinking
    /// a folder holding a second copy of a book would take the original
    /// down with it, and the file it still has on disk would look indexed
    /// while answering nothing.
    pub fn docs_only_in_root(&self, root_id: &str) -> Vec<String> {
        self.read(|c| {
            let mut q = c.prepare(
                "SELECT DISTINCT doc_id FROM files WHERE root_id = ?1
                   AND doc_id NOT IN (SELECT doc_id FROM files WHERE root_id != ?1)",
            )?;
            let rows = q.query_map([root_id], |r| r.get::<_, String>(0))?;
            rows.collect()
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

    // --- read recency -------------------------------------------------------

    /// Mark a document as read now, for the page cache's LRU.
    ///
    /// `at` is unix seconds, passed in rather than read here so the caller
    /// can throttle: this is written from the page-serving path, where a
    /// single search can ask for two dozen images at once and a book's
    /// scroll asks for hundreds. Once a minute per document is plenty to
    /// order an eviction sweep by.
    ///
    /// Upserts, because a document can be served before anything else has
    /// given it a row.
    pub fn touch_read(&self, doc: &str, at: u64) -> io::Result<()> {
        self.write(|c| {
            c.execute(
                "INSERT INTO docs (id, last_read_at) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET last_read_at = excluded.last_read_at",
                params![doc, at as i64],
            )?;
            Ok(())
        })
    }

    /// Every document with a row, least recently read first — the order an
    /// eviction sweep works through. Documents nobody has opened carry 0 and
    /// sort to the front, which is what we want: unread renders are the
    /// cheapest thing in the cache to lose.
    pub fn read_order(&self) -> Vec<(String, u64)> {
        self.read(|c| {
            let mut q =
                c.prepare("SELECT id, last_read_at FROM docs ORDER BY last_read_at ASC, id ASC")?;
            let rows = q.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
            })?;
            rows.collect()
        })
    }

    /// When a document was last read, or `None` if it has no row.
    pub fn last_read(&self, doc: &str) -> Option<u64> {
        self.read(|c| {
            c.query_row("SELECT last_read_at FROM docs WHERE id = ?1", [doc], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
        })
        .map(|v| v.max(0) as u64)
    }

    // --- titles -------------------------------------------------------------

    /// `doc id -> what a person would call its file`: the last path
    /// component, cleaned by [`crate::naming`].
    ///
    /// The display fallback, and the reason it has to exist: document ids
    /// are minted now, opaque by design so that renaming a file in Finder
    /// cannot orphan a book. They used to be slugs made from the filename,
    /// which meant "prettify the id" was a decent name for an untitled
    /// book. It isn't any more — it reads `D01713FA82AD0` — and the file
    /// name is the thing the user actually recognises.
    ///
    /// Derived on read rather than stored, so a better naming rule improves
    /// every library that already exists without a migration.
    ///
    /// Missing files still count. A document whose file went away keeps its
    /// name until something replaces it; falling back to the id there would
    /// be the same unreadable row, arriving at the worst moment.
    pub fn file_names(&self) -> BTreeMap<String, String> {
        self.read(|c| {
            // a book in two folders under two names: take one and take it
            // consistently, rather than letting row order decide
            let mut q = c.prepare("SELECT doc_id, relpath FROM files ORDER BY relpath")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out: BTreeMap<String, String> = BTreeMap::new();
            for row in rows {
                let (doc, relpath) = row?;
                out.entry(doc)
                    .or_insert_with(|| crate::naming::from_path(&relpath));
            }
            Ok(out)
        })
    }

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

    // --- shelves ------------------------------------------------------------

    /// `shelf -> doc ids`. A shelf is a folder: the depth-1 directory a
    /// document's file sits in, recorded by the scanner.
    ///
    /// There is no shelf table and no way to file a document into one from
    /// the app except by moving its file, which is the point — the shelves
    /// are something the user can see and rearrange in Finder, and they
    /// cannot drift out of step with the folders because they *are* the
    /// folders. Documents loose at a root's top level have no shelf.
    pub fn shelves(&self) -> Collections {
        self.read(|c| {
            let mut q = c.prepare(
                // a tombstoned doc keeps its shelf so re-linking the folder
                // restores it, but it must not be counted on one: the shelf
                // tabs are built from this, and a tab that opens onto an
                // empty shelf is how an unlinked folder used to linger
                "SELECT shelf, id FROM docs
                 WHERE shelf IS NOT NULL AND shelf != ''
                   AND coalesce(state, '') != 'deleted'
                 ORDER BY shelf, id",
            )?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Collections::new();
            for row in rows {
                let (shelf, doc) = row?;
                out.entry(shelf).or_default().push(doc);
            }
            Ok(out)
        })
    }

    /// The shelf one document is on.
    pub fn shelf_of_doc(&self, doc: &str) -> Option<String> {
        self.read(|c| {
            c.query_row("SELECT shelf FROM docs WHERE id = ?1", [doc], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .map(Option::flatten)
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
    fn read_recency_round_trips_and_orders_unread_first() {
        let m = Meta::open_in_memory().unwrap();
        m.set_doc_placement("kant", "pdf", None).unwrap();
        m.set_doc_placement("hume", "pdf", None).unwrap();
        m.set_doc_placement("locke", "pdf", None).unwrap();

        // never opened: 0, and no row of its own is needed to say so
        assert_eq!(m.last_read("kant"), Some(0));
        assert_eq!(m.last_read("nobody"), None);

        m.touch_read("hume", 500).unwrap();
        m.touch_read("locke", 100).unwrap();

        assert_eq!(m.last_read("hume"), Some(500));
        // an unread doc must sort ahead of everything read — its renders are
        // the cheapest thing in the cache to lose
        let order: Vec<String> = m.read_order().into_iter().map(|(d, _)| d).collect();
        assert_eq!(order, vec!["kant", "locke", "hume"]);

        // touching again moves it to the back, not to a second row
        m.touch_read("locke", 900).unwrap();
        let order: Vec<String> = m.read_order().into_iter().map(|(d, _)| d).collect();
        assert_eq!(order, vec!["kant", "hume", "locke"]);
    }

    // the page-serving path can be asked for an image before anything else
    // has minted the document a row
    #[test]
    fn touch_read_mints_a_row_for_an_unknown_doc() {
        let m = Meta::open_in_memory().unwrap();
        m.touch_read("fresh", 42).unwrap();
        assert_eq!(m.last_read("fresh"), Some(42));
        assert_eq!(m.read_order(), vec![("fresh".to_string(), 42)]);
    }

    // touch_read upserts into `docs`, so it must not disturb the columns a
    // document already had — losing an ingest state to a page view would be
    // a spectacular way to break the library
    #[test]
    fn touching_a_doc_leaves_the_rest_of_its_row_alone() {
        let m = Meta::open_in_memory().unwrap();
        m.set_title("kant", Some("Critique")).unwrap();
        m.set_doc_placement("kant", "pdf", Some("philosophy"))
            .unwrap();
        m.touch_read("kant", 7).unwrap();

        assert_eq!(m.titles().get("kant").map(String::as_str), Some("Critique"));
        assert_eq!(m.shelf_of_doc("kant").as_deref(), Some("philosophy"));
        assert_eq!(m.last_read("kant"), Some(7));
    }

    // A measurement harness reads a live library while the app is running.
    // "Read-only" has to mean the connection refuses writes, not that the
    // caller intends not to make any.
    #[test]
    fn readonly_open_neither_migrates_nor_writes() {
        let dir = std::env::temp_dir().join(format!("meta-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let m = Meta::open(&dir).unwrap();
            m.set_title("kant", Some("Critique")).unwrap();
        }
        // pretend the app is a schema behind this build: drop the version
        // rows so open() would re-run every migration, and open_readonly
        // must not
        {
            let m = Meta::open(&dir).unwrap();
            m.write(|c| c.execute("DELETE FROM schema_version", []))
                .unwrap();
        }

        let ro = Meta::open_readonly(&dir).unwrap();
        assert_eq!(
            ro.titles().get("kant").map(String::as_str),
            Some("Critique")
        );
        let versions: i64 = ro
            .write(|c| c.query_row("SELECT count(*) FROM schema_version", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(versions, 0, "open_readonly must not migrate");

        // and the connection itself refuses a write
        let err = ro
            .write(|c| c.execute("INSERT INTO schema_version (version) VALUES (1)", []))
            .expect_err("a read-only connection must reject writes");
        assert!(
            err.to_string().contains("readonly") || err.to_string().contains("read-only"),
            "unexpected error: {err}"
        );

        // opening a database that isn't there is an error, not a creation
        let missing = dir.join("nope");
        assert!(Meta::open_readonly(&missing).is_err());
        assert!(!missing.join(META_DB).exists());

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
    fn shelves_come_from_folders_and_need_no_pruning() {
        let m = Meta::open_in_memory().unwrap();
        assert!(m.shelves().is_empty());

        m.set_doc_placement("artusi", "pdf", Some("cookbooks"))
            .unwrap();
        m.set_doc_placement("escoffier", "pdf", Some("cookbooks"))
            .unwrap();
        m.set_doc_placement("sicp", "pdf", Some("software"))
            .unwrap();
        // a document loose at the top of a root is on no shelf
        m.set_doc_placement("stray", "pdf", None).unwrap();

        let shelves = m.shelves();
        assert_eq!(shelves["cookbooks"], vec!["artusi", "escoffier"]);
        assert_eq!(shelves["software"], vec!["sicp"]);
        assert_eq!(shelves.len(), 2, "no shelf for the loose document");
        assert_eq!(m.shelf_of_doc("artusi").as_deref(), Some("cookbooks"));
        assert_eq!(m.shelf_of_doc("stray"), None);

        // moving the last book off a shelf retires the shelf, with nothing
        // to prune: the shelf only ever existed as the documents on it
        m.set_doc_placement("sicp", "pdf", Some("cookbooks"))
            .unwrap();
        let shelves = m.shelves();
        assert!(!shelves.contains_key("software"));
        assert_eq!(shelves["cookbooks"].len(), 3);
    }

    #[test]
    fn a_document_is_named_after_its_file() {
        let m = Meta::open_in_memory().unwrap();
        let r = m.add_root(Path::new("/tmp/does-not-exist-n"), 1).unwrap();
        let stat = crate::roots::Stat {
            inode: 1,
            size: 2,
            mtime: 3,
            dataless: false,
        };
        m.put_file(&r.id, "Artusi 1891.pdf", "d1", &stat, None, 1)
            .unwrap();
        // the extension goes and so does the download tag — the naming
        // rules themselves are tested in `crate::naming`
        m.put_file(
            &r.id,
            "cookbooks/Il Cucchiaio (z-library.sk, 1lib.sk).pdf",
            "d2",
            &stat,
            None,
            1,
        )
        .unwrap();
        // a book in two folders under two names: one answer, deterministically
        m.put_file(&r.id, "zzz-late-edition.pdf", "d3", &stat, None, 1)
            .unwrap();
        m.put_file(&r.id, "aaa-early-edition.pdf", "d3", &stat, None, 1)
            .unwrap();

        let names = m.file_names();
        assert_eq!(names["d1"], "Artusi 1891");
        assert_eq!(names["d2"], "Il Cucchiaio");
        assert_eq!(names["d3"], "Aaa Early Edition");
        assert!(!names.contains_key("d-never-seen"));

        // a file that went away keeps naming its document — an unreadable
        // id is the last thing you want when a book has just gone missing
        m.set_file_state(&r.id, "Artusi 1891.pdf", "missing")
            .unwrap();
        assert_eq!(m.file_names()["d1"], "Artusi 1891");
    }

    #[test]
    fn unlinking_spares_a_book_that_lives_in_another_folder_too() {
        let m = Meta::open_in_memory().unwrap();
        let a = m.add_root(Path::new("/tmp/does-not-exist-a"), 1).unwrap();
        let b = m.add_root(Path::new("/tmp/does-not-exist-b"), 1).unwrap();
        let stat = crate::roots::Stat {
            inode: 1,
            size: 2,
            mtime: 3,
            dataless: false,
        };
        // one book only in A, one only in B, and one the user keeps in both
        m.put_file(&a.id, "solo-a.pdf", "dsoloa", &stat, None, 1)
            .unwrap();
        m.put_file(&b.id, "solo-b.pdf", "dsolob", &stat, None, 1)
            .unwrap();
        m.put_file(&a.id, "shared.pdf", "dshared", &stat, None, 1)
            .unwrap();
        m.put_file(&b.id, "copy-of-shared.pdf", "dshared", &stat, None, 1)
            .unwrap();

        assert_eq!(m.docs_only_in_root(&a.id), vec!["dsoloa"]);
        assert_eq!(m.docs_only_in_root(&b.id), vec!["dsolob"]);

        // the same rule the sweep needs: losing one copy of the shared book
        // is not losing the book
        m.set_file_state(&a.id, "shared.pdf", "missing").unwrap();
        assert!(m.has_present_file("dshared"), "B still has its copy");
        m.set_file_state(&b.id, "copy-of-shared.pdf", "missing")
            .unwrap();
        assert!(!m.has_present_file("dshared"), "now every copy is gone");
        assert!(!m.has_present_file("dnever-existed"));

        // once A is gone, B is the only place the shared book lives — and
        // unlinking B then does take it
        m.remove_root(&a.id).unwrap();
        let mut left = m.docs_only_in_root(&b.id);
        left.sort();
        assert_eq!(left, vec!["dshared", "dsolob"]);
    }

    #[test]
    fn a_tombstoned_doc_leaves_its_shelf_but_keeps_its_place() {
        // Unlinking a folder tombstones its documents. The shelf tabs are
        // built from `shelves()`, so a tombstone that stayed on its shelf
        // left a tab behind with a count and nothing under it — the folder
        // looked removed everywhere except the one place you'd look first.
        let m = Meta::open_in_memory().unwrap();
        m.set_doc_placement("artusi", "pdf", Some("cookbooks"))
            .unwrap();
        m.set_doc_placement("escoffier", "pdf", Some("cookbooks"))
            .unwrap();
        m.write(|c| {
            c.execute(
                "UPDATE docs SET state = 'deleted' WHERE id = 'escoffier'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        assert_eq!(m.shelves()["cookbooks"], vec!["artusi"]);

        // the last one goes and the shelf goes with it
        m.write(|c| {
            c.execute("UPDATE docs SET state = 'deleted' WHERE id = 'artusi'", [])?;
            Ok(())
        })
        .unwrap();
        assert!(m.shelves().is_empty());

        // but the placement is still recorded, so re-linking the folder
        // restores the shelf instead of re-deriving it from scratch
        assert_eq!(m.shelf_of_doc("artusi").as_deref(), Some("cookbooks"));
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
