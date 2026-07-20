//! End-to-end tests for the blended `answer` pipeline against hermetic
//! temp-dir stores: the per-stage perf breakdown, phase reporting, and the
//! image track (which runs on its own thread inside `answer`). No real
//! embedding model is ever loaded — instant/text-only and images-only
//! queries are exactly the modes that never touch ese, and CLIP is a
//! caller-supplied closure fed synthetic embeddings.

use library_core::{
    CLIP_DIM, ChunkKey, ChunkRec, ClipEmb, EMB_DIM, ImageKey, ImageRec, Images, Library, Query,
    Word, answer, open, open_images, perf,
};

fn chunk(doc: &str, idx: u32, text: &str, hot: usize) -> ChunkRec {
    let mut emb = [0.0f32; EMB_DIM];
    emb[hot] = 1.0;
    ChunkRec {
        key: ChunkKey {
            doc: doc.to_string(),
            page: 1,
            idx,
        },
        words: text
            .split(' ')
            .map(|t| Word {
                t: t.to_string(),
                x: 0.1,
                y: 0.1,
                w: 0.05,
                h: 0.01,
            })
            .collect(),
        emb,
    }
}

fn synthetic_library(name: &str) -> Library {
    let dir = std::env::temp_dir().join(format!("library-core-answer-e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let mut lib = open(dir);
    let chunks = [
        chunk("watchmaking", 0, "the escapement regulates the wheel", 0),
        chunk("cooking", 0, "stock reduction demands patience", 7),
    ];
    lib.wtx(|tx| {
        for c in &chunks {
            tx.upsert(&c.key, c);
        }
    });
    lib
}

fn fig(doc: &str, page: u32, hot: usize) -> ImageRec {
    let mut emb = [0.0f32; CLIP_DIM];
    emb[hot] = 1.0;
    ImageRec {
        key: ImageKey {
            doc: doc.to_string(),
            page,
            idx: 0,
        },
        bbox: [0.1, 0.1, 0.5, 0.5],
        emb,
    }
}

fn synthetic_images(name: &str) -> Images {
    let dir = std::env::temp_dir().join(format!("library-core-answer-e2e-img-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let mut images = open_images(dir);
    let figs = [fig("figdoc", 1, 0), fig("figdoc", 2, 3)];
    images.wtx(|tx| {
        for f in &figs {
            tx.upsert(&f.key, f);
        }
    });
    images
}

fn query(q: &str, mode: &str, kind: &str) -> Query {
    Query {
        seq: 1,
        q: q.to_string(),
        mode: mode.to_string(),
        col: String::new(),
        kind: kind.to_string(),
        doc: String::new(),
        offset: 0,
    }
}

/// The perf ring is process-global and tests run concurrently, so each test
/// uses a unique query string and fishes its own record back out.
fn record_for(q: &str) -> perf::SearchRecord {
    perf::search_log()
        .into_iter()
        .find(|r| r.q == q)
        .expect("answer() should have pushed a perf record")
}

#[test]
fn instant_text_query_reports_lexical_stage_breakdown() {
    let lib = synthetic_library("instant");
    let images = synthetic_images("instant");
    let q = query("escapement instant-stage-probe", "instant", "");
    let resp = answer(&lib, &images, &std::env::temp_dir(), &q, |_| None);

    // instant mode: lexical only — no image track, no query embedding
    assert_eq!(resp.phase, "lex");
    assert!(!resp.hits.is_empty());
    let rec = record_for(&q.q);
    let names: Vec<&str> = rec.stages.iter().map(|(n, _)| n.as_str()).collect();
    // vec_search is absent (no embedding), clip stages are absent (no track)
    assert_eq!(
        names,
        [
            "ese_embed",
            "term_expand",
            "lex_search",
            "fuse+resolve",
            "blend"
        ]
    );
    assert!(rec.lex_n > 0);
    assert_eq!(rec.sem_n, 0);
}

#[test]
fn images_query_runs_image_track_and_reports_stages() {
    let lib = synthetic_library("images");
    let images = synthetic_images("images");
    let q = query("figure image-stage-probe", "full", "images");
    // synthetic CLIP embedding aligned with the first figure: the second
    // (orthogonal) one lands on the noise floor and the spread cutoff kills it
    let resp = answer(&lib, &images, &std::env::temp_dir(), &q, |_| {
        let mut e: ClipEmb = [0.0f32; CLIP_DIM];
        e[0] = 1.0;
        Some(e)
    });

    assert_eq!(resp.phase, "img");
    assert!(!resp.hits.is_empty());
    let rec = record_for(&q.q);
    let names: Vec<&str> = rec.stages.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["clip_embed", "image_search", "blend"]);
    assert_eq!(rec.img_fetched, 2);
    assert_eq!(rec.img_killed, 1);
}

#[test]
fn failed_clip_embed_skips_image_search_stage() {
    let lib = synthetic_library("noclip");
    let images = synthetic_images("noclip");
    let q = query("figure noclip-stage-probe", "full", "images");
    // encoder not loaded yet: the track records its attempt and yields nothing
    let resp = answer(&lib, &images, &std::env::temp_dir(), &q, |_| None);

    assert!(resp.hits.is_empty());
    let rec = record_for(&q.q);
    let names: Vec<&str> = rec.stages.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["clip_embed", "blend"]);
    assert_eq!(rec.img_fetched, 0);
}
