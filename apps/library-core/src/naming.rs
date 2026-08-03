//! What to call a book nobody has named.
//!
//! Document ids are minted — opaque, so a Finder rename can't orphan a book
//! — which leaves the file's own name as the only thing an untitled book
//! can be called. Taken raw it is close, but a downloaded library is full
//! of names no one chose: `Il Cucchiaio (z-library.sk, 1lib.sk).pdf`,
//! `escoffier_ocr.pdf`, `the-art-of-plain-cookery.pdf`, `Some Book (1).pdf`.
//!
//! The rules here are deliberately timid, because the name on screen has to
//! stay the name in Finder — a shelf that renames a user's files out from
//! under them is worse than one that shows a stray tag:
//!
//! - **Only junk comes off.** A parenthetical is dropped when everything
//!   inside it is a domain, a site's name or a format word; `(2nd ed.)`,
//!   `(Vol. 2)` and `(A. Botanist)` stay, because they are how a person
//!   tells two copies apart.
//! - **Separators are expanded only where a person didn't type them.** If
//!   the name has a space in it, its hyphens and dots are somebody's
//!   punctuation and are left alone. Only a space-free name is read as a
//!   slug, and even then a lone hyphen (`moby-dick`) is treated as a
//!   compound rather than a word break.
//! - **Word order is never changed.** Telling `Julia Child - Mastering the
//!   Art` from `Mastering the Art - Julia Child` takes a guess about which
//!   half is a person, and a wrong guess shows a book under a name its
//!   owner can't find.
//! - **Gibberish is passed through untouched.** Scan-farm ids
//!   (`fringesofreasonw00unse`, `bub_gb_wTLLxvVeyEIC`) have no words to
//!   recover; [`is_opaque`] marks them so a caller with a better source —
//!   embedded PDF metadata, a title page — knows it is allowed to win.
//!
//! Nothing here is stored. The name is derived on read, so improving these
//! rules improves every existing library without a migration.

use std::path::Path;

/// Vowels, including the accented ones a European title carries. Used only
/// to judge whether a run of letters is pronounceable.
const VOWELS: &str = "aeiouyàáâäãåèéêëìíîïòóôöõùúûüæøœ";

/// Extensions worth stripping a second time: `book.pdf.pdf` stems to
/// `book.pdf`, and a download that arrived as `book.epub.pdf` is not
/// telling us anything a reader wants on the shelf.
const DOC_EXTS: &[&str] = &[
    "pdf", "epub", "djvu", "mobi", "azw3", "cbz", "cbr", "txt", "md", "doc", "docx",
];

/// Words that are a file's provenance, not a book's name: the sites that
/// serve scanned books, the formats they serve them in, and the marks a
/// pipeline leaves behind. Matched against a token stripped to its letters
/// and digits, so `Z-Library`, `z_library` and `zlibrary` are one entry.
const JUNK_WORDS: &[&str] = &[
    "zlib",
    "zlibrary",
    "zlibraryorg",
    "libgen",
    "librarygenesis",
    "annasarchive",
    "bok",
    "bibliotik",
    "pdfdrive",
    "ebook",
    "ebooks",
    "retail",
    "ocr",
    "ocrd",
    "scan",
    "scanned",
    "compressed",
    "optimized",
    "dragged",
    "copy",
    "pdf",
    "epub",
    "djvu",
    "mobi",
    "azw3",
    "cbz",
];

/// Marks a pipeline welds onto the end of a name with an underscore or a
/// hyphen — `Mastering the Art of French Cooking_text`, `escoffier-ocr`.
/// Only stripped in that position, which is what keeps a book called
/// `The Text` or `Copy of a Letter` from being cut down mid-title.
const SUFFIX_MARKS: &[&str] = &[
    "text",
    "ocr",
    "ocrd",
    "clean",
    "cleaned",
    "scan",
    "scanned",
    "compressed",
    "optimized",
    "copy",
];

/// The name to show for a file at `relpath`, relative to its watched root.
///
/// The last path component, minus its extension, cleaned by the rules in
/// this module's header. Never empty: a name that cleans away to nothing
/// falls back to the untouched stem.
pub fn from_path(relpath: &str) -> String {
    let stem = Path::new(relpath)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| relpath.to_string());
    from_stem(&strip_doubled_ext(&stem))
}

