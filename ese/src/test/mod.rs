#[test]
fn test_fuzz() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let seed_strings: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0xFF],
        vec![0, 0, 0, 0],
        vec![0xFF; 256],
        b"\x00\x01\x02\x03".to_vec(),
        b"\xc0\xaf".to_vec(),         // overlong UTF-8
        b"\xed\xa0\x80".to_vec(),     // surrogate half
        b"\xf4\x90\x80\x80".to_vec(), // above U+10FFFF
        "hello".as_bytes().to_vec(),
        " ".repeat(1000).into_bytes(),
        "\u{FFFD}\u{FFFD}".as_bytes().to_vec(),
        "\0\0\0".as_bytes().to_vec(),
        "##".as_bytes().to_vec(),
        "a\u{0300}\u{0301}b".as_bytes().to_vec(), // combining marks
        "\u{4E00}\u{9FFF}".as_bytes().to_vec(),   // CJK
        "!@#$%^&*()".as_bytes().to_vec(),
    ];

    // Test raw byte sequences that happen to be valid UTF-8
    for seed in &seed_strings {
        if let Ok(s) = std::str::from_utf8(seed) {
            let _ = crate::encode_single(s);
        }
    }

    // Pseudo-random generation from seeds
    for i in 0u64..10_000 {
        let mut h = DefaultHasher::new();
        i.hash(&mut h);
        let mut state = h.finish();

        let len = (state % 512) as usize;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push((state & 0xFF) as u8);
        }

        if let Ok(s) = String::from_utf8(bytes) {
            let _ = crate::encode_single(&s);
        }
    }

    // Also fuzz with valid unicode strings built from char ranges
    for cp in (0u32..0x10000).step_by(37) {
        if let Some(ch) = char::from_u32(cp) {
            let s: String = std::iter::repeat_n(ch, 5).collect();
            let _ = crate::encode_single(&s);
        }
    }
}

#[test]
fn wordpiece_garbage_token_maps_to_unk() {
    // A pre-token longer than MAX_WORD_LEN is treated as UNK unconditionally
    // by wordpiece_accumulate, regardless of what's actually in the baked
    // vocab -- this is the deterministic way to force the UNK path without
    // depending on which garbage strings happen to be absent from the vocab.
    let garbage: String = "x".repeat(crate::lookup::MAX_WORD_LEN + 1);

    let mut vector = [0.0f32; crate::DIMENSIONS];
    let mut token_count = 0u32;
    let mut wp_buf = String::new();
    crate::wordpiece::wordpiece_accumulate(&garbage, &mut vector, &mut token_count, &mut wp_buf);

    assert_eq!(
        token_count, 1,
        "over-long token should count as exactly one (UNK) piece"
    );

    let mut expected = [0.0f32; crate::DIMENSIONS];
    crate::wordpiece::accumulate(&mut expected, &crate::lookup::UNK);
    assert_eq!(
        vector, expected,
        "over-long token should accumulate the UNK embedding"
    );
}

#[test]
fn wordpiece_known_word_tokenizes_to_expected_piece_count() {
    // "the" is a whole-word entry in any BERT-style wordpiece vocab (this
    // model's tokenizer.json is a standard uncased wordpiece vocab), so it
    // must resolve as a single piece rather than falling back to subwords/UNK.
    let the_embedding = crate::lookup::lookup("the").expect("\"the\" missing from baked vocab");

    let mut vector = [0.0f32; crate::DIMENSIONS];
    let mut token_count = 0u32;
    let mut wp_buf = String::new();
    crate::wordpiece::wordpiece_accumulate("the", &mut vector, &mut token_count, &mut wp_buf);

    assert_eq!(
        token_count, 1,
        "\"the\" should tokenize to a single wordpiece"
    );

    let mut expected = [0.0f32; crate::DIMENSIONS];
    crate::wordpiece::accumulate(&mut expected, the_embedding);
    assert_eq!(vector, expected);
}

#[test]
fn lookup_known_term_resolves() {
    assert!(
        crate::lookup::lookup("the").is_some(),
        "\"the\" should resolve"
    );
    assert!(
        crate::lookup::lookup("hello").is_some(),
        "\"hello\" should resolve"
    );

    // Collision-bucket handling (which bucket/seed a key hashes into, probing
    // on a forced collision, etc.) isn't practically testable here: `lookup`
    // only exposes a black-box `&str -> Option<&Param>` API, and the bucket
    // assignment/seeds/slots are generated at build time from the live vocab
    // (ese/build.rs) with no test hook to introspect or force a collision.
    // Skipping rather than reaching into build-generated internals.
}

#[cfg(feature = "tests")]
mod optional {
    include!(concat!(env!("OUT_DIR"), "/optional_testdata.rs"));

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    }

    #[test]
    fn test_embeddings_match() {
        let embeddings = crate::encode(TEST_INPUTS);
        for (i, (want, got)) in TEST_OUTPUTS.iter().zip(embeddings.iter()).enumerate() {
            let sim = cosine_similarity(want, got);
            assert!(
                sim > 0.999999,
                "cosine similarity for {i} below threshold: {sim}"
            );
        }
    }
}
