#[inline(always)]
pub fn accumulate(vector: &mut [f32; crate::DIMENSIONS], param: &crate::lookup::Param) {
    for (v, &p) in vector.iter_mut().zip(param.iter()) {
        // Under quant-8/quant-16 the Param element type is u8/u16 and needs
        // widening; unquantized params are already f32 (no cast to lint).
        #[cfg(any(feature = "quant-8", feature = "quant-16"))]
        let p = p as f32;
        *v += crate::lookup::QUANT_MIN + p * crate::lookup::QUANT_SCALE;
    }
}

/// Greedy longest-match wordpiece over one pre-token, yielding each matched
/// piece's parameter row (UNK for unmatched or oversized words). The single
/// source of the tokenization rule — pooling ([`wordpiece_accumulate`]) and
/// per-token consumers (late-interaction scoring) both walk through here.
#[inline]
pub fn for_each_token(word: &str, wp_buf: &mut String, mut f: impl FnMut(&crate::lookup::Param)) {
    if word.chars().count() > crate::lookup::MAX_WORD_LEN {
        f(&crate::lookup::UNK);
        return;
    }
    let mut start = 0;
    while start < word.len() {
        let mut end = word.len();
        let mut matched = false;
        while end > start {
            while end < word.len() && !word.is_char_boundary(end) {
                end += 1;
            }
            let embedding = if start == 0 {
                crate::lookup::lookup(&word[start..end])
            } else {
                wp_buf.clear();
                wp_buf.push_str("##");
                wp_buf.push_str(&word[start..end]);
                crate::lookup::lookup(wp_buf.as_str())
            };
            if let Some(emb) = embedding {
                f(emb);
                start = end;
                matched = true;
                break;
            }
            end -= 1;
            while end > start && !word.is_char_boundary(end) {
                end -= 1;
            }
        }
        if !matched {
            f(&crate::lookup::UNK);
            return;
        }
    }
}

#[inline]
pub fn wordpiece_accumulate(
    word: &str,
    vector: &mut [f32; crate::DIMENSIONS],
    token_count: &mut u32,
    wp_buf: &mut String,
) {
    for_each_token(word, wp_buf, |param| {
        *token_count += 1;
        accumulate(vector, param);
    });
}