/// [`from_path`] for something that is already a bare name — a stem a
/// caller split itself.
pub fn from_stem(stem: &str) -> String {
    clean(stem, false)
}

/// [`from_stem`] for a string known to be a slug rather than a filename: a
/// document id minted before 0.2, when ids were made by slugging the file's
/// name. Every hyphen in one is a word break the slugging put there, so
/// `moby-dick` is two words here and one compound in [`from_stem`].
pub fn from_slug(slug: &str) -> String {
    clean(slug, true)
}

fn clean(stem: &str, slug: bool) -> String {
    let dropped = drop_junk_groups(stem);
    let dropped = strip_suffix_marks(dropped.trim());
    let s = dropped.trim();
    if s.is_empty() {
        return stem.to_string();
    }
    // gibberish has no words to rescue; anything we did to it would only
    // make it a different gibberish
    if is_opaque(s) {
        return s.to_string();
    }
    let (mut tokens, split) = tokenize(s, slug);
    strip_junk_edges(&mut tokens);
    if tokens.is_empty() {
        return s.to_string();
    }
    // a bare lowercase word — `escoffier.pdf`, or what `escoffier_ocr.pdf`
    // is left as — reads as a name once it has its capital. Only a bare
    // one: `moby-dick` keeps its own shape.
    let split = split
        || (tokens.len() == 1
            && tokens[0].chars().all(char::is_alphabetic)
            && !tokens[0].chars().any(char::is_uppercase));
    let out = if split {
        // a slug carries no capitalization of its own. The short-word guard
        // keeps `a-history-of-tea` off `A History Of Tea` — but the first
        // word is capitalized whatever its length, because `a History of
        // Tea` reads as a mistake on a shelf
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| if i == 0 { upcase(t) } else { capitalize(t) })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        tokens.join(" ")
    };
    let out = out.trim_matches(|c: char| c.is_whitespace() || "-_.,;".contains(c));
    if out.is_empty() {
        s.to_string()
    } else {
        out.to_string()
    }
}

/// Whether a name has no readable words in it — a minted id
/// (`D01713FA82AD0`), a scan-farm id (`fringesofreasonw00unse`), a hash.
///
/// Public because it is the seam for better sources: a caller holding a
/// title from PDF metadata or a title page can ask whether the filename is
/// worth preferring, and an opaque one never is.
pub fn is_opaque(name: &str) -> bool {
    // a space means a person typed this, and a person's name for a thing is
    // not for us to second-guess
    if name.contains(char::is_whitespace) {
        return false;
    }
    // a one-character segment is evidence of nothing — `doc-a` and
    // `a-history-of-tea` are both perfectly readable
    let segs: Vec<&str> = name
        .split(['-', '_', '.'])
        .filter(|s| s.chars().count() > 1)
        .collect();
    match segs.len() {
        0 => name.chars().count() > 1,
        1 => !is_wordish(segs[0]),
        // a slug reads as words: `bub_gb_wTLLxvVeyEIC` doesn't, and neither
        // does `b29326679_0002`
        _ => segs.iter().filter(|s| is_wordish(s)).count() * 2 <= segs.len(),
    }
}

/// `book.pdf` (from `book.pdf.pdf`) -> `book`. Only known document
/// extensions, so `Vol.2` and `Il Cucchiaio (1lib.sk)` keep their dots.
fn strip_doubled_ext(stem: &str) -> String {
    match stem.rsplit_once('.') {
        Some((head, ext))
            if !head.is_empty() && DOC_EXTS.contains(&ext.to_ascii_lowercase().as_str()) =>
        {
            head.to_string()
        }
        _ => stem.to_string(),
    }
}

/// Peel `_text`, `-ocr` and friends off the end, however many are stacked
/// there. Stops at a head of fewer than two characters, so a name can never
/// be stripped away to nothing.
fn strip_suffix_marks(s: &str) -> String {
    let mut t = s;
    while let Some(cut) = t.rfind(['_', '-']) {
        let (head, tail) = t.split_at(cut);
        if head.chars().count() < 2 || !SUFFIX_MARKS.contains(&normalize(&tail[1..]).as_str()) {
            break;
        }
        t = head.trim_end();
    }
    t.to_string()
}

