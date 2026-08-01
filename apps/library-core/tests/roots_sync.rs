//! Watching a real folder end to end: what the user does in Finder, and
//! what the library does about it.
//!
//! `roots::reconcile` has the decision table under unit test; this drives
//! the whole path — real files, a real database — because the parts that
//! break in practice are the seams between them (a rename must keep the
//! document *and* its row, a re-scan must be idempotent).

use library_core::meta::{Ctx, RootRec};
use library_core::roots::{self, Declined};

struct Lib {
    ctx: Ctx,
    root: RootRec,
    dir: std::path::PathBuf,
    clock: std::cell::Cell<u64>,
}

impl Drop for Lib {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Lib {
    fn new(name: &str) -> Lib {
        let dir = std::env::temp_dir().join(format!("roots-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("library")).expect("fixture root");
        let ctx = Ctx::in_memory(&dir).expect("meta db");
        let root = ctx
            .add_root(&dir.join("library"), 1)
            .expect("link the root");
        Lib {
            ctx,
            root,
            dir,
            clock: std::cell::Cell::new(1),
        }
    }

    fn lib_dir(&self) -> std::path::PathBuf {
        self.dir.join("library")
    }

    fn write(&self, relpath: &str, bytes: &[u8]) {
        let p = self.lib_dir().join(relpath);
        std::fs::create_dir_all(p.parent().expect("a parent")).expect("mkdir");
        std::fs::write(p, bytes).expect("write file");
    }

    fn rename(&self, from: &str, to: &str) {
        let to = self.lib_dir().join(to);
        std::fs::create_dir_all(to.parent().expect("a parent")).expect("mkdir");
        std::fs::rename(self.lib_dir().join(from), to).expect("rename");
    }

    fn remove(&self, relpath: &str) {
        std::fs::remove_file(self.lib_dir().join(relpath)).expect("remove");
    }

    /// Sync, advancing the clock so `last_seen_at` moves.
    fn sync(&self) -> roots::Applied {
        self.clock.set(self.clock.get() + 1);
        let root = self
            .ctx
            .roots()
            .into_iter()
            .find(|r| r.id == self.root.id)
            .expect("root row");
        roots::sync_root(&self.ctx.meta, &root, self.clock.get())
    }

    /// Files the library can actually see right now.
    fn present_docs(&self) -> Vec<(String, String)> {
        self.ctx
            .files_in_root(&self.root.id)
            .into_iter()
            .filter(|k| !k.missing)
            .map(|k| (k.relpath, k.doc))
            .collect()
    }

    fn docs_on_disk(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .ctx
            .files_in_root(&self.root.id)
            .into_iter()
            .map(|k| (k.relpath, k.doc))
            .collect();
        v.sort();
        v
    }
}

#[test]
fn adding_a_file_mints_one_document_and_a_rescan_adds_nothing() {
    let lib = Lib::new("add");
    lib.write("artusi.pdf", b"%PDF-artusi");

    let a = lib.sync();
    assert_eq!(a.queued.len(), 1, "one new document");
    assert_eq!(a.missing.len(), 0);
    assert_eq!(a.declined, None);

    // idempotence is the whole contract of a periodic scan
    let b = lib.sync();
    assert_eq!(b.queued.len(), 0, "an unchanged file queues nothing");
    assert_eq!(b.moved, 0);
    assert_eq!(lib.docs_on_disk().len(), 1);
}

#[test]
fn renaming_in_finder_keeps_the_document() {
    let lib = Lib::new("rename");
    lib.write("bad-scan-name.pdf", b"%PDF-artusi");
    let doc = lib.sync().queued.pop().expect("a document");

    lib.rename("bad-scan-name.pdf", "Artusi 1891.pdf");
    let a = lib.sync();
    assert_eq!(a.moved, 1);
    assert_eq!(
        a.queued,
        Vec::<String>::new(),
        "a rename re-indexes nothing"
    );
    assert_eq!(a.missing, Vec::<String>::new());
    assert_eq!(
        lib.docs_on_disk(),
        vec![("Artusi 1891.pdf".to_string(), doc)],
        "same document id at the new path"
    );
}

#[test]
fn moving_between_folders_refiles_without_reindexing() {
    let lib = Lib::new("shelve");
    lib.write("artusi.pdf", b"%PDF-artusi");
    let doc = lib.sync().queued.pop().expect("a document");

    lib.rename("artusi.pdf", "Cookbooks/artusi.pdf");
    let a = lib.sync();
    assert_eq!(a.moved, 1);
    assert!(a.queued.is_empty());

    // the shelf came from the folder
    let shelf: Option<String> = lib
        .ctx
        .write(|c| c.query_row("SELECT shelf FROM docs WHERE id = ?1", [&doc], |r| r.get(0)))
        .unwrap();
    assert_eq!(shelf.as_deref(), Some("Cookbooks"));
}

#[test]
fn a_copy_is_recognised_as_the_same_document() {
    let lib = Lib::new("dupe");
    lib.write("artusi.pdf", b"%PDF-artusi");
    let doc = lib.sync().queued.pop().expect("a document");

    // the same book, downloaded twice under different names
    lib.write("Cookbooks/artusi (1).pdf", b"%PDF-artusi");
    let a = lib.sync();
    assert_eq!(a.duplicates, 1);
    assert!(a.queued.is_empty(), "a duplicate must not be re-OCR'd");

    let docs = lib.docs_on_disk();
    assert_eq!(docs.len(), 2, "two files");
    assert!(
        docs.iter().all(|(_, d)| d == &doc),
        "one document between them"
    );

    // and the copy does not re-file the book it duplicates: the original
    // was loose at the top level and stays there, even though the copy
    // landed in a folder
    let shelf: Option<String> = lib
        .ctx
        .write(|c| c.query_row("SELECT shelf FROM docs WHERE id = ?1", [&doc], |r| r.get(0)))
        .expect("shelf");
    assert_eq!(shelf, None, "a duplicate must not move the document");
}

#[test]
fn deleting_a_file_marks_the_document_missing_and_restoring_it_returns_the_same_one() {
    let lib = Lib::new("delete");
    for i in 0..6 {
        lib.write(&format!("book-{i}.pdf"), format!("%PDF-{i}").as_bytes());
    }
    lib.sync();
    let before = lib.docs_on_disk();
    let doc = before
        .iter()
        .find(|(p, _)| p == "book-3.pdf")
        .map(|(_, d)| d.clone())
        .expect("book-3");

    lib.remove("book-3.pdf");
    let a = lib.sync();
    assert_eq!(a.missing, vec![doc.clone()]);
    assert_eq!(a.declined, None);
    assert_eq!(lib.present_docs().len(), 5, "out of the working set");

    // a second scan while it is still gone must not report it again —
    // otherwise the document is re-retracted forever and the mass-deletion
    // guard never resets
    let quiet = lib.sync();
    assert_eq!(quiet.missing, Vec::<String>::new());

    // put it back: the *same* document returns. This is the assertion the
    // count-only version of this test was missing, and the bug it hid was
    // a restored file minting a second document and orphaning the first
    // one's page renders and notes.
    lib.write("book-3.pdf", b"%PDF-3");
    let b = lib.sync();
    assert_eq!(b.returned, 1);
    assert_eq!(b.missing, Vec::<String>::new());
    assert_eq!(b.queued, vec![doc.clone()], "re-indexed under its own id");
    assert_eq!(
        lib.docs_on_disk(),
        before,
        "every path maps to the document it did before"
    );
}

#[test]
fn an_unmounted_root_never_retracts() {
    let lib = Lib::new("unmount");
    for i in 0..6 {
        lib.write(&format!("book-{i}.pdf"), format!("%PDF-{i}").as_bytes());
    }
    lib.sync();

    // the volume goes away — every file "disappears" at once
    std::fs::remove_dir_all(lib.lib_dir()).unwrap();
    let a = lib.sync();
    assert_eq!(a.missing, Vec::<String>::new(), "nothing may be retracted");
    assert_eq!(a.declined, Some(Declined::RootUnavailable));
    assert_eq!(
        lib.ctx
            .roots()
            .into_iter()
            .find(|r| r.id == lib.root.id)
            .unwrap()
            .state,
        "unavailable"
    );
    assert_eq!(lib.docs_on_disk().len(), 6, "the library is intact");

    // remount: everything comes back, nothing is re-ingested
    std::fs::create_dir_all(lib.lib_dir()).unwrap();
    for i in 0..6 {
        lib.write(&format!("book-{i}.pdf"), format!("%PDF-{i}").as_bytes());
    }
    let b = lib.sync();
    assert_eq!(b.declined, None);
    assert!(b.queued.is_empty(), "a remount is not a re-ingest");
}

#[test]
fn a_mass_deletion_is_refused_and_the_library_survives() {
    let lib = Lib::new("mass");
    for i in 0..10 {
        lib.write(&format!("book-{i}.pdf"), format!("%PDF-{i}").as_bytes());
    }
    lib.sync();

    for i in 0..5 {
        lib.remove(&format!("book-{i}.pdf"));
    }
    let a = lib.sync();
    assert!(matches!(a.declined, Some(Declined::MassDeletion { .. })));
    assert_eq!(a.missing, Vec::<String>::new());
    assert_eq!(lib.docs_on_disk().len(), 10, "nothing retracted");
}

#[test]
fn editing_a_file_re_ingests_under_the_same_id() {
    let lib = Lib::new("edit");
    lib.write("notes.pdf", b"%PDF-one");
    let doc = lib.sync().queued.pop().expect("a document");

    // rewrite with different bytes and a later mtime
    std::thread::sleep(std::time::Duration::from_millis(1100));
    lib.write("notes.pdf", b"%PDF-one-and-then-some-more");
    let a = lib.sync();
    assert_eq!(a.queued, vec![doc], "same document, re-ingested");
    assert_eq!(a.moved, 0);
}

#[test]
fn unsupported_and_hidden_files_are_not_documents() {
    let lib = Lib::new("skip");
    lib.write("book.pdf", b"%PDF-");
    lib.write("notes.txt", b"just a note");
    lib.write("draft.docx", b"zip");
    lib.write(".DS_Store", b"junk");

    let a = lib.sync();
    assert_eq!(a.queued.len(), 1);
    assert_eq!(lib.docs_on_disk().len(), 1);
}

#[test]
fn the_first_linked_folder_becomes_the_drop_target() {
    let lib = Lib::new("default");
    assert!(lib.ctx.default_root().is_some());
    assert_eq!(lib.ctx.default_root().unwrap().id, lib.root.id);

    let other = lib.dir.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let second = lib.ctx.add_root(&other, 2).unwrap();
    assert!(!second.is_default, "linking never steals the drop target");

    lib.ctx.set_default_root(&second.id).unwrap();
    assert_eq!(lib.ctx.default_root().unwrap().id, second.id);
    assert_eq!(lib.ctx.roots().len(), 2);

    // linking the same folder twice is a no-op, not a second root
    let again = lib.ctx.add_root(&other, 3).unwrap();
    assert_eq!(again.id, second.id);
    assert_eq!(lib.ctx.roots().len(), 2);
}

#[test]
fn a_documents_path_resolves_back_to_the_file() {
    let lib = Lib::new("resolve");
    lib.write("Cookbooks/artusi.pdf", b"%PDF-");
    let doc = lib.sync().queued.pop().expect("a document");

    let path = lib.ctx.doc_path(&doc).expect("a path");
    assert!(path.ends_with("library/Cookbooks/artusi.pdf"));
    assert!(path.exists(), "reveal-in-Finder needs a real path");
}
