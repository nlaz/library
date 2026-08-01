//! Watched folders, and reconciling them against what we have indexed.
//!
//! The filesystem is the truth and the index is a reflection of it. A root
//! is a folder the user pointed us at — the default `~/The Library`, or one
//! they linked in place — and we never move, rename or reorganize anything
//! inside one. Scanning a root produces a list of what is there; reconciling
//! that against the `files` table produces the changes to apply.
//!
//! # Identity
//!
//! A document's id is minted once ([`crate::notes::mint_id`]) and never
//! derived from its path, because a path is the least stable thing about a
//! file. What ties a file to its document across a change is, in order:
//!
//! - the **path**, when nothing moved;
//! - the **inode**, when it was renamed or moved within a volume — this is
//!   what keeps a Finder rename from orphaning 400 page renders;
//! - the **content hash**, when it was copied, or moved across volumes
//!   (which changes the inode).
//!
//! # Refusing to act
//!
//! Deleting a file deletes its document, which makes every *apparent*
//! deletion dangerous. An unmounted volume, a folder that briefly fails to
//! read, an iCloud eviction — each looks exactly like "the user deleted
//! everything". [`reconcile`] therefore refuses to report disappearances
//! unless it is confident it saw the root properly, and [`Verdict`] carries
//! the reason when it declines.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::records::SourceKind;

/// Fraction of a root's documents that can vanish in one scan before we
/// stop believing the scan. Bulk deletions happen — but so do half-mounted
/// network shares, and one of those two is recoverable.
pub const MASS_DELETE_RATIO: f32 = 0.2;

/// Below this many documents, the ratio guard is off: a 3-document root
/// hits 20% by losing one file, which is an ordinary thing to do.
pub const MASS_DELETE_FLOOR: usize = 5;

/// What the filesystem says about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub inode: u64,
    pub size: u64,
    pub mtime: i64,
    /// The file exists but its contents are not on this disk — an iCloud or
    /// Dropbox stub (`SF_DATALESS`). Reading it would trigger a download we
    /// have no business starting during a scan.
    pub dataless: bool,
}

/// One file found by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Path relative to the root, `/`-separated.
    pub relpath: String,
    pub stat: Stat,
    pub kind: SourceKind,
}

/// What we already know about a file, from the `files` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub doc: String,
    pub relpath: String,
    pub inode: u64,
    pub size: u64,
    pub mtime: i64,
    pub content_hash: Option<String>,
}

/// One change to apply to the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A file we have never seen. Mint a document and queue an ingest.
    Added {
        relpath: String,
        stat: Stat,
        kind: SourceKind,
    },
    /// The same file at a new path — a rename or a move within the root.
    /// Nothing to re-index; the document keeps its caches and its notes.
    Moved {
        doc: String,
        from: String,
        to: String,
        stat: Stat,
    },
    /// Same path, different bytes. Re-ingest in place under the same id.
    Edited {
        doc: String,
        relpath: String,
        stat: Stat,
    },
    /// A copy of a document we already have.
    Duplicate {
        doc: String,
        relpath: String,
        stat: Stat,
    },
    /// The file is gone. The document goes `missing`, keeping its caches
    /// and its notes until the user says otherwise.
    Missing { doc: String, relpath: String },
    /// Present but not readable right now (evicted to the cloud). Not a
    /// disappearance, and never a reason to retract anything.
    Offline { doc: String, relpath: String },
}

/// Why a scan's disappearances were not believed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declined {
    /// The root itself could not be read — unmounted, unplugged, deleted.
    RootUnavailable,
    /// Too much vanished at once to be a deliberate act.
    MassDeletion { vanished: usize, known: usize },
}

/// The outcome of reconciling one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub changes: Vec<Change>,
    /// Set when disappearances were suppressed. The changes list still
    /// carries everything else the scan found — a half-visible root can
    /// still add and update — but nothing was retracted.
    pub declined: Option<Declined>,
}