/// Drop `(...)`/`[...]` groups whose whole contents are junk. An unbalanced
/// opener ends the scan — the rest is text, not a group.
fn drop_junk_groups(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find(['(', '[']) {
        let (before, from_open) = rest.split_at(open);
        let closer = if from_open.starts_with('(') { ')' } else { ']' };
        let Some(close) = from_open.find(closer) else {
            break;
        };
        out.push_str(before);
        if !is_junk_group(&from_open[1..close]) {
            out.push_str(&from_open[..=close]);
        }
        rest = &from_open[close + 1..];
    }
    out.push_str(rest);
    out
}

/// A group is junk when *every* comma-separated part of it is: one real
/// word in there (`(A. Botanist, ed.)`) and the whole group stays.
fn is_junk_group(inner: &str) -> bool {
    let parts: Vec<&str> = inner
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    !parts.is_empty() && parts.iter().all(|p| is_junk_part(p))
}

fn is_junk_part(p: &str) -> bool {
    if is_domain(p) {
        return true;
    }
    let norm = normalize(p);
    if norm.is_empty() {
        return false;
    }
    // `(1)`, `(2)` — a downloader's duplicate marker. Longer digit runs are
    // left alone: `(1911)` is an edition a reader wants to see.
    if norm.len() <= 2 && norm.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    JUNK_WORDS.contains(&norm.as_str())
}

