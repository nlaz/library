//! TermDict: a fold terminal holding every live term under raw UTF-8 keys, so
//! the trailing (partial) token of a query can be expanded with a prefix scan.
//! Bm25 can't do this: postcard string keys are length-prefixed.

use fold::pipeline::Push;
use fold::stream::{PipelineInitCtx, Readable, WriteTx};
use fxhash::FxHashMap;

/// Max edit distance for fuzzy term correction ([`TermDictReader::correct`]).
pub(crate) const MAX_FUZZ_DIST: usize = 2;

/// Levenshtein edit distance, capped at `max`: returns `max + 1` as soon as it
/// is certain the true distance exceeds `max` (callers only care about the
/// `<= max` band), keeping each comparison cheap.
pub(crate) fn levenshtein(a: &str, b: &str, max: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > max {
        return max + 1;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut cur = vec![0usize; lb + 1];
    for i in 1..=la {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=lb {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return max + 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[lb]
}

pub struct TermDict {
    name: String,
    ks: Option<fjall::SingleWriterTxKeyspace>,
    pending: FxHashMap<Vec<u8>, i64>,
}

impl TermDict {
    pub fn new(name: impl Into<String>) -> Self {
        TermDict {
            name: name.into(),
            ks: None,
            pending: FxHashMap::default(),
        }
    }
}

pub struct TermDictReader<'tx, R: Readable> {
    tx: &'tx R,
    ks: fjall::SingleWriterTxKeyspace,
}

impl<R: Readable> TermDictReader<'_, R> {
    /// Up to `k` live terms starting with `prefix`, lexicographic.
    pub fn complete(&self, prefix: &str, k: usize) -> Vec<String> {
        self.tx
            .prefix(&self.ks, prefix.as_bytes())
            .take(k)
            .map(|kv| String::from_utf8(kv.key().unwrap().to_vec()).unwrap())
            .collect()
    }

    /// Up to `k` live terms starting with `prefix`, ranked by corpus
    /// frequency (descending), ties broken lexicographically for
    /// determinism. Unlike [`complete`](Self::complete) — which takes the
    /// first `k` lexicographically and is right for query-time term
    /// expansion — this is for user-facing type-ahead, where the most common
    /// completions are what a human wants to see first. Scans at most
    /// `SCAN_CAP` matching terms so a 1-char prefix can't walk the whole
    /// keyspace.
    pub fn complete_ranked(&self, prefix: &str, k: usize) -> Vec<String> {
        const SCAN_CAP: usize = 2000;
        let mut cands: Vec<(i64, String)> = self
            .tx
            .prefix(&self.ks, prefix.as_bytes())
            .take(SCAN_CAP)
            .map(|kv| {
                let (key, val) = kv.into_inner().unwrap();
                let freq = i64::from_be_bytes(val.as_ref().try_into().unwrap());
                (freq, String::from_utf8(key.to_vec()).unwrap())
            })
            .collect();
        cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        cands.into_iter().take(k).map(|(_, t)| t).collect()
    }

    /// Whether `term` is a live vocabulary term (exact match). Used to decide
    /// if a query token needs fuzzy correction.
    pub fn contains(&self, term: &str) -> bool {
        self.tx.get(&self.ks, term.as_bytes()).unwrap().is_some()
    }

    /// Up to `k` live terms within edit distance [`MAX_FUZZ_DIST`] of `token`,
    /// nearest first (ties broken by higher corpus frequency). Scans only the
    /// term-dict bucket sharing `token`'s first two characters — typos and OCR
    /// errors overwhelmingly preserve leading characters — so it never walks
    /// the whole vocabulary. This is the fuzzy-correction primitive: an unknown
    /// query word is replaced by its nearest real words, which then feed the
    /// exact lexical index (no document-scale fuzzy index needed). Limitation:
    /// a corruption in the first two characters is not recovered.
    pub fn correct(&self, token: &str, k: usize) -> Vec<String> {
        const SCAN_CAP: usize = 4000;
        let prefix: String = token.chars().take(2).collect();
        let mut cands: Vec<(usize, i64, String)> = self
            .tx
            .prefix(&self.ks, prefix.as_bytes())
            .take(SCAN_CAP)
            .filter_map(|kv| {
                let (key, val) = kv.into_inner().unwrap();
                let term = String::from_utf8(key.to_vec()).ok()?;
                if term == token {
                    return None; // an exact match isn't a correction
                }
                let d = levenshtein(token, &term, MAX_FUZZ_DIST);
                (d <= MAX_FUZZ_DIST).then(|| {
                    let freq = i64::from_be_bytes(val.as_ref().try_into().unwrap());
                    (d, freq, term)
                })
            })
            .collect();
        cands.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        cands.into_iter().take(k).map(|(_, _, t)| t).collect()
    }
}

impl Push<String> for TermDict {
    type Reader<'tx, R: Readable + 'tx> = TermDictReader<'tx, R>;

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.ks = Some(init.keyspace(&self.name));
    }

    fn push(&mut self, _tx: &mut WriteTx<'_>, data: &String, delta: isize) {
        *self.pending.entry(data.as_bytes().to_vec()).or_insert(0) += delta as i64;
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        let ks = self.ks.clone().unwrap();
        for (key, delta) in self.pending.drain() {
            if delta == 0 {
                continue;
            }
            let cur = tx
                .get(&ks, &key)
                .map(|v| i64::from_be_bytes(v.as_ref().try_into().unwrap()))
                .unwrap_or(0);
            let new = cur + delta;
            if new > 0 {
                tx.insert(&ks, &key, new.to_be_bytes());
            } else {
                tx.remove(&ks, &key);
            }
        }
    }

    fn abort(&mut self) {
        self.pending.clear();
    }

    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        TermDictReader {
            tx,
            ks: self.ks.clone().unwrap(),
        }
    }
}

