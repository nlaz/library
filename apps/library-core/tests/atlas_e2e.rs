//! End-to-end atlas build against a real temp-dir store: synthetic chunks
//! with hand-built embeddings (no model), a stub data dir for the doc
//! enumeration, and the sidecar cache round trip. Temp dirs only — never
//! the live `data/`.

use library_core::atlas;
use library_core::{ChunkKey, ChunkRec, EMB_DIM, Emb, Library, Word, open};
use std::path::PathBuf;

fn key(doc: &str, page: u32, idx: u32) -> ChunkKey {
    ChunkKey {
        doc: doc.to_string(),
        page,
        idx,
    }
}

/// A cluster member: base direction 0 plus two perturbation dims drawn
/// from a small pool (a "row" and a "column" of a 6×6 grid keyed by
/// `salt`). Chunks sharing a row or column sit closer (cos ≈ 0.94) than
/// unrelated members (≈0.89), so neighborhoods are graded and symmetric —
/// a compact blob like a real theme, not a chain that label propagation
/// would split into bands. Only exact copies (same salt) hit sim 1.0.
fn cluster_emb(salt: usize) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[0] = 1.0;
    e[10 + (salt % 6)] = 0.25;
    e[20 + (salt / 6) % 6] = 0.25;
    e
}

fn noise_emb(salt: usize) -> Emb {
    let mut e = [0.0f32; EMB_DIM];
    e[100 + (salt % 300)] = 1.0;
    e
}

fn chunk(doc: &str, page: u32, idx: u32, text: &str, emb: Emb) -> ChunkRec {
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
        key: key(doc, page, idx),
        words,
        emb,
    }
}

/// Store + data dir: four distinct works sharing one embedding cluster,
/// a `-2` physical copy, off-cluster noise, and one reserved card chunk.
fn fixture(name: &str) -> (Library, PathBuf) {
    let root = std::env::temp_dir().join(format!("atlas-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let data = root.join("data");
    std::fs::create_dir_all(data.join("text")).expect("fixture data dir");

    let docs = ["alpha", "beta", "gamma", "delta", "delta-2"];
    for d in docs {
        std::fs::write(
            data.join("text").join(format!("{d}.md")),
            format!("# {d}\n"),
        )
        .expect("stub markdown");
    }
    std::fs::write(data.join("titles.json"), r#"{"alpha": "Alpha Book"}"#).expect("titles");
    std::fs::write(
        data.join("collections.json"),
        r#"{"shelf": ["alpha", "beta"]}"#,
    )
    .expect("collections");

    let mut lib = open(root.join("library.db"));
    let mut chunks = Vec::new();
    // 8 cluster chunks per work: gears vocabulary so terms have material
    for (di, doc) in ["alpha", "beta", "gamma", "delta"].iter().enumerate() {
        for c in 0..8u32 {
            chunks.push(chunk(
                doc,
                c + 1,
                0,
                "gears mesh with pinions while the escapement holds tempo steady",
                cluster_emb(di * 8 + c as usize),
            ));
        }
    }
    // the copy: byte-identical passages and embeddings to delta's
    for c in 0..8u32 {
        chunks.push(chunk(
            "delta-2",
            c + 1,
            0,
            "gears mesh with pinions while the escapement holds tempo steady",
            cluster_emb(24 + c as usize),
        ));
    }
    // off-cluster noise so the cluster is a minority of the corpus
    for (di, doc) in docs.iter().enumerate() {
        for c in 0..4u32 {
            chunks.push(chunk(
                doc,
                90 + c,
                0,
                "unrelated marginal passage about weather and postage",
                noise_emb(di * 4 + c as usize),
            ));
        }
    }
    // reserved synthetic doc: indexed, but must never surface in the atlas
    chunks.push(chunk(
        "~card/1",
        0,
        0,
        "a note card about gears",
        cluster_emb(99),
    ));

    lib.wtx(|tx| {
        for c in &chunks {
            tx.upsert(&c.key, c);
        }
    });
    (lib, data)
}

#[test]
fn atlas_builds_themes_trails_and_fresh_sidecar() {
    let (mut lib, data) = fixture("full");
    let fp = atlas::fingerprint(&lib, &data);
    assert_eq!(fp.docs, 5);
    assert!(atlas::load_fresh(&data, &fp).is_none(), "no sidecar yet");

    let claim = atlas::try_claim().expect("no concurrent build in tests");
    let built = atlas::build(claim, &lib, &data, None).expect("build succeeds");

    // themes: the cluster spans 4 works (5 docs), 40 chunks ≥ threshold
    let theme = built
        .themes
        .iter()
        .find(|t| t.ndocs >= 3)
        .expect("cross-doc theme found");
    assert!(theme.size >= 25);
    assert!(
        theme.terms.iter().any(|t| t == "gears"),
        "distinctive vocabulary surfaces: {:?}",
        theme.terms
    );

    // nothing reserved leaks out
    assert!(built.docs.iter().all(|d| !d.id.starts_with('~')));
    let n_real: u32 = built.docs.iter().map(|d| d.chunks).sum();
    assert_eq!(n_real, 60, "40 cluster + 20 noise, card excluded");

    // small corpus: every chunk is a map point, theme members labeled
    assert_eq!(built.points.len(), 60);
    let members = built.points.iter().filter(|p| p.c == theme.id).count();
    assert_eq!(members as u32, theme.size);

    // the trail walks distinct works — the delta-2 copy never earns a
    // second visit to delta's work
    let trail = built
        .trails
        .iter()
        .find(|t| t.c == theme.id)
        .expect("theme has a trail");
    assert!(trail.steps.len() >= 4);
    let mut bases: Vec<String> = trail
        .steps
        .iter()
        .map(|s| {
            let id = &built.docs[s.d as usize].id;
            id.strip_suffix("-2").unwrap_or(id).to_string()
        })
        .collect();
    let steps = bases.len();
    bases.sort();
    bases.dedup();
    assert_eq!(bases.len(), steps, "one visit per work");

    // doc metadata plumbed through
    let alpha = built.docs.iter().find(|d| d.id == "alpha").expect("alpha");
    assert_eq!(alpha.title, "Alpha Book");
    assert_eq!(alpha.collection, "shelf");

    // sidecar round trip: fresh now, stale after another commit
    assert!(atlas::sidecar_path(&data).exists());
    assert!(atlas::load_fresh(&data, &fp).is_some());
    lib.wtx(|tx| {
        let extra = chunk("alpha", 50, 0, "an appendix afterthought", noise_emb(77));
        tx.upsert(&extra.key, &extra);
    });
    let fp2 = atlas::fingerprint(&lib, &data);
    assert_ne!(fp, fp2);
    assert!(
        atlas::load_fresh(&data, &fp2).is_none(),
        "stale after commit"
    );
}