/// `z-library.sk`, `1lib.sk`, `annas-archive.org`. The lowercase-TLD rule
/// keeps `Vol.2` (no letters after the dot) and `Mr.Smith` out of it.
fn is_domain(p: &str) -> bool {
    let Some((host, tld)) = p.rsplit_once('.') else {
        return false;
    };
    !host.is_empty()
        && !p.contains(char::is_whitespace)
        && (2..=8).contains(&tld.chars().count())
        && tld.chars().all(|c| c.is_ascii_lowercase())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Split into display tokens. Returns whether the split was a *slug* split
/// — the only case where we get to impose capitalization, since a name with
/// spaces in it already carries whatever case its owner wanted.
fn tokenize(s: &str, slug: bool) -> (Vec<String>, bool) {
    if s.contains(char::is_whitespace) {
        return (s.split_whitespace().map(str::to_string).collect(), false);
    }
    let count = |c: char| s.matches(c).count();
    let mut seps = Vec::new();
    // an underscore is never punctuation somebody wanted to read
    if count('_') > 0 {
        seps.push('_');
    }
    // in a filename one hyphen is a compound (`moby-dick`, `anti-oedipus`)
    // and three segments or more is a slug; in a slug every hyphen is a
    // word break by construction
    if count('-') > 0 && (slug || count('-') >= 2) {
        seps.push('-');
    }
    // `the.art.of.plain.cookery`, but not `Vol.2` and not `9.7 Theses`
    if count('.') >= 2 && s.split('.').filter(|p| !p.is_empty()).all(is_wordish) {
        seps.push('.');
    }
    if seps.is_empty() {
        return (vec![s.to_string()], false);
    }
    (
        s.split(|c| seps.contains(&c))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        true,
    )
}

/// Take junk off both ends — `A Field Guide - z-library.sk`, `escoffier ocr`,
/// `Cooking 9780140449136`, `Some Book copy 2`. Never takes the last token:
/// a name of nothing but junk is still the name on disk.
fn strip_junk_edges(tokens: &mut Vec<String>) {
    while tokens.len() > 1 {
        let last = tokens.len() - 1;
        // `copy 2` — the count belongs to the junk word before it
        if tokens.len() > 2
            && tokens[last].chars().count() <= 2
            && tokens[last].chars().all(|c| c.is_ascii_digit())
            && JUNK_WORDS.contains(&normalize(&tokens[last - 1]).as_str())
        {
            tokens.truncate(last - 1);
            continue;
        }
        if is_junk_token(&tokens[last]) {
            tokens.truncate(last);
            continue;
        }
        if is_junk_token(&tokens[0]) {
            tokens.remove(0);
            continue;
        }
        break;
    }
}

fn is_junk_token(t: &str) -> bool {
    let norm = normalize(t);
    if norm.is_empty() {
        return true; // a stray dash left behind by a dropped tag
    }
    if is_domain(t.trim_matches(|c: char| !c.is_alphanumeric())) {
        return true;
    }
    if JUNK_WORDS.contains(&norm.as_str()) {
        return true;
    }
    // an ISBN — exactly ten or thirteen digits, so an author's dates
    // (`1924-2003`) and a year are left where they are
    if norm.chars().all(|c| c.is_ascii_digit()) {
        return matches!(norm.chars().count(), 10 | 13);
    }
    // the hash a converter stamped on. Letters *and* digits, both required:
    // hex without a digit is a word (`deadbeef`), hex without a letter is a
    // number somebody meant
    norm.chars().count() >= 8
        && norm.chars().any(|c| c.is_ascii_digit())
        && norm.chars().any(|c| c.is_ascii_alphabetic())
        && norm.chars().all(|c| c.is_ascii_hexdigit())
}

/// Lowercase letters and digits only, for comparing a token to a word list.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a run of characters reads as a word. A year counts — a slug's
/// `gardening-encyclopedia-1911` is three words, not two and a serial.
fn is_wordish(w: &str) -> bool {
    let lower: String = w.chars().flat_map(char::to_lowercase).collect();
    let n = lower.chars().count();
    if n < 2 {
        return false;
    }
    if lower.chars().all(|c| c.is_ascii_digit()) {
        // a year, and only a year: `0002` is a volume number off a scanner
        return n == 4 && lower.starts_with(['1', '2']);
    }
    if !lower.chars().all(char::is_alphabetic) {
        return false;
    }
    let mut vowels = 0;
    let mut run = 0;
    for c in lower.chars() {
        if VOWELS.contains(c) {
            vowels += 1;
            run = 0;
        } else {
            run += 1;
            // no word has four consonants in a row; an id does
            if run >= 4 {
                return false;
            }
        }
    }
    vowels > 0
}

/// The slug rule the shelf has always used: capitalize a word, leave the
/// short ones (`of`, `a`, `to`) as they were.
fn capitalize(w: &str) -> String {
    if w.chars().count() > 2 {
        upcase(w)
    } else {
        w.to_string()
    }
}

fn upcase(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_a_person_typed_survives_intact() {
        assert_eq!(from_path("Artusi 1891.pdf"), "Artusi 1891");
        // the hyphen and the case are somebody's; `Author - Title` is not
        // reordered, because guessing which half is the author is a guess
        assert_eq!(
            from_path("Julia Child - Mastering the Art.pdf"),
            "Julia Child - Mastering the Art"
        );
        // one hyphen reads as a compound, not a word break
        assert_eq!(from_path("moby-dick.pdf"), "moby-dick");
        // a dot that isn't an extension stays put
        assert_eq!(from_path("cookbooks/Vol.2.pdf"), "Vol.2");
    }

    #[test]
    fn download_site_tags_come_off() {
        assert_eq!(
            from_path("cookbooks/Il Cucchiaio (z-library.sk, 1lib.sk).pdf"),
            "Il Cucchiaio"
        );
        assert_eq!(
            from_path("A Taste of India (Z-Library).pdf"),
            "A Taste of India"
        );
        assert_eq!(from_path("Larousse [libgen].pdf"), "Larousse");
        // a bare domain at the end, and the dash that introduced it
        assert_eq!(
            from_path("A Field Guide - z-library.sk.pdf"),
            "A Field Guide"
        );
        // the duplicate marker a browser adds, but not an edition year
        assert_eq!(from_path("Some Book (1).pdf"), "Some Book");
        assert_eq!(from_path("Some Book (1911).pdf"), "Some Book (1911)");
    }

    #[test]
    fn a_parenthetical_a_reader_needs_stays() {
        assert_eq!(from_path("Cooking (2nd ed.).pdf"), "Cooking (2nd ed.)");
        assert_eq!(from_path("Escoffier (Vol. 2).pdf"), "Escoffier (Vol. 2)");
        assert_eq!(
            from_path("Field Guide (A. Botanist) (source.example).pdf"),
            "Field Guide (A. Botanist)"
        );
    }

    #[test]
    fn pipeline_marks_and_serials_come_off_the_ends() {
        assert_eq!(from_path("escoffier_ocr.pdf"), "Escoffier");
        assert_eq!(
            from_path("Le Guide Culinaire copy 2.pdf"),
            "Le Guide Culinaire"
        );
        assert_eq!(from_path("Cooking 9780140449136.pdf"), "Cooking");
        // a bare lowercase word gets the capital a shelf wants
        assert_eq!(from_path("book.pdf.pdf"), "Book");
        // stripping never empties a name: junk on disk is still the name on
        // disk, and an empty shelf row helps nobody
        assert_eq!(from_path("ocr.pdf"), "Ocr");
    }

    #[test]
    fn a_slug_becomes_words() {
        assert_eq!(
            from_stem("the-art-of-plain-cookery"),
            "The Art of Plain Cookery"
        );
        // short words stay short, except the first one
        assert_eq!(from_stem("a-history-of-tea"), "A History of Tea");
        assert_eq!(
            from_path("the.art.of.plain.cookery.pdf"),
            "The Art of Plain Cookery"
        );
        assert_eq!(
            from_path("gardening_encyclopedia_1911.pdf"),
            "Gardening Encyclopedia 1911"
        );
    }

    #[test]
    fn a_slug_reads_its_hyphens_differently_from_a_filename() {
        // the same string, and both answers are right: a person typed the
        // hyphen in `moby-dick.pdf`, and the slugger put the one in the id
        assert_eq!(from_stem("moby-dick"), "moby-dick");
        assert_eq!(from_slug("moby-dick"), "Moby Dick");
        // a one-letter word doesn't make a name gibberish
        assert_eq!(from_slug("doc-a"), "Doc a");
    }

    #[test]
    fn gibberish_is_left_exactly_as_it_is() {
        // a scan-farm id: no words in there to recover
        assert_eq!(
            from_path("fringesofreasonw00unse.pdf"),
            "fringesofreasonw00unse"
        );
        assert_eq!(from_path("bub_gb_wTLLxvVeyEIC.pdf"), "bub_gb_wTLLxvVeyEIC");
        assert_eq!(from_path("b29326679_0002.pdf"), "b29326679_0002");
        // ...but a tag on gibberish still comes off
        assert_eq!(
            from_path("fringesofreasonw00unse (z-lib.org).pdf"),
            "fringesofreasonw00unse"
        );
    }

    /// Names taken off a real shelf, which is where the rules above come
    /// from: the tags a download site staples on, an author parenthetical
    /// worth keeping, a pipeline's suffix, a slug, and two ids with nothing
    /// in them to read.
    #[test]
    fn a_real_shelf() {
        for (file, want) in [
            (
                "A taste of India (Jaffrey, Madhur) (z-library.sk, 1lib.sk, z-lib.sk).pdf",
                "A taste of India (Jaffrey, Madhur)",
            ),
            (
                "Julia Child - Mastering the Art of French Cooking_text.pdf",
                "Julia Child - Mastering the Art of French Cooking",
            ),
            (
                "The Talisman Italian Cook Book - Boni, Ada.pdf",
                "The Talisman Italian Cook Book - Boni, Ada",
            ),
            // a double space is the one bit of a typed name we do tidy
            (
                "Fish  Shellfish (James Peterson) (z-library.sk, 1lib.sk).pdf",
                "Fish Shellfish (James Peterson)",
            ),
            // the author's dates are not a serial number
            (
                "North Atlantic seafood (Davidson, Alan, 1924-2003) (z-lib.sk).pdf",
                "North Atlantic seafood (Davidson, Alan, 1924-2003)",
            ),
            ("a-guide-to-modern-cookery.pdf", "A Guide to Modern Cookery"),
            // run-together words are not a slug: there is no way back
            (
                "DictionnaireLarousseGastronomique.pdf",
                "DictionnaireLarousseGastronomique",
            ),
            ("Papers/cerf74.pdf", "cerf74"),
            ("fringesofreasonw00unse.pdf", "fringesofreasonw00unse"),
        ] {
            assert_eq!(from_path(file), want, "naming {file}");
        }
    }

    #[test]
    fn is_opaque_marks_what_a_better_source_may_overrule() {
        assert!(is_opaque("D01713FA82AD0"));
        assert!(is_opaque("fringesofreasonw00unse"));
        assert!(is_opaque("bub_gb_wTLLxvVeyEIC"));
        assert!(!is_opaque("Artusi 1891"));
        assert!(!is_opaque("the-art-of-plain-cookery"));
        assert!(!is_opaque("gardening-encyclopedia-1911"));
    }
}
