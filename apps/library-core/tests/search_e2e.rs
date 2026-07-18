//! End-to-end tests of the full search path — fuzzy correction → lexical
//! BM25 → RRF fusion → MMR diversity → resolve — against a real temp-dir
//! store populated with synthetic chunks and hand-built embeddings. No
//! model runs here: `search` takes the query embedding as a parameter.

use library_core::{ChunkKey, ChunkRec, EMB_DIM, Emb, FxHashSet, Library, Word, open, search};

fn key(doc: &str, idx: u32) -> ChunkKey {
    ChunkKey {
        doc: doc.to_string(),
        page: 1,
        idx,
    }
}

fn one_hot(hot: usize) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[hot % EMB_DIM] = 1.0;
    e
}

fn chunk(doc: &str, idx: u32, text: &str, hot: usize) -> ChunkRec {
    let words = text
        .split_whitespace()
        .map(|t| Word {
            t: t.to_string(),
            x: 0.0,
            y: 0.0,
            w: 0.1,
            h: 0.1,
        })
        .collect();
    ChunkRec {
        key: key(doc, idx),
        words,
        emb: one_hot(hot),
    }
}

/// A small library with controlled vocabulary: two near-duplicate chunks
/// (same embedding direction, graded lexical strength), one novel chunk,
/// a typo-bait term, and an unrelated doc for filter tests.
fn synthetic_library(name: &str) -> Library {
    let dir = std::env::temp_dir().join(format!("library-core-e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let mut lib = open(dir);
    let chunks = [
        // near-duplicates: same direction (0), descending lexical strength
        chunk("watchmaking-a", 0, "gear gear gear train", 0),
        chunk("watchmaking-b", 0, "gear gear train", 0),
        // novel direction, weakest lexically
        chunk("watchmaking-c", 0, "gear ratio", 7),
        // typo bait: "escapement" is the only esc* term in the vocabulary
        chunk("watchmaking-a", 1, "the escapement regulates the wheel", 3),
        chunk("cooking", 0, "stock reduction demands patience", 11),
        chunk("cooking", 1, "a heavy pan holds gear for camp cooking", 12),
    ];
    lib.wtx(|tx| {
        for c in &chunks {
            tx.upsert(&c.key, c);
        }
    });
    lib
}

/// search() with this suite's fixed choices spelled out once.
fn run(
    lib: &Library,
    query: &str,
    qemb: Option<&Emb>,
    filter: Option<&FxHashSet<String>>,
    complete: bool,
    fuzzy: bool,
    diversify: bool,
) -> Vec<library_core::Hit> {
    lib.rtx(|r| {
        search(
            &r,
            query,
            qemb,
            10,
            filter,
            complete,
            fuzzy,
            diversify,
            |k| lib.get(k),
            None,
        )
    })
}

#[test]
fn end_to_end_lexical_search_ranks_exact_match_first() {
    let lib = synthetic_library("lexical");
    let hits = run(&lib, "escapement", None, None, false, false, false);
    assert!(!hits.is_empty());
    let top = &hits[0];
    assert_eq!(
        (top.key.doc.as_str(), top.key.page, top.key.idx),
        ("watchmaking-a", 1, 1)
    );
    // the top lexical hit defines rel = 1.0 and carries its BM25 evidence
    assert_eq!(top.rel, 1.0);
    assert!(top.bm25 > 0.0);
    assert_eq!(top.lex_rank, Some(0));
    assert_eq!(top.sem_rank, None); // no query embedding given
    // resolve populated the hit's words from the primary table
    assert!(top.words.iter().any(|w| w.t == "escapement"));
}

#[test]
fn fuzzy_correction_recovers_typo_query() {
    let lib = synthetic_library("fuzzy");
    // "escapment" (missing 'e') is not in the vocabulary
    let exact = run(&lib, "escapment", None, None, false, false, false);
    assert!(exact.is_empty(), "typo must not match without fuzzy");
    let fuzzy = run(&lib, "escapment", None, None, false, true, false);
    assert_eq!(fuzzy.len(), 1);
    assert_eq!(fuzzy[0].key.doc, "watchmaking-a");
    // a clean query is untouched by the fuzzy pass (exact terms expand nothing)
    let clean = run(&lib, "escapement", None, None, false, true, false);
    assert_eq!(clean[0].key, fuzzy[0].key);
}

#[test]
fn mmr_demotes_near_duplicate_full_path() {
    let lib = synthetic_library("mmr");
    let qemb = one_hot(0); // aligned with the duplicate pair
    let plain = run(
        &lib,
        "gear train ratio",
        Some(&qemb),
        None,
        false,
        false,
        false,
    );
    let order = |hits: &[library_core::Hit]| -> Vec<String> {
        hits.iter().map(|h| h.key.doc.clone()).collect()
    };
    // fused relevance order: the two near-duplicates lead
    assert_eq!(order(&plain)[..2], ["watchmaking-a", "watchmaking-b"]);
    let diverse = run(
        &lib,
        "gear train ratio",
        Some(&qemb),
        None,
        false,
        false,
        true,
    );
    // MMR keeps the best duplicate, promotes the novel chunk over the twin
    assert_eq!(order(&diverse)[0], "watchmaking-a");
    let pos =
        |hits: &[library_core::Hit], doc: &str| hits.iter().position(|h| h.key.doc == doc).unwrap();
    assert!(
        pos(&diverse, "watchmaking-c") < pos(&diverse, "watchmaking-b"),
        "novel chunk must outrank the near-duplicate under MMR: {:?}",
        order(&diverse)
    );
    // every hit survives — diversity reorders, never drops
    assert_eq!(plain.len(), diverse.len());
}

#[test]
fn filter_restricts_every_ranker() {
    let lib = synthetic_library("filter");
    let only_cooking: FxHashSet<String> = std::iter::once("cooking".to_string()).collect();
    let qemb = one_hot(0); // semantically nearest to watchmaking chunks
    let hits = run(
        &lib,
        "gear",
        Some(&qemb),
        Some(&only_cooking),
        false,
        false,
        false,
    );
    assert!(!hits.is_empty());
    assert!(
        hits.iter().all(|h| h.key.doc == "cooking"),
        "filter must bind lexical AND semantic rankers: {:?}",
        hits.iter().map(|h| &h.key.doc).collect::<Vec<_>>()
    );
}

#[test]
fn typeahead_complete_expands_last_token() {
    let lib = synthetic_library("typeahead");
    // mid-word: "escap" only matches via term-dict prefix expansion
    let without = run(&lib, "escap", None, None, false, false, false);
    assert!(without.is_empty());
    let with = run(&lib, "escap", None, None, true, false, false);
    assert_eq!(with.len(), 1);
    assert_eq!(with[0].key.doc, "watchmaking-a");
}

#[test]
fn empty_and_contentless_queries_return_nothing() {
    let lib = synthetic_library("empty");
    assert!(run(&lib, "", None, None, true, true, true).is_empty());
    // punctuation-only tokenizes to nothing
    assert!(run(&lib, "!?! …", None, None, true, true, true).is_empty());
}
