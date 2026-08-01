//! Cards and annotations as search citizens: save → findable, edit →
//! stale terms retracted, filed/deleted → gone. Hand-built embeddings —
//! the bucket is the first word of the text, so same-topic fixtures land
//! in the same bucket and everything is deterministic.

use library_core::annots::{AnnotKind, AnnotRec, annot_doc, store_annots};
use library_core::meta::Ctx;
use library_core::notes::{AnchorKind, NewCard, QuoteAnchor, create_card, load_cards, update_card};
use library_core::{ChunkKey, ChunkRec, EMB_DIM, Emb, Library, Word, open, search};

fn one_hot(hot: usize) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[hot % EMB_DIM] = 1.0;
    e
}

/// Bucket by first word: identical first words are exact neighbors.
fn embed(text: &str) -> Emb {
    let bucket = text
        .split_whitespace()
        .next()
        .map(|w| w.bytes().map(usize::from).sum())
        .unwrap_or(0);
    one_hot(bucket)
}

fn fixture(name: &str) -> (Library, Ctx) {
    let dir = std::env::temp_dir().join(format!("library-core-notebox-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let lib = open(dir.join("library.db"));
    (lib, Ctx::in_memory(&dir).expect("meta"))
}

/// Docs of the lexical hits for `query`, in rank order.
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

fn new_card(title: &str) -> NewCard {
    NewCard {
        title: title.to_string(),
        body: String::new(),
        evidence: vec![],
        links: vec![],
    }
}

/// One real page chunk, so a card has page text to be ranked against
/// (cards are indexed alongside page text in the one text index).
fn commit_page_chunk(lib: &mut Library, doc: &str, page: u32, text: &str) {
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
    let chunk = ChunkRec {
        key: ChunkKey {
            doc: doc.to_string(),
            page,
            idx: 0,
        },
        words,
        emb: embed(text),
    };
    library_core::store::commit_chunks(lib, doc, &[chunk]);
}

/// What the old pen's save path used to index for a noted mark: one
/// zero-geometry chunk under `~annot/<id>` — seeded directly so the
/// migration tests can watch it retract.
fn commit_legacy_annot_chunk(lib: &mut Library, id: &str, text: &str) {
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
    let chunk = ChunkRec {
        key: ChunkKey {
            doc: annot_doc(id),
            page: 0,
            idx: 0,
        },
        words,
        emb: embed(text),
    };
    library_core::store::commit_chunks(lib, &annot_doc(id), &[chunk]);
}

#[test]
fn card_lifecycle_in_search() {
    let (mut lib, ctx) = fixture("card-life");

    let mut input = new_card("casting speed is a boast");
    input.evidence.push(QuoteAnchor {
        doc: "moxon".into(),
        page: 215,
        kind: AnchorKind::Text {
            w0: 10,
            w1: 16,
            text: "an hundred and twenty in the hour".into(),
            boxes: vec![[0.1, 0.4, 0.5, 0.02]],
        },
    });
    let card = create_card(&mut lib, &ctx, input, &embed).unwrap();

    // findable by claim AND by quoted evidence, under the reserved doc
    let doc = format!("~card/{}", card.id);
    assert_eq!(lexical_docs(&lib, "boast"), vec![doc.clone()]);
    assert_eq!(lexical_docs(&lib, "hundred"), vec![doc.clone()]);

    // edit: stale terms retracted, identity immutable
    let mut edit = card.clone();
    edit.title = "casting speed is a ceiling".into();
    edit.created = 999_999;
    let saved = update_card(&mut lib, &ctx, edit, &embed).unwrap();
    assert_eq!(saved.id, card.id);
    assert_eq!(saved.created, card.created, "created stamp is immutable");
    assert!(lexical_docs(&lib, "boast").is_empty());
    assert_eq!(lexical_docs(&lib, "ceiling"), vec![doc.clone()]);

    // filing retracts from search but keeps the record
    let mut filed = saved.clone();
    filed.filed = true;
    update_card(&mut lib, &ctx, filed, &embed).unwrap();
    assert!(lexical_docs(&lib, "ceiling").is_empty());
    assert!(load_cards(&ctx).iter().any(|c| c.id == card.id && c.filed));

    // and unfiling brings it back
    let mut back = load_cards(&ctx)
        .into_iter()
        .find(|c| c.id == card.id)
        .unwrap();
    back.filed = false;
    update_card(&mut lib, &ctx, back, &embed).unwrap();
    assert_eq!(lexical_docs(&lib, "ceiling"), vec![doc]);
}

/// A card outranks a page chunk carrying the same lexical evidence — the
/// NOTE_BOOST end to end. Identical text on both sides, so nothing but the
/// boost can order them (and without it the `~` doc id would sort last).
#[test]
fn cards_outrank_pages_on_equal_evidence() {
    let (mut lib, ctx) = fixture("note-boost");
    let text = "gear train sketch";
    commit_page_chunk(&mut lib, "moxon", 215, text);
    let card = create_card(&mut lib, &ctx, new_card(text), &embed).unwrap();

    assert_eq!(
        lexical_docs(&lib, "gear train"),
        vec![format!("~card/{}", card.id), "moxon".to_string()]
    );
}

#[test]
fn card_births_append_to_the_timeline() {
    let (mut lib, ctx) = fixture("card-birth");
    let first = create_card(&mut lib, &ctx, new_card("gears one"), &embed).unwrap();
    let second = create_card(&mut lib, &ctx, new_card("gears two"), &embed).unwrap();
    let third = create_card(&mut lib, &ctx, new_card("cooking stock"), &embed).unwrap();

    // fresh unique ids, appended in birth order
    let ids: Vec<String> = load_cards(&ctx).into_iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![first.id, second.id, third.id]);
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        3
    );

    // editing an unknown card is an input error
    let mut ghost = create_card(&mut lib, &ctx, new_card("gears ghost"), &embed).unwrap();
    ghost.id = "c000000000000".into();
    assert!(update_card(&mut lib, &ctx, ghost, &embed).is_err());
}

