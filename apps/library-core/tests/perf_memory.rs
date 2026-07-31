//! The memory-provenance payload against a real temp-dir text + image
//! store: coverage of both HNSW indexes and both fjall stores, arithmetic
//! of the accounted/unaccounted split, and the serialized field names the
//! web view's hand-mirrored types depend on.

use library_core::images::{ImageKey, ImageRec, open_images};
use library_core::perf::memory;
use library_core::{CLIP_DIM, ChunkKey, ChunkRec, EMB_DIM, Word, open};

fn chunk(doc: &str, idx: u32, text: &str, hot: usize) -> ChunkRec {
    let mut emb = [0.0f32; EMB_DIM];
    emb[hot % EMB_DIM] = 1.0;
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
        emb,
    }
}

fn figure(doc: &str, idx: u32, hot: usize) -> ImageRec {
    let mut emb = [0.0f32; CLIP_DIM];
    emb[hot % CLIP_DIM] = 1.0;
    ImageRec {
        key: ImageKey {
            doc: doc.to_string(),
            page: 1,
            idx,
        },
        bbox: [0.1, 0.1, 0.5, 0.5],
        emb,
    }
}

#[test]
fn memory_breakdown_covers_indexes_caches_and_stores() {
    let dir = std::env::temp_dir().join("library-core-perf-memory");
    let _ = std::fs::remove_dir_all(&dir);
    // a corpus on disk: one original, two page scans, one ocr artifact
    std::fs::create_dir_all(dir.join("pdfs")).unwrap();
    std::fs::create_dir_all(dir.join("pages/doc-a")).unwrap();
    std::fs::create_dir_all(dir.join("ocr")).unwrap();
    std::fs::write(dir.join("pdfs/doc-a.pdf"), vec![0u8; 1000]).unwrap();
    std::fs::write(dir.join("pages/doc-a/1.jpeg"), vec![0u8; 300]).unwrap();
    std::fs::write(dir.join("pages/doc-a/2.jpeg"), vec![0u8; 300]).unwrap();
    std::fs::write(dir.join("ocr/doc-a.json"), vec![0u8; 50]).unwrap();
    let mut lib = open(dir.join("library.db"));
    let mut images = open_images(dir.join("images.db"));

    lib.wtx(|tx| {
        for i in 0..5u32 {
            let c = chunk("doc-a", i, "gear train escapement", i as usize);
            tx.upsert(&c.key, &c);
        }
    });
    images.wtx(|tx| {
        for i in 0..3u32 {
            let f = figure("doc-a", i, i as usize);
            tx.upsert(&f.key, &f);
        }
    });

    let rss = 1u64 << 30;
    let m = memory(&lib, &images, &dir, Some(rss));

    // the corpus section: counts, exact embedding payload, per-source disk
    assert_eq!(
        (m.corpus.docs, m.corpus.chunks, m.corpus.figures),
        (1, 5, 3)
    );
    assert_eq!(
        m.corpus.emb_bytes,
        ((5 * EMB_DIM + 3 * CLIP_DIM) * 4) as u64
    );
    assert_eq!(m.corpus.pdf_bytes, 1000);
    assert_eq!(m.corpus.page_bytes, 600);
    assert_eq!(m.corpus.ocr_bytes, 50);

    // both HNSW indexes, with live counts matching what was inserted
    assert_eq!(m.indexes.len(), 2);
    assert_eq!(m.indexes[0].stats.live, 5);
    assert_eq!(m.indexes[1].stats.live, 3);
    assert!(m.indexes.iter().all(|i| i.bytes > 0 && !i.stats.stale));
    assert_eq!(
        m.indexes[0].per_vector_bytes as usize,
        EMB_DIM * 4 + 32 * 4 + 8
    );

    // both stores, each covering its sink keyspaces
    assert_eq!(m.stores.len(), 2);
    let lib_names: Vec<&str> = m.stores[0]
        .db
        .keyspaces
        .iter()
        .map(|k| k.name.as_str())
        .collect();
    for want in [
        "sink_lex",
        "sink_vec",
        "sink_manifest",
        "sink_terms",
        "keyed_root",
    ] {
        assert!(
            lib_names.contains(&want),
            "library.db missing {want}: {lib_names:?}"
        );
    }
    let img_names: Vec<&str> = m.stores[1]
        .db
        .keyspaces
        .iter()
        .map(|k| k.name.as_str())
        .collect();
    assert!(
        img_names.contains(&"sink_imgvec"),
        "images.db: {img_names:?}"
    );

    // arithmetic: accounted sums the RAM line items; unaccounted is signed
    let expected: u64 = m.indexes.iter().map(|i| i.bytes).sum::<u64>()
        + m.caches.iter().map(|c| c.bytes).sum::<u64>()
        + m.models.iter().filter_map(|x| x.bytes).sum::<u64>()
        + m.stores.iter().map(|s| s.ram_bytes).sum::<u64>();
    assert_eq!(m.accounted_bytes, expected);
    assert_eq!(
        m.unaccounted_bytes,
        Some(rss as i64 - m.accounted_bytes as i64)
    );
    assert_eq!(memory(&lib, &images, &dir, None).unaccounted_bytes, None);

    // ese is priced, clip is opaque
    assert!(
        m.models
            .iter()
            .any(|x| x.bytes == Some(ese::MODEL_BYTES as u64))
    );
    assert!(m.models.iter().any(|x| x.bytes.is_none()));

    // serialized field names are the contract with the web mirror
    let v = serde_json::to_value(&m).expect("breakdown serializes");
    assert!(v["rss_bytes"].is_u64());
    let idx = &v["indexes"][0];
    for field in [
        "live",
        "slots",
        "free_slots",
        "stale",
        "map_bytes",
        "graph_bytes",
    ] {
        assert!(!idx[field].is_null(), "flattened index field {field}");
    }
    let store = &v["stores"][0];
    for field in [
        "ram_bytes",
        "disk_bytes",
        "write_buffer_bytes",
        "block_cache_capacity",
        "block_cache_bytes",
        "keyspaces",
    ] {
        assert!(!store[field].is_null(), "flattened store field {field}");
    }
    assert!(!v["caches"][0]["warmed"].is_null());
    assert!(v["atlas_building"].is_boolean());
    for field in [
        "docs",
        "emb_bytes",
        "pdf_bytes",
        "page_bytes",
        "ocr_bytes",
        "chunk_table_bytes",
        "model_dir_bytes",
    ] {
        assert!(!v["corpus"][field].is_null(), "corpus field {field}");
    }
}
