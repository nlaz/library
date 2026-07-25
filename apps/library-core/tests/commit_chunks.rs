//! commit_chunks against a real temp-dir store: the manifest-scoped diff
//! must upsert new chunks, retract vanished keys, and treat `&[]` as full
//! retraction — the contract both ingest and the note box rely on.

use library_core::{ChunkKey, ChunkRec, EMB_DIM, Emb, Library, Word, commit_chunks, open, search};

fn key(doc: &str, idx: u32) -> ChunkKey {
    ChunkKey {
        doc: doc.to_string(),
        page: 0,
        idx,
    }
}

fn one_hot(hot: usize) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[hot % EMB_DIM] = 1.0;
    e
}

fn chunk(doc: &str, idx: u32, text: &str) -> ChunkRec {
    let words = text
        .split_whitespace()
        .map(|t| Word {
            t: t.to_string(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        })
        .collect();
    ChunkRec {
        key: key(doc, idx),
        words,
        emb: one_hot(idx as usize),
    }
}

fn fresh(name: &str) -> Library {
    let dir = std::env::temp_dir().join(format!("library-core-commit-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    open(dir)
}

fn manifest_keys(lib: &Library, doc: &str) -> Vec<ChunkKey> {
    let mut keys = lib.rtx(|(_, (manifest, _))| manifest.search(&doc.to_string()));
    keys.sort();
    keys
}

fn lexical_docs(lib: &Library, query: &str) -> Vec<String> {
    lib.rtx(|r| {
        search(
            &r,
            query,
            None,
            10,
            None,
            false,
            false,
            false,
            |k| lib.get(k),
            None,
        )
    })
    .into_iter()
    .map(|h| h.key.doc)
    .collect()
}

#[test]
fn upsert_diff_and_full_retract() {
    let mut lib = fresh("diff");

    let (removed, added) = commit_chunks(
        &mut lib,
        "doc",
        &[
            chunk("doc", 0, "brass astrolabe rete"),
            chunk("doc", 1, "mater and tympan plates"),
        ],
    );
    assert_eq!((removed, added), (0, 2));
    assert_eq!(
        manifest_keys(&lib, "doc"),
        vec![key("doc", 0), key("doc", 1)]
    );
    assert_eq!(lexical_docs(&lib, "astrolabe"), vec!["doc"]);

    // shrink to one chunk: the vanished key is retracted everywhere
    let (removed, added) = commit_chunks(
        &mut lib,
        "doc",
        &[chunk("doc", 1, "mater and tympan plates")],
    );
    assert_eq!((removed, added), (1, 1));
    assert_eq!(manifest_keys(&lib, "doc"), vec![key("doc", 1)]);
    assert!(lexical_docs(&lib, "astrolabe").is_empty());
    assert_eq!(lexical_docs(&lib, "tympan"), vec!["doc"]);

    // re-commit unchanged: nothing removed, still searchable
    let (removed, _) = commit_chunks(
        &mut lib,
        "doc",
        &[chunk("doc", 1, "mater and tympan plates")],
    );
    assert_eq!(removed, 0);
    assert_eq!(lexical_docs(&lib, "tympan"), vec!["doc"]);

    // edit in place under the same key: stale terms retracted
    commit_chunks(&mut lib, "doc", &[chunk("doc", 1, "alidade sights")]);
    assert!(lexical_docs(&lib, "tympan").is_empty());
    assert_eq!(lexical_docs(&lib, "alidade"), vec!["doc"]);

    // `&[]` retracts the doc entirely, other docs untouched
    commit_chunks(&mut lib, "other", &[chunk("other", 0, "quadrant scale")]);
    let (removed, added) = commit_chunks(&mut lib, "doc", &[]);
    assert_eq!((removed, added), (1, 0));
    assert!(manifest_keys(&lib, "doc").is_empty());
    assert!(lexical_docs(&lib, "alidade").is_empty());
    assert_eq!(lexical_docs(&lib, "quadrant"), vec!["other"]);
}
