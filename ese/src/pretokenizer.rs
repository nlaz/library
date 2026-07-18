pub fn normalize_into(input: &str, out: &mut String) {
    use unicode_normalization::UnicodeNormalization;
    out.clear();
    out.reserve(input.len());
    out.extend(
        input
            .chars()
            .filter(|&ch| {
                if ch == '\0' || ch == '\u{FFFD}' {
                    return false;
                }
                !ch.is_control() || ch == '\t' || ch == '\n' || ch == '\r'
            })
            .flat_map(|ch| {
                if ch.is_whitespace() {
                    return SmallCharIter::one(' ');
                }
                if is_chinese_char(ch) {
                    return SmallCharIter::three(' ', ch, ' ');
                }
                SmallCharIter::one(ch)
            })
            .flat_map(|c| c.to_lowercase())
            .nfd()
            .filter(|ch| !is_mark(*ch)),
    );
}

struct SmallCharIter {
    chars: [char; 3],
    pos: u8,
    len: u8,
}

impl SmallCharIter {
    #[inline]
    fn one(a: char) -> Self {
        Self {
            chars: [a, '\0', '\0'],
            pos: 0,
            len: 1,
        }
    }

    #[inline]
    fn three(a: char, b: char, c: char) -> Self {
        Self {
            chars: [a, b, c],
            pos: 0,
            len: 3,
        }
    }
}

impl Iterator for SmallCharIter {
    type Item = char;

    #[inline]
    fn next(&mut self) -> Option<char> {
        if self.pos < self.len {
            let ch = self.chars[self.pos as usize];
            self.pos += 1;
            Some(ch)
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.len - self.pos) as usize;
        (n, Some(n))
    }
}

fn is_chinese_char(ch: char) -> bool {
    let cp = ch as u32;
    matches!(cp,
        0x4E00..=0x9FFF |
        0x3400..=0x4DBF |
        0x20000..=0x2A6DF |
        0x2A700..=0x2B73F |
        0x2B740..=0x2B81F |
        0x2B820..=0x2CEAF |
        0xF900..=0xFAFF |
        0x2F800..=0x2FA1F
    )
}

#[inline]
fn is_mark(ch: char) -> bool {
    matches!(
        unicode_general_category::get_general_category(ch),
        unicode_general_category::GeneralCategory::NonspacingMark
            | unicode_general_category::GeneralCategory::SpacingMark
            | unicode_general_category::GeneralCategory::EnclosingMark
    )
}

#[inline]
pub fn for_each_pre_token<F: FnMut(&str)>(normalized: &str, mut f: F) {
    let mut start: Option<usize> = None;

    for (i, ch) in normalized.char_indices() {
        if ch.is_whitespace() {
            if let Some(s) = start.take() {
                f(&normalized[s..i]);
            }
            continue;
        }

        if is_punctuation(ch) {
            if let Some(s) = start.take() {
                f(&normalized[s..i]);
            }
            let end = i + ch.len_utf8();
            f(&normalized[i..end]);
            continue;
        }

        if start.is_none() {
            start = Some(i);
        }
    }

    if let Some(s) = start {
        f(&normalized[s..]);
    }
}

fn is_punctuation(ch: char) -> bool {
    let cp = ch as u32;
    if matches!(cp, 33..=47 | 58..=64 | 91..=96 | 123..=126) {
        return true;
    }
    if ch.is_ascii() {
        return false;
    }
    matches!(
        unicode_general_category::get_general_category(ch),
        unicode_general_category::GeneralCategory::ConnectorPunctuation
            | unicode_general_category::GeneralCategory::DashPunctuation
            | unicode_general_category::GeneralCategory::OpenPunctuation
            | unicode_general_category::GeneralCategory::ClosePunctuation
            | unicode_general_category::GeneralCategory::InitialPunctuation
            | unicode_general_category::GeneralCategory::FinalPunctuation
            | unicode_general_category::GeneralCategory::OtherPunctuation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_char_boundaries() {
        // Main CJK Unified Ideographs block: U+4E00..=U+9FFF.
        assert!(is_chinese_char('\u{4E00}'), "block start should be CJK");
        assert!(is_chinese_char('\u{9FFF}'), "block end should be CJK");
        // One codepoint outside each edge. U+4DFF sits in the Yijing Hexagram
        // Symbols block (between Extension A and the main block) and is not
        // covered by any of the impl's ranges; U+A000 (Yi Syllables) is past
        // the main block's end.
        assert!(
            !is_chinese_char('\u{4DFF}'),
            "just below block start should not be CJK"
        );
        assert!(
            !is_chinese_char('\u{A000}'),
            "just above block end should not be CJK"
        );
    }

    #[test]
    fn punctuation_covers_ascii_and_unicode_categories() {
        // ASCII punctuation, covered by the impl's literal codepoint ranges.
        for ch in [
            '.', ',', '-', '!', '?', '(', ')', '[', ']', '{', '}', ':', '@',
        ] {
            assert!(is_punctuation(ch), "{ch:?} should be punctuation (ascii)");
        }
        assert!(!is_punctuation('A'), "ascii letter is not punctuation");
        assert!(!is_punctuation('0'), "ascii digit is not punctuation");

        // Non-ASCII Unicode punctuation categories the impl matches via
        // unicode_general_category: Pd (dash), Pi/Pf (quotes), Ps (open).
        assert!(
            is_punctuation('\u{2014}'),
            "em dash (Pd) should be punctuation"
        );
        assert!(
            is_punctuation('\u{201C}'),
            "left double quotation mark (Pi) should be punctuation"
        );
        assert!(
            is_punctuation('\u{201D}'),
            "right double quotation mark (Pf) should be punctuation"
        );
        assert!(
            is_punctuation('\u{3010}'),
            "CJK left black lenticular bracket (Ps) should be punctuation"
        );

        // The impl only matches Unicode Punctuation (P*) categories, not
        // Symbol (S*) categories, for non-ASCII input -- a plain currency /
        // copyright symbol is not treated as punctuation.
        assert!(
            !is_punctuation('\u{00A9}'),
            "copyright sign (So, a Symbol category) should not be punctuation"
        );
        assert!(!is_punctuation('猫'), "a CJK ideograph is not punctuation");
    }

    #[test]
    fn small_char_iter_roundtrip() {
        // Mixed ascii + multibyte accented + CJK + emoji (surrogate-pair-range) input.
        let s = "a-b café 猫犬 🎉!";
        let via_small_iter: Vec<char> = s.chars().flat_map(SmallCharIter::one).collect();
        let via_std_chars: Vec<char> = s.chars().collect();
        assert_eq!(via_small_iter, via_std_chars);
        assert_eq!(via_small_iter.into_iter().collect::<String>(), s);

        // `three` is used by normalize_into to pad CJK chars with spaces; confirm
        // its iteration order and length independently of that call site.
        let padded: Vec<char> = SmallCharIter::three(' ', '猫', ' ').collect();
        assert_eq!(padded, vec![' ', '猫', ' ']);
    }
}
