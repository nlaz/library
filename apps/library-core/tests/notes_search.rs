//! Wire shaping of note-box hits through the full answer() path: reserved
//! docs rank like text but reach the client as kind "card" with their
//! page-scan assumptions stripped (mark-cards carrying the real doc/page
//! of their first anchor), and they never leak into collection- or
//! doc-scoped queries (member sets hold real doc ids only — pinned here
//! as intended behavior).

use library_core::meta::Ctx;
use library_core::notes::{AnchorKind, NewCard, QuoteAnchor, create_card};
use library_core::{
    ChunkKey, ChunkRec, EMB_DIM, Emb, Images, Library, Query, Word, answer, commit_chunks, open,
    open_images,
};

fn embed(_: &str) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[0] = 1.0;
    e
}

fn fixture(name: &str) -> (Library, Images, Ctx) {
    let dir = std::env::temp_dir().join(format!("library-core-noteswire-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    (
        open(dir.join("library.db")),
        open_images(dir.join("images.db")),
        Ctx::in_memory(&dir).expect("meta"),
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

/// A library-wide query in one of the popover's Cmd+F kinds.
fn kind_query(q: &str, kind: &str) -> Query {
    serde_json::from_value(serde_json::json!({
        "seq": 1,
        "q": q,
        "mode": "full",
        "kind": kind,
    }))
    .expect("query json")
}

#[test]
fn reserved_hits_are_shaped_and_scoped() {
    let (mut lib, images, ctx) = fixture("shape");

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
        &ctx,
        NewCard {
            title: "escapement is the governor".into(),
            body: String::new(),
            evidence: vec![],
            links: vec![],
        },
        &embed,
    )
    .expect("create card");
    // a mark-card: born from the reader with an anchored page
    let mark = create_card(
        &mut lib,
        &ctx,
        NewCard {
            title: "escapement sketch compare".into(),
            body: String::new(),
            evidence: vec![QuoteAnchor {
                doc: "moxon".into(),
                page: 12,
                kind: AnchorKind::Region {
                    bbox: [0.25, 0.25, 0.5, 0.25],
                },
            }],
            links: vec![],
        },
        &embed,
    )
    .expect("create mark card");
    ctx.collect("shelf", "moxon").expect("collections");

    // library-wide: both kinds surface, reserved ones decorated
    let r = answer(&lib, &images, &ctx, &query("escapement", "", ""), |_| None);
    let kinds: Vec<&str> = r.hits.iter().map(|h| h.kind).collect();
    assert!(kinds.contains(&"text") && kinds.contains(&"card"));

    let c = r
        .hits
        .iter()
        .find(|h| h.kind == "card" && h.doc.ends_with(&card.id))
        .expect("card hit");
    assert_eq!(c.img, "", "no /pages url for a synthetic doc");
    assert!(c.boxes.is_empty(), "zero-geometry boxes stripped");
    let meta = c.card.as_ref().expect("card meta");
    assert_eq!(meta.id, card.id);
    assert_eq!(meta.title, "escapement is the governor");
    assert!(!c.snippet.is_empty(), "snippet built from card words");
    assert!(
        meta.doc.is_none() && meta.page.is_none(),
        "no anchor, no reader jump"
    );

    // the mark-card carries its first anchor's real doc and page
    let m = r
        .hits
        .iter()
        .find(|h| h.kind == "card" && h.doc.ends_with(&mark.id))
        .expect("mark-card hit");
    let mmeta = m.card.as_ref().expect("mark-card meta");
    assert_eq!(mmeta.doc.as_deref(), Some("moxon"));
    assert_eq!(mmeta.page, Some(12));

    // wire shape: absent metas are absent keys, not nulls
    let t = r.hits.iter().find(|h| h.kind == "text").expect("text hit");
    let tj = serde_json::to_value(t).expect("json");
    assert!(tj.get("card").is_none() && tj.get("annot").is_none());
    let cj = serde_json::to_value(c).expect("json");
    assert!(cj.get("card").is_some());
    assert!(cj["card"].get("doc").is_none(), "absent, not null");
    // the threads layer is gone from the wire — pin the keys out
    for dead in ["address", "thread", "breadcrumb"] {
        assert!(cj["card"].get(dead).is_none(), "{dead} retired from wire");
    }
    let mj = serde_json::to_value(m).expect("json");
    assert_eq!(mj["card"]["doc"], "moxon");
    assert_eq!(mj["card"]["page"], 12);

    // collection scope: cards are not on shelves
    let r = answer(
        &lib,
        &images,
        &ctx,
        &query("escapement", "shelf", ""),
        |_| None,
    );
    assert!(r.hits.iter().all(|h| h.kind == "text"));

    // doc-scoped find: reader find never sees reserved hits
    let r = answer(
        &lib,
        &images,
        &ctx,
        &query("escapement", "", "moxon"),
        |_| None,
    );
    assert!(r.hits.iter().all(|h| h.kind == "text" && h.doc == "moxon"));

    // --- the Cmd+F kind cycle, through the same path -----------------------

    // "text" is text *and* notes: the whole text index, figures withheld.
    // Cards keep their decoration — the mode is about figures, not cards.
    let r = answer(
        &lib,
        &images,
        &ctx,
        &kind_query("escapement", "text"),
        |_| None,
    );
    let kinds: Vec<&str> = r.hits.iter().map(|h| h.kind).collect();
    assert!(kinds.contains(&"text") && kinds.contains(&"card"));
    assert!(!kinds.contains(&"image"));
    assert!(
        r.hits
            .iter()
            .filter(|h| h.kind == "card")
            .all(|h| h.card.is_some() && h.img.is_empty())
    );

    // "images": the text track does not run at all, so a figure-less
    // library answers nothing rather than falling back to page text
    let r = answer(
        &lib,
        &images,
        &ctx,
        &kind_query("escapement", "images"),
        |_| None,
    );
    assert!(r.hits.is_empty());

    // "all" (and the empty default) keep the blend
    for k in ["", "all"] {
        let r = answer(&lib, &images, &ctx, &kind_query("escapement", k), |_| None);
        let kinds: Vec<&str> = r.hits.iter().map(|h| h.kind).collect();
        assert!(
            kinds.contains(&"text") && kinds.contains(&"card"),
            "kind={k}"
        );
    }
}