/// Reconcile one root's scan against what we have indexed.
///
/// `found` is `None` when the root could not be read at all, which is
/// different from an empty root: an unmounted drive returns `None`, an
/// emptied folder returns `Some(vec![])`, and only the second one means
/// the documents are gone.
pub fn reconcile(known: &[Known], found: Option<&[Found]>) -> Verdict {
    let Some(found) = found else {
        return Verdict {
            changes: Vec::new(),
            declined: Some(Declined::RootUnavailable),
        };
    };

    let by_path: BTreeMap<&str, &Known> = known.iter().map(|k| (k.relpath.as_str(), k)).collect();
    // an inode is only a useful identifier while it is unique: reuse after
    // a delete is normal, so a duplicated one identifies nothing
    let mut by_inode: BTreeMap<u64, Option<&Known>> = BTreeMap::new();
    for k in known.iter().filter(|k| k.inode != 0) {
        by_inode
            .entry(k.inode)
            .and_modify(|slot| *slot = None)
            .or_insert(Some(k));
    }

    let mut changes = Vec::new();
    let mut matched: Vec<&str> = Vec::new();

    for f in found {
        if let Some(k) = by_path.get(f.relpath.as_str()) {
            matched.push(k.relpath.as_str());
            if f.stat.dataless {
                changes.push(Change::Offline {
                    doc: k.doc.clone(),
                    relpath: f.relpath.clone(),
                });
            } else if k.size != f.stat.size || k.mtime != f.stat.mtime {
                // the cheap probe disagrees; the caller hashes to confirm
                changes.push(Change::Edited {
                    doc: k.doc.clone(),
                    relpath: f.relpath.clone(),
                    stat: f.stat.clone(),
                });
            }
            continue;
        }

        // new path. Same inode as something we know? Then it is that file,
        // renamed or moved — provided the file it used to be is no longer
        // at its old path (else this is a hardlink, i.e. a second name).
        let moved = by_inode
            .get(&f.stat.inode)
            .and_then(|slot| *slot)
            .filter(|k| !found.iter().any(|g| g.relpath == k.relpath));
        if let Some(k) = moved {
            matched.push(k.relpath.as_str());
            changes.push(Change::Moved {
                doc: k.doc.clone(),
                from: k.relpath.clone(),
                to: f.relpath.clone(),
                stat: f.stat.clone(),
            });
            continue;
        }

        changes.push(Change::Added {
            relpath: f.relpath.clone(),
            stat: f.stat.clone(),
            kind: f.kind,
        });
    }

    // whatever the scan never accounted for has disappeared
    let vanished: Vec<&Known> = known
        .iter()
        .filter(|k| !matched.contains(&k.relpath.as_str()))
        .collect();

    if !vanished.is_empty() && is_mass_deletion(vanished.len(), known.len()) {
        return Verdict {
            changes,
            declined: Some(Declined::MassDeletion {
                vanished: vanished.len(),
                known: known.len(),
            }),
        };
    }
    for k in vanished {
        changes.push(Change::Missing {
            doc: k.doc.clone(),
            relpath: k.relpath.clone(),
        });
    }
    Verdict {
        changes,
        declined: None,
    }
}

fn is_mass_deletion(vanished: usize, known: usize) -> bool {
    known >= MASS_DELETE_FLOOR && vanished as f32 / known as f32 > MASS_DELETE_RATIO
}

/// The shelf a file's path puts it on: its first path component, or
/// `None` for a file sitting loose at the root.
///
/// Deeper nesting flattens into the top folder rather than making shelves
/// of its own — the shelf list is a browsing surface, and one that mirrors
/// an arbitrarily deep tree stops being one.
pub fn shelf_of(relpath: &str) -> Option<&str> {
    let (head, rest) = relpath.split_once('/')?;
    (!rest.is_empty() && !head.is_empty()).then_some(head)
}

