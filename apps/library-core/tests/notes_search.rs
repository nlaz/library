//! Wire shaping of note-box hits through the full answer() path: reserved
//! docs rank like text but reach the client as kind "card"/"annotation"
//! with their page-scan assumptions stripped, and they never leak into
//! collection- or doc-scoped queries (member sets hold real doc ids only
//! — pinned here as intended behavior).

use std::path::PathBuf;

use library_core::annots::{AnnotKind, AnnotRec, save_annot};
use library_core::notes::{NewCard, create_card};
use library_core::{
    ChunkKey, ChunkRec, EMB_DIM, Emb, Images, Library, Query, Word, answer, commit_chunks, open,
    open_images,
};

fn embed(_: &str) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[0] = 1.0;
    e
}

fn fixture(name: &str) -> (Library, Images, PathBuf) {
    let dir = std::env::temp_dir().join(format!("library-core-noteswire-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    (
        open(dir.join("library.db")),
        open_images(dir.join("images.db")),
        dir,
    )
}

fn page_chunk(doc: &str, page: u32, text: &str) -> ChunkRec {
    let words = text
        .split_whitespace()
        .map(|t| Word {
            t: t.to_string(),
            x: 0.1,
            y: 0.2,
            w: 0.05,
            h: 0.02,
        })
        .collect();
    ChunkRec {
        key: ChunkKey {
            doc: doc.to_string(),
            page,
            idx: 0,
        },
        words,
        emb: embed(text),
    }
}

fn query(q: &str, col: &str, doc: &str) -> Query {
    serde_json::from_value(serde_json::json!({
        "seq": 1,
        "q": q,
        "mode": "instant",
        "col": col,
        "doc": doc,
    }))
    .expect("query json")
}

#[test]
fn reserved_hits_are_shaped_and_scoped() {
    let (mut lib, images, data) = fixture("shape");

    commit_chunks(
        &mut lib,
        "moxon",
        &[page_chunk(
            "moxon",
            215,
            "the escapement regulates the wheel",
        )],
    );
    let card = create_card(
        &mut lib,
        &data,
        NewCard {
            title: "escapement is the governor".into(),
            body: String::new(),
            evidence: vec![],
            links: vec![],
            parent: None,
            thread: None,
        },
        &embed,
    )
    .expect("create card");
    let annot = save_annot(
        &mut lib,
        &data,
        AnnotRec {
            id: String::new(),
            doc: "moxon".into(),
            page: 12,
            kind: AnnotKind::Region {
                bbox: [0.25, 0.25, 0.5, 0.25],
            },
            note: "escapement sketch compare".into(),
            created: 0,
        },
        &embed,
    )
    .expect("save annot");
    library_core::sidecar::write_json_atomic(
        &data.join("collections.json"),
        &serde_json::json!({ "shelf": ["moxon"] }),
    )
    .expect("collections sidecar");

    // library-wide: all three kinds surface, reserved ones decorated
    let r = answer(&lib, &images, &data, &query("escapement", "", ""), |_| None);
    let kinds: Vec<&str> = r.hits.iter().map(|h| h.kind).collect();
    assert!(kinds.contains(&"text") && kinds.contains(&"card") && kinds.contains(&"annotation"));

    let c = r.hits.iter().find(|h| h.kind == "card").expect("card hit");
    assert_eq!(c.img, "", "no /pages url for a synthetic doc");
    assert!(c.boxes.is_empty(), "zero-geometry boxes stripped");
    let meta = c.card.as_ref().expect("card meta");
    assert_eq!(meta.id, card.id);
    assert_eq!(meta.address, "1/1");
    assert_eq!(meta.thread, 1);
    assert_eq!(meta.breadcrumb, "1 · escapement is the governor");
    assert!(!c.snippet.is_empty(), "snippet built from card words");

    let a = r
        .hits
        .iter()
        .find(|h| h.kind == "annotation")
        .expect("annotation hit");
    assert_eq!(a.img, "");
    let ameta = a.annot.as_ref().expect("annot meta");
    assert_eq!((ameta.doc.as_str(), ameta.page), ("moxon", 12));
    assert_eq!(ameta.id, annot.id);

    // wire shape: absent metas are absent keys, not nulls
    let t = r.hits.iter().find(|h| h.kind == "text").expect("text hit");
    let tj = serde_json::to_value(t).expect("json");
    assert!(tj.get("card").is_none() && tj.get("annot").is_none());
    let cj = serde_json::to_value(c).expect("json");
    assert!(cj.get("card").is_some() && cj.get("annot").is_none());

    // collection scope: cards are not on shelves
    let r = answer(
        &lib,
        &images,
        &data,
        &query("escapement", "shelf", ""),
        |_| None,
    );
    assert!(r.hits.iter().all(|h| h.kind == "text"));

    // doc-scoped find: reader find never sees reserved hits
    let r = answer(
        &lib,
        &images,
        &data,
        &query("escapement", "", "moxon"),
        |_| None,
    );
    assert!(r.hits.iter().all(|h| h.kind == "text" && h.doc == "moxon"));
}