#[test]
fn migration_moves_noted_marks_into_cards() {
    use library_core::annots::migrate_annots_to_cards;

    let (mut lib, ctx) = fixture("annot-migrate");

    // seed what the old pen left behind: two noted marks + a bare
    // highlight on "moxon", one noted mark on "fournier" — noted ones
    // indexed under ~annot/
    let mk = |id: &str, page: u32, y: f32, note: &str| AnnotRec {
        id: id.into(),
        doc: String::new(),
        page,
        kind: AnnotKind::Text {
            w0: 5,
            w1: 9,
            text: "rubs and dresses the same".into(),
            boxes: vec![[0.1, y, 0.5, 0.02]],
        },
        note: note.into(),
        created: 7,
    };
    let mut late = mk("a1", 9, 0.3, "plantin built the hinge");
    late.doc = "moxon".into();
    let mut early = mk("a2", 2, 0.5, "compare the day-book");
    early.doc = "moxon".into();
    let mut bare = mk("a3", 4, 0.1, "");
    bare.doc = "moxon".into();
    let mut other = mk("a4", 1, 0.1, "fournier measured the foot");
    other.doc = "fournier".into();
    // stored out of reading order on purpose
    store_annots(
        &ctx.data,
        "moxon",
        &[late.clone(), early.clone(), bare.clone()],
    )
    .unwrap();
    store_annots(&ctx.data, "fournier", &[other.clone()]).unwrap();
    for a in [&late, &early, &other] {
        commit_legacy_annot_chunk(&mut lib, &a.id, &a.note);
    }
    assert_eq!(lexical_docs(&lib, "plantin").len(), 1);
    let sidecar_bytes = std::fs::read(ctx.data.join("annotations").join("moxon.json")).unwrap();

    let n = migrate_annots_to_cards(&mut lib, &ctx, &embed).unwrap();
    assert_eq!(n, 3);

    // noted marks only, reading order within each doc, stamps kept
    let cards = load_cards(&ctx);
    assert_eq!(cards.len(), 3);
    let moxon_titles: Vec<&str> = cards
        .iter()
        .filter(|c| c.evidence.iter().any(|q| q.doc == "moxon"))
        .map(|c| c.title.as_str())
        .collect();
    assert_eq!(
        moxon_titles,
        vec!["compare the day-book", "plantin built the hinge"]
    );
    assert_eq!(
        cards
            .iter()
            .filter(|c| c.evidence.iter().any(|q| q.doc == "fournier"))
            .count(),
        1
    );
    assert!(cards.iter().all(|c| c.created == 7));

    // notes findable under ~card/, the ~annot/ namespace is empty
    let hits = lexical_docs(&lib, "plantin");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].starts_with("~card/"));
    assert!(
        lexical_docs(&lib, "compare")
            .iter()
            .all(|d| d.starts_with("~card/"))
    );

    // the sidecar files were left byte-for-byte alone
    assert_eq!(
        std::fs::read(ctx.data.join("annotations").join("moxon.json")).unwrap(),
        sidecar_bytes
    );

    // second run is a no-op
    assert_eq!(migrate_annots_to_cards(&mut lib, &ctx, &embed).unwrap(), 0);
    assert_eq!(load_cards(&ctx).len(), 3);
}