/// Read a root, returning every ingestible file under it.
///
/// `None` means the root could not be read — the caller must treat that as
/// "unknown", never as "empty". Unreadable *sub*directories are skipped
/// with the rest of the scan intact: one bad folder inside a large root
/// should not blind us to the whole thing.
pub fn scan(root: &Path) -> Option<Vec<Found>> {
    if !root.is_dir() {
        return None;
    }
    let mut out = Vec::new();
    walk(root, root, &mut out, 0);
    out.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    Some(out)
}

/// Deepest directory nesting a scan will follow. Deep enough for any real
/// document folder, shallow enough that a symlink cycle can't hang a scan.
const MAX_DEPTH: usize = 16;

fn walk(root: &Path, dir: &Path, out: &mut Vec<Found>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // unreadable subdirectory: skip it, keep the rest
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        // dotfiles are never documents, and skipping them keeps .git,
        // .DS_Store and the sync clients' scratch out of the library
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(root, &path, out, depth + 1);
            continue;
        }
        // symlinks are followed for files (an alias to a PDF is a
        // reasonable thing to keep) but never for directories, where they
        // are how a scan ends up walking a whole home folder
        let Some(kind) = SourceKind::of(&path) else {
            continue;
        };
        let Ok(relpath) = path.strip_prefix(root) else {
            continue;
        };
        let Some(stat) = stat(&path) else { continue };
        out.push(Found {
            relpath: relpath.to_string_lossy().replace('\\', "/"),
            stat,
            kind,
        });
    }
}

/// `SF_DATALESS` — set by macOS on a file whose contents have been evicted
/// to iCloud. Not in the libc crate.
#[cfg(target_os = "macos")]
const SF_DATALESS: u32 = 0x4000_0000;

#[cfg(target_os = "macos")]
pub fn stat(path: &Path) -> Option<Stat> {
    use std::os::macos::fs::MetadataExt as _;
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::metadata(path).ok()?;
    Some(Stat {
        inode: m.ino(),
        size: m.size(),
        mtime: m.mtime(),
        dataless: m.st_flags() & SF_DATALESS != 0,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn stat(path: &Path) -> Option<Stat> {
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::metadata(path).ok()?;
    Some(Stat {
        inode: m.ino(),
        size: m.size(),
        mtime: m.mtime(),
        dataless: false,
    })
}

/// Content hash: xxh3-128, hex. Fast enough (multiple GB/s) to hash a whole
/// book without thinking about it, and used only as a dedup and change
/// signal — never as an identifier, so its lack of collision resistance
/// against an adversary doesn't matter here.
pub fn content_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes)))
}

/// Absolute path of a file inside a root.
pub fn abs(root: &Path, relpath: &str) -> PathBuf {
    root.join(relpath)
}

/// What a sync did, for the caller to report and to decide whether to wake
/// the ingest worker.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Documents needing ingest — new files and edited ones.
    pub queued: Vec<String>,
    pub moved: usize,
    pub missing: Vec<String>,
    pub offline: usize,
    pub duplicates: usize,
    pub declined: Option<Declined>,
}

