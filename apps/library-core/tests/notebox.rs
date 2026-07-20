//! Cards and annotations as search citizens: save → findable, edit →
//! stale terms retracted, filed/deleted → gone, plus the embedding
//! neighborhood used by the thread rail. Hand-built embeddings — the
//! bucket is the first word of the text, so same-topic fixtures are
//! exact neighbors and everything is deterministic.

use std::path::PathBuf;

use library_core::annots::{AnnotKind, AnnotRec, delete_annot, load_annots, save_annot};
use library_core::notes::{
    NewCard, QuoteAnchor, card_neighbors, create_card, load_cards, propose_thread, update_card,
};
use library_core::{EMB_DIM, Emb, Library, open, search};

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

#[test]
fn card_lifecycle_in_search() {
    let (mut lib, data) = fixture("card-life");

    let mut input = new_card("casting speed is a boast");
    input.evidence.push(QuoteAnchor {
        doc: "moxon".into(),
        page: 215,
        w0: 10,
        w1: 16,
        text: "an hundred and twenty in the hour".into(),
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
fn annotation_notes_are_the_searchable_part() {
    let (mut lib, data) = fixture("annot-life");

    // bare highlight: sidecar yes, search no
    let bare = save_annot(
        &mut lib,
        &data,
        AnnotRec {
            id: String::new(),
            doc: "moxon".into(),
            page: 3,
            kind: AnnotKind::Text {
                w0: 5,
                w1: 9,
                text: "rubs and dresses the same".into(),
                boxes: vec![[0.1, 0.2, 0.5, 0.02]],
            },
            note: String::new(),
            created: 0,
        },
        &embed,
    )
    .unwrap();
    assert!(bare.id.starts_with('a'));
    assert!(lexical_docs(&lib, "dresses").is_empty());

    // adding a note makes it findable — by the note and the snapshot
    let mut noted = bare.clone();
    noted.note = "compare plantin hinge".into();
    save_annot(&mut lib, &data, noted, &embed).unwrap();
    let doc = format!("~annot/{}", bare.id);
    assert_eq!(lexical_docs(&lib, "plantin"), vec![doc.clone()]);
    assert_eq!(lexical_docs(&lib, "dresses"), vec![doc.clone()]);

    // region with note indexes; wiping the note retracts
    let region = save_annot(
        &mut lib,
        &data,
        AnnotRec {
            id: String::new(),
            doc: "moxon".into(),
            page: 4,
            kind: AnnotKind::Region {
                bbox: [0.25, 0.25, 0.5, 0.25],
            },
            note: "the hand mold opened".into(),
            created: 0,
        },
        &embed,
    )
    .unwrap();
    assert_eq!(
        lexical_docs(&lib, "mold"),
        vec![format!("~annot/{}", region.id)]
    );
    let mut wiped = region.clone();
    wiped.note = String::new();
    save_annot(&mut lib, &data, wiped, &embed).unwrap();
    assert!(lexical_docs(&lib, "mold").is_empty());

    // delete removes sidecar row and index entry
    delete_annot(&mut lib, &data, "moxon", &bare.id).unwrap();
    assert!(lexical_docs(&lib, "plantin").is_empty());
    assert_eq!(load_annots(&data, "moxon").len(), 1);
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

    // an annotation in the same embedding bucket must never appear
    save_annot(
        &mut lib,
        &data,
        AnnotRec {
            id: String::new(),
            doc: "moxon".into(),
            page: 1,
            kind: AnnotKind::Region {
                bbox: [0.0, 0.0, 1.0, 1.0],
            },
            note: "gears note".into(),
            created: 0,
        },
        &embed,
    )
    .unwrap();

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
