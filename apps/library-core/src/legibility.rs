//! Legibility scoring for OCR page text.
//!
//! Some docs (notably Internet Archive scans) carry an embedded PDF text
//! layer of decades-old multi-column OCR that interleaves columns into word
//! salad. The chat agent quotes page text verbatim, so tools need a cheap
//! way to tell readable prose from OCR noise — to skip bad pages when
//! sampling, to warn the model off quoting, and to rank docs for re-OCR.

/// Common function words in the library's languages (English, plus Italian
/// and French for the cookbook shelf). Real prose in any of them has a high
/// hit-rate against this list; OCR salad has almost none.
const STOPWORDS: &[&str] = &[
    // English
    "the", "of", "and", "to", "a", "in", "is", "it", "you", "that", "for",
    "on", "with", "as", "are", "this", "be", "at", "or", "from", "by",
    "an", "was", "we", "can", "your", "all", "have", "will", "one", "but",
    "not", "they", "his", "her", "has", "had", "more", "when", "which",
    "their", "if", "there", "what", "about", "out", "up", "so", "them",
    "some", "into", "than", "then", "its", "also", "how", "our", "these",
    // Italian
    "di", "e", "il", "la", "che", "un", "per", "è", "con", "non", "si",
    "le", "del", "i", "da", "al", "come", "dei", "nel", "se", "della",
    "o", "ma", "più", "lo", "su", "una", "questo", "anche", "ne", "gli",
    "alla", "poi", "quando", "chi", "due", "essa", "ed", "delle", "alle",
    // French
    "les", "des", "du", "et", "en", "une", "dans", "pour", "que", "qui",
    "sur", "avec", "au", "aux", "ce", "cette", "elle", "pas", "ou",
    "mais", "son", "sa", "ses", "vous", "nous", "je", "faire", "bien",
];

/// Pages at or above this overall score are served without hesitation;
/// below it, sampling keeps drawing and tool results carry a paraphrase
/// note. Calibrated against the corpus: clean prose pages run 0.92–0.99
/// (all languages), code listings and garbled scans 0.35–0.45.
pub const LEGIBLE_OK: f32 = 0.55;

/// `min_window` below this means some stretch of the page is unquotable
/// (column-interleaved OCR salad). Calibrated: the worst clean-page windows
/// (address/price blocks in catalogs) bottom out ~0.47.
pub const NOISY_MIN: f32 = 0.45;

/// Collapse every whitespace run to a single space. Page text goes to the
/// model inside JSON; the 3B model copies literal \n escapes into its
/// answers otherwise, and layout whitespace is meaningless to a narrator.
pub fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 0..1 estimate of how much of `text` reads as English prose.
///
/// Dependency-free blend. Stopword hit-rate is the anchor: clean prose runs
/// ~0.35–0.5 against the list above, OCR salad near 0 (misreads like
/// "Veckly" or "Stece" are word-*shaped*, so shape alone can't tell them
/// apart). Junk fragments (short non-stopword confetti like "ee ee cs" left
/// by column interleaving) and word shape fill in the rest, with a small
/// sanity term on mean token length.
pub fn legibility(text: &str) -> f32 {
    let mut tokens = 0usize;
    let mut stop = 0usize;
    let mut shaped = 0usize;
    let mut junk = 0usize;
    let mut len_sum = 0usize;
    for raw in text.split_whitespace() {
        let tok: String = raw
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if tok.is_empty() {
            continue;
        }
        tokens += 1;
        let chars = tok.chars().count();
        len_sum += chars;
        let is_stop = STOPWORDS.contains(&tok.as_str());
        if is_stop {
            stop += 1;
        }
        let alpha = tok.chars().filter(|c| c.is_alphabetic()).count();
        let has_vowel = tok.chars().any(|c| "aeiouy".contains(c));
        if chars <= 20 && alpha * 5 >= chars * 4 && has_vowel {
            shaped += 1;
        }
        if chars <= 2 && !is_stop && alpha == chars {
            junk += 1;
        }
    }
    if tokens == 0 {
        return 0.0;
    }
    let stop_score = ((stop as f32 / tokens as f32) / 0.28).min(1.0);
    let shaped_score = shaped as f32 / tokens as f32;
    let junk_free = 1.0 - ((junk as f32 / tokens as f32) / 0.20).min(1.0);
    let mean_len = len_sum as f32 / tokens as f32;
    let len_score = if (3.0..=9.0).contains(&mean_len) {
        1.0
    } else if mean_len < 3.0 {
        (mean_len / 3.0).max(0.0)
    } else {
        (1.0 - (mean_len - 9.0) / 6.0).max(0.0)
    };
    (0.55 * stop_score + 0.20 * shaped_score + 0.15 * junk_free + 0.10 * len_score)
        .clamp(0.0, 1.0)
}