#[cfg(test)]
mod termdict_tests {
    use super::*;
    use crate::{ChunkKey, ChunkRec, EMB_DIM, Library, Word, open};

    /// Temp-dir store per test, cleaned from any earlier run — the same
    /// pattern as fold's `fresh_db`.
    fn fresh(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("library-core-termdict-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn chunk(doc: &str, idx: u32, text: &str) -> (ChunkKey, ChunkRec) {
        let key = ChunkKey {
            doc: doc.to_string(),
            page: 1,
            idx,
        };
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
        let mut emb = [0.0f32; EMB_DIM];
        emb[idx as usize % EMB_DIM] = 1.0;
        (key.clone(), ChunkRec { key, words, emb })
    }

    /// escapement x3, escape x1, plus unrelated vocabulary.
    fn sample_library(name: &str) -> Library {
        let mut lib = open(fresh(name));
        lib.wtx(|tx| {
            for (i, text) in [
                "escapement escapement escapement",
                "escape chain",
                "rhodium watch",
            ]
            .iter()
            .enumerate()
            {
                let (k, r) = chunk("horology", i as u32, text);
                tx.upsert(&k, &r);
            }
        });
        lib
    }

    #[test]
    fn complete_returns_prefix_matches_capped_at_k() {
        let lib = sample_library("complete");
        lib.rtx(|((_, _), (_, terms))| {
            // lexicographic: "escape" < "escapement"
            assert_eq!(terms.complete("esc", 10), vec!["escape", "escapement"]);
            assert_eq!(terms.complete("esc", 1), vec!["escape"]);
            assert_eq!(terms.complete("zz", 5), Vec::<String>::new());
        });
    }

    #[test]
    fn complete_ranked_prefers_frequent_terms() {
        let lib = sample_library("ranked");
        lib.rtx(|((_, _), (_, terms))| {
            // corpus frequency beats lexicographic order
            assert_eq!(
                terms.complete_ranked("esc", 2),
                vec!["escapement", "escape"]
            );
            // the empty prefix matches the whole (capped) vocabulary
            assert_eq!(terms.complete("", 3).len(), 3);
        });
    }

    #[test]
    fn correct_recovers_single_edit_typo() {
        let lib = sample_library("correct");
        lib.rtx(|((_, _), (_, terms))| {
            // missing letter (the classic OCR/typo case)
            assert_eq!(terms.correct("escapment", 3), vec!["escapement"]);
            // never proposes the token itself, and respects MAX_FUZZ_DIST:
            // "escape" -> "escapement" is 4 edits, out of range
            assert_eq!(terms.correct("escape", 3), Vec::<String>::new());
            // corruption in the first two chars is out of scope by design
            assert_eq!(terms.correct("zscapement", 3), Vec::<String>::new());
        });
    }

    #[test]
    fn contains_is_exact_only() {
        let lib = sample_library("contains");
        lib.rtx(|((_, _), (_, terms))| {
            assert!(terms.contains("escape"));
            assert!(terms.contains("rhodium"));
            assert!(!terms.contains("esc"));
            assert!(!terms.contains("escapements"));
        });
    }

    #[test]
    fn termdict_forgets_terms_of_removed_chunks() {
        let mut lib = sample_library("retract");
        let (k, _) = chunk("horology", 2, "");
        lib.wtx(|tx| {
            tx.remove(&k);
        });
        lib.rtx(|((_, _), (_, terms))| {
            assert!(!terms.contains("rhodium"), "retracted term still live");
            assert!(terms.contains("escapement"), "unrelated term lost");
        });
    }

    #[test]
    fn levenshtein_is_bounded() {
        assert_eq!(levenshtein("escapement", "escapement", 2), 0);
        assert_eq!(levenshtein("escapment", "escapement", 2), 1); // one deletion
        assert_eq!(levenshtein("escaprnent", "escapement", 2), 2); // OCR rn->m
        assert_eq!(levenshtein("abc", "abd", 2), 1);
        // beyond the cap: reports max+1, not the true distance (3)
        assert_eq!(levenshtein("kitten", "sitting", 2), 3);
    }
}
