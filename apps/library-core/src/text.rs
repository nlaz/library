//! Query/document tokenization shared by every index and the term dictionary.

pub fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace()
        .filter_map(|t| {
            let t: String = t
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();
            (t.len() > 1).then_some(t)
        })
        .collect()
}

/// [`tokenize`] in fold's Bm25 buffer convention (`\0`-terminated tokens).
/// The Bm25 sink MUST tokenize exactly like [`tokenize`]: TermDict's
/// completion terms come from it, and prefix-expanded query terms only match
/// postings produced the same way. fold's default tokenizer is ASCII-only
/// and keeps 1-char tokens, so it would silently disagree.
pub fn lex_tokenize(text: &str, tokens: &mut Vec<u8>) {
    tokens.clear();
    for t in tokenize(text) {
        tokens.extend_from_slice(t.as_bytes());
        tokens.push(0);
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;

    /// Inputs chosen to stress every branch: case, inner punctuation,
    /// digits, unicode (accents, CJK), 1-char tokens, and emptiness.
    const NASTY: &[&str] = &[
        "",
        " ",
        "a",
        "ok",
        "The Watch-Maker's 2nd chain!",
        "self-winding    self-winding",
        "Ω-mega Ωmega",
        "café CAFÉ",
        "手表 zhōng biǎo",
        "x7 7x 77 xx",
        "…ellipsis… (parens) [brackets]",
        "trailing-",
        "-leading",
        "under_score",
        "MiXeD CaSe",
        "ﬁligree", // ligature char is alphabetic
        "naïve résumé",
        "a.b.c d,e,f",
        "1 22 333",
        "I a an of ok",
    ];

    #[test]
    fn tokenize_lowercases_strips_punct_and_splits() {
        assert_eq!(
            tokenize("The Watch-Maker's 2nd chain!"),
            vec!["the", "watchmakers", "2nd", "chain"]
        );
        // 1-char tokens (after stripping) are dropped
        assert_eq!(tokenize("a I x7 ok"), vec!["x7", "ok"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("!!! ..."), Vec::<String>::new());
    }

    /// Pins the contract in [`lex_tokenize`]'s doc comment: the Bm25 sink
    /// MUST tokenize exactly like [`tokenize`], or TermDict completions stop
    /// matching lexical postings. If you change either function, change the
    /// other to match — this test is the tripwire.
    #[test]
    fn tokenize_agrees_with_lex_tokenize() {
        let mut buf = Vec::new();
        for s in NASTY {
            lex_tokenize(s, &mut buf);
            let expect: Vec<u8> = tokenize(s)
                .iter()
                .flat_map(|t| t.bytes().chain(std::iter::once(0)))
                .collect();
            assert_eq!(buf, expect, "tokenizers disagree on {s:?}");
        }
    }
}