/// Minimum `legibility` over sliding ~40-token windows (stride 20).
///
/// Garbled runs are usually sub-page — a column-interleaved stretch in the
/// middle of otherwise-clean prose scores fine at page level but is exactly
/// what the model must not quote. Short texts fall back to the whole-text
/// score.
pub fn min_window(text: &str) -> f32 {
    const WINDOW: usize = 40;
    const STRIDE: usize = 20;
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= WINDOW {
        return legibility(text);
    }
    let mut min = 1.0f32;
    let mut start = 0;
    while start < words.len() {
        let end = (start + WINDOW).min(words.len());
        let win = words[start..end].join(" ");
        min = min.min(legibility(&win));
        if end == words.len() {
            break;
        }
        start += STRIDE;
    }
    min
}

/// Only the quotable stretches of a page: ~40-word blocks that score at
/// least `NOISY_MIN`, gaps elided with " … ". Empty when nothing on the
/// page is quotable.
///
/// For browse surfaces (sample_page), where serving less-but-clean beats
/// serving everything plus a "don't quote the garbled parts" note — the 3B
/// model parrots such notes back at the user (measured: mt-another-fact
/// probe). Fidelity-critical paths (read_pages, top_hit_page) keep full
/// text instead.
/// Blocks are word-granular filters, so a few salad words bleed through at
/// prose/salad boundaries — acceptable for browse, where the win is
/// dropping the long interleaved runs. 20 words keeps that bleed small
/// while giving `legibility` enough tokens to judge. The bar is the strict
/// LEGIBLE_OK, not NOISY_MIN: for an excerpt, dropping a dull address
/// block costs less than keeping a salad block that scraped by.
pub fn legible_excerpt(text: &str) -> String {
    const BLOCK: usize = 20;
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= BLOCK {
        let joined = words.join(" ");
        return if legibility(&joined) >= LEGIBLE_OK { joined } else { String::new() };
    }
    let mut out: Vec<String> = Vec::new();
    let mut prev_kept = false;
    for chunk in words.chunks(BLOCK) {
        let block = chunk.join(" ");
        if legibility(&block) >= LEGIBLE_OK {
            if prev_kept {
                let last = out.last_mut().expect("prev_kept implies non-empty");
                last.push(' ');
                last.push_str(&block);
            } else {
                out.push(block);
            }
            prev_kept = true;
        } else {
            prev_kept = false;
        }
    }
    out.join(" … ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GARBLED: &str = "Gotu iicher cs Veckly etc Seeger sty Merkel Stece ee 7 ee ee peter Seeeetca in Uriters Market c/a";
    const CLEAN: &str = "This is a do-it-yourself kit for new publishers. The biggest selling \
        points are its excellent planning forms, which show how to figure a budget for book \
        production, promotion, and marketing across the first year.";

    #[test]
    fn garbled_scores_below_clean_prose() {
        let g = legibility(GARBLED);
        let c = legibility(CLEAN);
        assert!(g < 0.45, "garbled scored {g}");
        assert!(c > 0.6, "clean scored {c}");
        assert!(g + 0.2 < c, "no separation: garbled {g} vs clean {c}");
    }

    #[test]
    fn degenerate_inputs_score_low() {
        assert_eq!(legibility(""), 0.0);
        assert_eq!(legibility("   \n\t "), 0.0);
        assert!(legibility("14 92 7 381 6 55 210 9") < 0.45);
    }

    #[test]
    fn min_window_catches_garbled_run_inside_clean_page() {
        let clean_half = CLEAN.repeat(2);
        let mixed = format!("{clean_half} {GARBLED} {GARBLED} {clean_half}");
        assert!(legibility(&mixed) > LEGIBLE_OK, "page average should pass");
        assert!(
            min_window(&mixed) < NOISY_MIN,
            "worst window should flag the garbled run: {}",
            min_window(&mixed)
        );
        assert!(min_window(CLEAN) > LEGIBLE_OK);
    }

    #[test]
    fn legible_excerpt_keeps_prose_and_drops_salad() {
        let clean_half = CLEAN.repeat(2);
        let salad_run = [GARBLED; 4].join(" ");
        let mixed = format!("{clean_half} {salad_run} {clean_half}");
        let ex = legible_excerpt(&mixed);
        assert!(ex.contains("do-it-yourself kit"), "prose survived: {ex}");
        // interior salad blocks drop; at most boundary bleed survives
        let veckly = ex.matches("Veckly").count();
        assert!(veckly <= 1, "salad bulk dropped ({veckly} of 4 remain): {ex}");
        assert!(legibility(&ex) > legibility(&mixed));
        // fully garbled text has no quotable stretch at all
        assert_eq!(legible_excerpt(GARBLED), "");
        // clean short text passes through whole
        assert_eq!(legible_excerpt(CLEAN), squash_ws(CLEAN));
    }

    #[test]
    fn italian_prose_is_not_penalized() {
        let it = "Il pane tagliatelo a quadrettini e friggetelo nel burro o \
            nel lardo, e quando occorre servite la minestra con il brodo \
            di carne che avrete preparato per questo piatto della festa.";
        assert!(legibility(it) > 0.6, "italian scored {}", legibility(it));
    }

    #[test]
    fn squash_ws_collapses_all_whitespace() {
        assert_eq!(squash_ws("a  b\n\nc\td "), "a b c d");
        assert_eq!(squash_ws(""), "");
    }
}
