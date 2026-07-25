//! Cards and annotations as search citizens: save → findable, edit →
//! stale terms retracted, filed/deleted → gone, plus the embedding
//! neighborhood used by the thread rail. Hand-built embeddings — the
//! bucket is the first word of the text, so same-topic fixtures are
//! exact neighbors and everything is deterministic.

use std::path::PathBuf;

use library_core::annots::{AnnotKind, AnnotRec, annot_doc, store_annots};
use library_core::notes::{
    AnchorKind, NewCard, QuoteAnchor, card_neighbors, create_card, load_cards, propose_thread,
    update_card,
};
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

fn fixture(name: &str) -> (Library, PathBuf) {
    let dir = std::env::temp_dir().join(format!("library-core-notebox-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    (open(dir.join("library.db")), dir)
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

fn new_card(title: &str) -> NewCard {
    NewCard {
        title: title.to_string(),
        body: String::new(),
        evidence: vec![],
        links: vec![],
        parent: None,
        thread: None,
    }
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
    let (mut lib, data) = fixture("card-life");

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
    let card = create_card(&mut lib, &data, input, &embed).unwrap();
    assert_eq!((card.thread, card.addr.as_slice()), (1, &[1u32][..]));

    // findable by claim AND by quoted evidence, under the reserved doc
    let doc = format!("~card/{}", card.id);
    assert_eq!(lexical_docs(&lib, "boast"), vec![doc.clone()]);
    assert_eq!(lexical_docs(&lib, "hundred"), vec![doc.clone()]);

    // edit: stale terms retracted, identity and address immutable
    let mut edit = card.clone();
    edit.title = "casting speed is a ceiling".into();
    edit.addr = vec![9, 9];
    edit.thread = 42;
    let saved = update_card(&mut lib, &data, edit, &embed).unwrap();
    assert_eq!((saved.thread, saved.addr.as_slice()), (1, &[1u32][..]));
    assert!(lexical_docs(&lib, "boast").is_empty());
    assert_eq!(lexical_docs(&lib, "ceiling"), vec![doc.clone()]);

    // filing retracts from search but keeps the record
    let mut filed = saved.clone();
    filed.filed = true;
    update_card(&mut lib, &data, filed, &embed).unwrap();
    assert!(lexical_docs(&lib, "ceiling").is_empty());
    assert!(load_cards(&data).iter().any(|c| c.id == card.id && c.filed));

    // and unfiling brings it back
    let mut back = load_cards(&data)
        .into_iter()
        .find(|c| c.id == card.id)
        .unwrap();
    back.filed = false;
    update_card(&mut lib, &data, back, &embed).unwrap();
    assert_eq!(lexical_docs(&lib, "ceiling"), vec![doc]);
}

#[test]
fn card_births_follow_parent_and_thread() {
    let (mut lib, data) = fixture("card-birth");
    let trunk = create_card(&mut lib, &data, new_card("gears one"), &embed).unwrap();
    // explicit thread append
    let mut t = new_card("gears two");
    t.thread = Some(trunk.thread);
    let second = create_card(&mut lib, &data, t, &embed).unwrap();
    assert_eq!(second.addr, vec![2]);
    // branch under the first
    let mut b = new_card("gears aside");
    b.parent = Some(trunk.id.clone());
    let branch = create_card(&mut lib, &data, b, &embed).unwrap();
    assert_eq!(
        (branch.thread, branch.addr.as_slice()),
        (trunk.thread, &[1u32, 1][..])
    );
    // no context = fresh thread
    let fresh = create_card(&mut lib, &data, new_card("cooking stock"), &embed).unwrap();
    assert_eq!((fresh.thread, fresh.addr.as_slice()), (2, &[1u32][..]));
    // unknown parent is an input error
    let mut bad = new_card("orphan");
    bad.parent = Some("c000000000000".into());
    assert!(create_card(&mut lib, &data, bad, &embed).is_err());
}

#[test]
fn migration_moves_noted_marks_into_cards() {
    use library_core::annots::migrate_annots_to_cards;

    let (mut lib, data) = fixture("annot-migrate");

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
    store_annots(&data, "moxon", &[late.clone(), early.clone(), bare.clone()]).unwrap();
    store_annots(&data, "fournier", &[other.clone()]).unwrap();
    for a in [&late, &early, &other] {
        commit_legacy_annot_chunk(&mut lib, &a.id, &a.note);
    }
    assert_eq!(lexical_docs(&lib, "plantin").len(), 1);
    let sidecar_bytes = std::fs::read(data.join("annotations").join("moxon.json")).unwrap();

    let n = migrate_annots_to_cards(&mut lib, &data, &embed).unwrap();
    assert_eq!(n, 3);

    // one fresh thread per doc, trunk cards in reading order, stamps kept
    let cards = load_cards(&data);
    assert_eq!(cards.len(), 3);
    let by_doc = |d: &str| -> Vec<&library_core::notes::CardRec> {
        cards
            .iter()
            .filter(|c| c.evidence.iter().any(|q| q.doc == d))
            .collect()
    };
    let moxon = by_doc("moxon");
    let fournier = by_doc("fournier");
    assert_eq!(moxon.len(), 2);
    assert_eq!(fournier.len(), 1);
    assert_ne!(moxon[0].thread, fournier[0].thread);
    assert_eq!(moxon[0].thread, moxon[1].thread);
    let titles: Vec<&str> = moxon.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["compare the day-book", "plantin built the hinge"]
    );
    assert_eq!(
        (moxon[0].addr.as_slice(), moxon[1].addr.as_slice()),
        (&[1u32][..], &[2u32][..])
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
        std::fs::read(data.join("annotations").join("moxon.json")).unwrap(),
        sidecar_bytes
    );

    // second run is a no-op
    assert_eq!(migrate_annots_to_cards(&mut lib, &data, &embed).unwrap(), 0);
    assert_eq!(load_cards(&data).len(), 3);
}

#[test]
fn neighbors_and_proposals_stay_in_the_card_namespace() {
    let (mut lib, data) = fixture("neighbors");

    let a = create_card(&mut lib, &data, new_card("gears mesh finely"), &embed).unwrap();
    let mut in_thread = new_card("gears wear down");
    in_thread.thread = Some(a.thread);
    let b = create_card(&mut lib, &data, in_thread, &embed).unwrap();
    let mut linked = new_card("gears sing");
    linked.thread = Some(a.thread);
    linked.links.push(library_core::notes::CardLink {
        to: a.id.clone(),
        kind: library_core::notes::LinkKind::Relates,
    });
    let c = create_card(&mut lib, &data, linked, &embed).unwrap();
    let far = create_card(&mut lib, &data, new_card("cooking stock"), &embed).unwrap();

    // a stray reserved chunk (a not-yet-migrated legacy mark) in the
    // same embedding bucket must never appear as a card neighbor
    commit_legacy_annot_chunk(&mut lib, "a9", "gears note");

    let n = card_neighbors(&lib, &data, &a.id, 8);
    let ids: Vec<&str> = n.iter().map(|x| x.id.as_str()).collect();
    assert!(
        ids.contains(&b.id.as_str()),
        "unlinked same-bucket card is a neighbor"
    );
    assert!(!ids.contains(&a.id.as_str()), "self excluded");
    assert!(!ids.contains(&c.id.as_str()), "linked (incoming) excluded");
    assert!(n.iter().all(|x| !x.id.is_empty() && !x.address.is_empty()));

    // proposal files the new text after its nearest card
    let p = propose_thread(&lib, &data, &embed("gears everywhere")).unwrap();
    assert_eq!(p.thread, a.thread);
    assert!([a.id.as_str(), b.id.as_str(), c.id.as_str()].contains(&p.parent.as_str()));
    assert!(p.address.starts_with(&format!("{}/", a.thread)));

    // filed cards have no neighborhood
    let mut filed = load_cards(&data)
        .into_iter()
        .find(|x| x.id == far.id)
        .unwrap();
    filed.filed = true;
    update_card(&mut lib, &data, filed, &embed).unwrap();
    assert!(card_neighbors(&lib, &data, &far.id, 8).is_empty());
}