/// Scan one root and bring the index in line with it.
///
/// Returns the documents that need ingesting. Nothing here touches the
/// search indexes: this decides *what* changed, and the ingest worker acts
/// on it. That split is what lets a sync run while a commit is in flight.
pub fn sync_root(meta: &crate::meta::Meta, root: &crate::meta::RootRec, now: u64) -> Applied {
    let known = meta.files_in_root(&root.id);
    let found = scan(&root.path);
    let verdict = reconcile(&known, found.as_deref());

    let state = if matches!(verdict.declined, Some(Declined::RootUnavailable)) {
        "unavailable"
    } else {
        "watching"
    };
    let _ = meta.set_root_state(&root.id, state, Some(now));

    let mut out = Applied {
        declined: verdict.declined.clone(),
        ..Default::default()
    };
    for change in verdict.changes {
        match change {
            Change::Added {
                relpath,
                stat,
                kind,
            } => {
                // hash before minting: a file that appeared with a fresh
                // inode but bytes we already hold is a copy, not a new book
                let hash = content_hash(&abs(&root.path, &relpath));
                let existing = hash.as_deref().and_then(|h| meta.doc_with_hash(h));
                let doc = match existing {
                    Some(doc) => {
                        out.duplicates += 1;
                        doc
                    }
                    None => {
                        let doc = crate::notes::mint_id('d');
                        out.queued.push(doc.clone());
                        doc
                    }
                };
                let _ = meta.put_file(&root.id, &relpath, &doc, &stat, hash.as_deref(), now);
                let _ = meta.set_doc_placement(&doc, kind.as_str(), shelf_of(&relpath));
            }
            Change::Moved {
                doc,
                from,
                to,
                stat,
            } => {
                let _ = meta.move_file(&root.id, &from, &to, &stat, now);
                // the shelf follows the folder, so a move between folders
                // re-files the document without re-indexing a byte
                let kind = SourceKind::of(Path::new(&to)).unwrap_or(SourceKind::Pdf);
                let _ = meta.set_doc_placement(&doc, kind.as_str(), shelf_of(&to));
                out.moved += 1;
            }
            Change::Edited { doc, relpath, stat } => {
                let hash = content_hash(&abs(&root.path, &relpath));
                let _ = meta.put_file(&root.id, &relpath, &doc, &stat, hash.as_deref(), now);
                out.queued.push(doc);
            }
            Change::Duplicate { doc, relpath, stat } => {
                let _ = meta.put_file(&root.id, &relpath, &doc, &stat, None, now);
                out.duplicates += 1;
            }
            Change::Missing { doc, relpath } => {
                let _ = meta.set_file_state(&root.id, &relpath, "missing");
                out.missing.push(doc);
            }
            Change::Offline { relpath, .. } => {
                let _ = meta.set_file_state(&root.id, &relpath, "dataless");
                out.offline += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(inode: u64, size: u64, mtime: i64) -> Stat {
        Stat {
            inode,
            size,
            mtime,
            dataless: false,
        }
    }

    fn known(doc: &str, relpath: &str, inode: u64, size: u64, mtime: i64) -> Known {
        Known {
            doc: doc.into(),
            relpath: relpath.into(),
            inode,
            size,
            mtime,
            content_hash: None,
        }
    }

    fn found(relpath: &str, stat: Stat) -> Found {
        Found {
            relpath: relpath.into(),
            stat,
            kind: SourceKind::Pdf,
        }
    }

    #[test]
    fn a_new_file_is_added() {
        let v = reconcile(&[], Some(&[found("a.pdf", st(1, 10, 100))]));
        assert_eq!(v.declined, None);
        assert!(matches!(
            v.changes.as_slice(),
            [Change::Added { relpath, .. }] if relpath == "a.pdf"
        ));
    }

    #[test]
    fn an_unchanged_file_produces_nothing() {
        let k = [known("d1", "a.pdf", 1, 10, 100)];
        let v = reconcile(&k, Some(&[found("a.pdf", st(1, 10, 100))]));
        assert_eq!(v.changes, vec![]);
    }

    #[test]
    fn a_rename_keeps_the_document() {
        // the whole point of minted ids: same inode, new name
        let k = [known("d1", "old-name.pdf", 42, 10, 100)];
        let v = reconcile(&k, Some(&[found("new-name.pdf", st(42, 10, 100))]));
        assert_eq!(
            v.changes,
            vec![Change::Moved {
                doc: "d1".into(),
                from: "old-name.pdf".into(),
                to: "new-name.pdf".into(),
                stat: st(42, 10, 100),
            }]
        );
    }

    #[test]
    fn a_move_into_a_subfolder_keeps_the_document() {
        let k = [known("d1", "loose.pdf", 42, 10, 100)];
        let v = reconcile(&k, Some(&[found("Cookbooks/loose.pdf", st(42, 10, 100))]));
        assert!(matches!(&v.changes[..], [Change::Moved { doc, to, .. }]
            if doc == "d1" && to == "Cookbooks/loose.pdf"));
    }

    #[test]
    fn an_edit_in_place_re_ingests_under_the_same_id() {
        let k = [known("d1", "a.pdf", 1, 10, 100)];
        let v = reconcile(&k, Some(&[found("a.pdf", st(1, 99, 200))]));
        assert!(matches!(&v.changes[..], [Change::Edited { doc, .. }] if doc == "d1"));
    }

    #[test]
    fn a_deleted_file_goes_missing() {
        let k = [known("d1", "a.pdf", 1, 10, 100)];
        let v = reconcile(&k, Some(&[]));
        assert_eq!(
            v.changes,
            vec![Change::Missing {
                doc: "d1".into(),
                relpath: "a.pdf".into()
            }]
        );
    }

    #[test]
    fn a_hardlink_is_a_new_file_not_a_move() {
        // same inode, but the original is still sitting at its old path —
        // treating this as a move would silently drop one of the two
        let k = [known("d1", "a.pdf", 42, 10, 100)];
        let v = reconcile(
            &k,
            Some(&[
                found("a.pdf", st(42, 10, 100)),
                found("b.pdf", st(42, 10, 100)),
            ]),
        );
        assert!(matches!(&v.changes[..], [Change::Added { relpath, .. }] if relpath == "b.pdf"));
    }

    #[test]
    fn a_reused_inode_does_not_claim_a_rename() {
        // two known files share an inode (one was deleted and the number
        // recycled): it identifies neither, so this is a plain add
        let k = [
            known("d1", "a.pdf", 42, 10, 100),
            known("d2", "b.pdf", 42, 20, 100),
        ];
        let v = reconcile(&k, Some(&[found("c.pdf", st(42, 30, 300))]));
        let adds = v
            .changes
            .iter()
            .filter(|c| matches!(c, Change::Added { .. }))
            .count();
        assert_eq!(adds, 1, "ambiguous inode must not resolve to a move");
    }

    #[test]
    fn an_unreadable_root_retracts_nothing() {
        // the unmounted-volume case: everything looks deleted, nothing is
        let k = [
            known("d1", "a.pdf", 1, 10, 100),
            known("d2", "b.pdf", 2, 10, 100),
        ];
        let v = reconcile(&k, None);
        assert_eq!(v.changes, vec![]);
        assert_eq!(v.declined, Some(Declined::RootUnavailable));
    }

    #[test]
    fn a_mass_deletion_is_refused() {
        let k: Vec<Known> = (0..10)
            .map(|i| known(&format!("d{i}"), &format!("{i}.pdf"), i as u64 + 1, 10, 100))
            .collect();
        // three of ten vanish: over the ratio, so nothing is retracted
        let f: Vec<Found> = (0..7)
            .map(|i| found(&format!("{i}.pdf"), st(i as u64 + 1, 10, 100)))
            .collect();
        let v = reconcile(&k, Some(&f));
        assert!(
            !v.changes
                .iter()
                .any(|c| matches!(c, Change::Missing { .. }))
        );
        assert_eq!(
            v.declined,
            Some(Declined::MassDeletion {
                vanished: 3,
                known: 10
            })
        );
    }

    #[test]
    fn an_ordinary_deletion_is_believed() {
        let k: Vec<Known> = (0..10)
            .map(|i| known(&format!("d{i}"), &format!("{i}.pdf"), i as u64 + 1, 10, 100))
            .collect();
        let f: Vec<Found> = (0..9)
            .map(|i| found(&format!("{i}.pdf"), st(i as u64 + 1, 10, 100)))
            .collect();
        let v = reconcile(&k, Some(&f));
        assert_eq!(v.declined, None);
        assert!(matches!(&v.changes[..], [Change::Missing { doc, .. }] if doc == "d9"));
    }

    #[test]
    fn a_small_root_can_lose_a_file_without_tripping_the_guard() {
        // 1 of 3 is 33%, over the ratio — but three files is not a library
        // whose loss needs a second opinion
        let k = [
            known("d1", "a.pdf", 1, 10, 100),
            known("d2", "b.pdf", 2, 10, 100),
            known("d3", "c.pdf", 3, 10, 100),
        ];
        let v = reconcile(
            &k,
            Some(&[
                found("a.pdf", st(1, 10, 100)),
                found("b.pdf", st(2, 10, 100)),
            ]),
        );
        assert_eq!(v.declined, None);
        assert!(matches!(&v.changes[..], [Change::Missing { doc, .. }] if doc == "d3"));
    }

    #[test]
    fn an_evicted_file_is_offline_not_missing() {
        // iCloud pulled the bytes; the file is still there and the document
        // must not be retracted
        let k = [known("d1", "a.pdf", 1, 10, 100)];
        let stub = Stat {
            dataless: true,
            ..st(1, 10, 100)
        };
        let v = reconcile(&k, Some(&[found("a.pdf", stub)]));
        assert_eq!(
            v.changes,
            vec![Change::Offline {
                doc: "d1".into(),
                relpath: "a.pdf".into()
            }]
        );
    }

    #[test]
    fn an_evicted_file_is_not_reported_as_edited() {
        // eviction can change the reported size; without the dataless check
        // that reads as an edit and triggers a re-ingest of a file whose
        // bytes aren't on this disk
        let k = [known("d1", "a.pdf", 1, 4_000_000, 100)];
        let stub = Stat {
            size: 0,
            dataless: true,
            ..st(1, 0, 400)
        };
        let v = reconcile(&k, Some(&[found("a.pdf", stub)]));
        assert!(matches!(&v.changes[..], [Change::Offline { .. }]));
    }

    #[test]
    fn shelf_is_the_first_path_component() {
        assert_eq!(shelf_of("Cookbooks/artusi.pdf"), Some("Cookbooks"));
        // deeper nesting flattens into the top folder
        assert_eq!(
            shelf_of("Cookbooks/Italian/marcella.pdf"),
            Some("Cookbooks")
        );
        // a loose file has no shelf
        assert_eq!(shelf_of("loose.pdf"), None);
    }

    #[test]
    fn scan_finds_nested_files_and_skips_the_rest() {
        let dir = std::env::temp_dir().join(format!("roots-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Cookbooks/Italian")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("loose.pdf"), b"%PDF-").unwrap();
        std::fs::write(dir.join("Cookbooks/artusi.PDF"), b"%PDF-").unwrap();
        std::fs::write(dir.join("Cookbooks/Italian/marcella.pdf"), b"%PDF-").unwrap();
        std::fs::write(dir.join("Cookbooks/notes.txt"), b"hi").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"junk").unwrap();
        std::fs::write(dir.join(".hidden/secret.pdf"), b"%PDF-").unwrap();

        let found = scan(&dir).expect("readable root");
        let paths: Vec<&str> = found.iter().map(|f| f.relpath.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "Cookbooks/Italian/marcella.pdf",
                "Cookbooks/artusi.PDF",
                "loose.pdf",
            ],
            "nested yes; unsupported, dotfiles and dot-dirs no"
        );

        // a root that isn't there reads as unknown, never as empty
        assert!(scan(&dir.join("nope")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn content_hash_is_stable_and_distinguishes() {
        let dir = std::env::temp_dir().join(format!("roots-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), b"the same bytes").unwrap();
        std::fs::write(dir.join("b"), b"the same bytes").unwrap();
        std::fs::write(dir.join("c"), b"other bytes").unwrap();

        let a = content_hash(&dir.join("a")).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(a, content_hash(&dir.join("b")).unwrap());
        assert_ne!(a, content_hash(&dir.join("c")).unwrap());
        assert_eq!(content_hash(&dir.join("missing")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
