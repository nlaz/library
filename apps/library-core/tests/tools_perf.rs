//! The agent's searches land in the same perf ring as the UI's, marked
//! `mode=agent`, and hand back the record's timestamp so the chat provenance
//! view can link a tool call to its ranker breakdown. Before this, an agent
//! search was invisible: only `answer()` recorded, so "why did the librarian
//! say that?" bottomed out at a hit count with no provenance behind it.
//!
//! Hermetic: a temp-dir store plus a temp `data/` with page text. `ese` is
//! compiled into the crate, so `search_tool`'s embed call touches no network
//! and no real library.

use library_core::meta::Ctx;
use library_core::{ChunkKey, ChunkRec, EMB_DIM, Library, Word, open, perf, tools};

fn chunk(doc: &str, idx: u32, text: &str) -> ChunkRec {
    ChunkRec {
        key: ChunkKey {
            doc: doc.to_string(),
            page: 1,
            idx,
        },
        words: text
            .split_whitespace()
            .map(|t| Word {
                t: t.to_string(),
                x: 0.0,
                y: 0.0,
                w: 0.1,
                h: 0.1,
            })
            .collect(),
        emb: [0.0f32; EMB_DIM],
    }
}

fn fixture(name: &str) -> (Library, Ctx) {
    let dir = std::env::temp_dir().join(format!("library-core-tools-perf-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("text")).expect("fixture: create temp text dir");
    std::fs::write(
        dir.join("text/watchmaking.md"),
        "# w\n<!-- page 1 -->\nthe escapement regulates the balance wheel and plenty more readable text besides\n",
    )
    .expect("fixture: write page text");

    let mut lib = open(dir.join("store"));
    lib.wtx(|tx| {
        for c in [
            chunk(
                "watchmaking",
                0,
                "the escapement regulates the balance wheel",
            ),
            chunk("watchmaking", 1, "a mainspring stores the driving force"),
        ] {
            tx.upsert(&c.key, &c);
        }
    });
    let ctx = Ctx::in_memory(&dir).expect("fixture: meta");
    (lib, ctx)
}

#[test]
fn agent_searches_land_in_the_perf_ring_with_a_link_back() {
    let (lib, ctx) = fixture("ring");
    let out = lib.rtx(|r| tools::search_tool(&r, &lib, &ctx, "escapement", "", 6));

    let ts = out["perf_ts"]
        .as_u64()
        .expect("tool result carries perf_ts");
    let rec = perf::search_log()
        .into_iter()
        .find(|r| r.ts_ms == ts)
        .expect("a record with that ts is in the ring");

    assert_eq!(rec.mode, "agent");
    assert_eq!(rec.phase, "tool");
    assert_eq!(rec.q, "escapement");
    assert!(!rec.zero);
    assert_eq!(rec.served, rec.text_hits.len());
    // the provenance is the hits the model was actually shown
    assert_eq!(rec.text_hits[0].doc, "watchmaking");
    // stages are what the perf view's waterfall draws
    let names: Vec<&str> = rec.stages.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["ese_embed", "search", "dedup+cutoff", "top_hit_page"]
    );
    assert!(rec.total_us > 0);
}

/// A miss is the case the Agent tab exists for. Semantic search still hands
/// back hits (and `rel` is relative-to-top, so junk scores 1.0) — it's
/// `confidence` that says the library doesn't cover this. The ring must
/// record the hits anyway, or the provenance for a hedged answer is missing
/// exactly when it's most wanted.
#[test]
fn a_miss_records_its_hits_even_though_confidence_is_none() {
    let (lib, ctx) = fixture("miss");
    let out = lib.rtx(|r| tools::search_tool(&r, &lib, &ctx, "photosynthesis", "", 6));

    assert_eq!(out["confidence"], "none");
    let ts = out["perf_ts"].as_u64().unwrap();
    let rec = perf::search_log()
        .into_iter()
        .find(|r| r.ts_ms == ts)
        .expect("a miss is recorded too");
    assert_eq!(rec.mode, "agent");
    assert_eq!(rec.served, rec.text_hits.len());
    // no top_hit_page hop on a "none" verdict, but the stage is still timed
    assert!(rec.stages.iter().any(|(n, _)| n == "top_hit_page"));
}
