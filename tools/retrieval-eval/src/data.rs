//! Dataset loading: GooAQ question/answer pairs (network, cached under
//! target/gooaq/) and the checked-in paraphrase fixture (offline).

use anyhow::{Context, Result, bail, ensure};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use std::fs;
use std::path::{Path, PathBuf};

/// Same files the ese gooaq bench uses — the `pair` config carries both the
/// question and its answer, i.e. a labeled (query, positive-doc) set.
const PARQUET_URLS: &[&str] = &[
    "https://huggingface.co/api/datasets/sentence-transformers/gooaq/parquet/pair/train/0.parquet",
    "https://huggingface.co/api/datasets/sentence-transformers/gooaq/parquet/pair/train/1.parquet",
];

/// A query set over a document corpus. Alignment is the gold labeling:
/// the relevant doc for `questions[i]` is `answers[i]`. `answers` may be
/// longer than `questions` — the tail is distractor documents.
pub struct Pairs {
    pub questions: Vec<String>,
    pub answers: Vec<String>,
}

/// Shares the ese bench's cache when run from the repo root, but resolves
/// CWD-independently via the manifest dir.
fn cache_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gooaq")
}

fn download(url: &str, path: &Path) -> Result<()> {
    eprintln!("downloading {url} ...");
    let resp = minreq::get(url)
        .with_timeout(600)
        .send()
        .with_context(|| format!("download failed: {url}"))?;
    // an unchecked write would cache an error body forever
    ensure!(
        resp.status_code == 200,
        "HTTP {} for {url}",
        resp.status_code
    );
    fs::write(path, resp.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn extract_pairs(path: &Path) -> Result<Vec<(String, String)>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = SerializedFileReader::new(file).context("parquet reader")?;
    let mut pairs = Vec::new();
    for row in reader.get_row_iter(None).context("row iterator")? {
        let row = row.context("read row")?;
        let mut q = None;
        let mut a = None;
        for (name, field) in row.get_column_iter() {
            if let Field::Str(s) = field {
                match name.as_str() {
                    "question" | "sentence1" => q = Some(s.clone()),
                    "answer" | "sentence2" => a = Some(s.clone()),
                    _ => {}
                }
            }
        }
        if let (Some(q), Some(a)) = (q, a) {
            pairs.push((q, a));
        }
    }
    Ok(pairs)
}

pub fn load_gooaq() -> Result<Vec<(String, String)>> {
    let dir = cache_dir();
    fs::create_dir_all(&dir)?;
    let mut all = Vec::new();
    for (i, url) in PARQUET_URLS.iter().enumerate() {
        let cached = dir.join(format!("gooaq_{i}.parquet"));
        if !cached.exists() {
            download(url, &cached)?;
        }
        all.extend(extract_pairs(&cached)?);
    }
    ensure!(!all.is_empty(), "gooaq dataset is empty");
    eprintln!("loaded {} gooaq pairs", all.len());
    Ok(all)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Deterministic sample: dedupe by exact answer text (GooAQ repeats answers
/// across questions — with single-positive gold, a duplicate answer ranked
/// above the paired one would count as a miss), seeded shuffle, take
/// `n_docs` pairs as the corpus with the first `n_queries` as queries.
pub fn sample_pairs(
    all: Vec<(String, String)>,
    n_docs: usize,
    n_queries: usize,
    seed: u64,
) -> Result<Pairs> {
    ensure!(n_queries <= n_docs, "need n_queries <= n_docs");
    let mut seen = fxhash::FxHashSet::default();
    let mut pairs: Vec<(String, String)> = all
        .into_iter()
        .filter(|(q, a)| !q.trim().is_empty() && !a.trim().is_empty() && seen.insert(a.clone()))
        .collect();
    ensure!(
        pairs.len() >= n_docs,
        "only {} unique pairs, need {n_docs}",
        pairs.len()
    );
    let mut s = seed ^ 0x243F6A8885A308D3;
    for i in (1..pairs.len()).rev() {
        let j = (splitmix64(&mut s) as usize) % (i + 1);
        pairs.swap(i, j);
    }
    pairs.truncate(n_docs);
    let questions = pairs[..n_queries].iter().map(|(q, _)| q.clone()).collect();
    let answers = pairs.into_iter().map(|(_, a)| a).collect();
    Ok(Pairs { questions, answers })
}

/// The offline smoke corpus: prose passages plus two tagged query
/// workloads — `paraphrase` (minimal lexical overlap, the documented ese
/// weak spot) and `known-item` (short remembered-phrase lookups whose words
/// appear in the gold doc). `kind: None` loads every query; the corpus is
/// always all docs, so per-kind runs are comparable.
pub fn load_fixture(kind: Option<&str>) -> Result<Pairs> {
    let raw: serde_json::Value = serde_json::from_str(include_str!("../fixtures/paraphrase.json"))
        .context("parse fixtures/paraphrase.json")?;
    let docs = raw["docs"].as_array().context("fixture: docs array")?;
    let queries = raw["queries"]
        .as_array()
        .context("fixture: queries array")?;

    let mut ids = Vec::new();
    let mut texts = Vec::new();
    for d in docs {
        let id = d["id"].as_str().context("fixture: doc id")?;
        let text = d["text"].as_str().context("fixture: doc text")?;
        ids.push(id.to_string());
        texts.push(text.to_string());
    }

    // reorder so gold doc i pairs with query i, distractors after
    let mut questions = Vec::new();
    let mut ordered = Vec::new();
    let mut taken = vec![false; ids.len()];
    let mut included = vec![false; ids.len()];
    for q in queries {
        let text = q["q"].as_str().context("fixture: query text")?;
        let gold = q["gold"].as_str().context("fixture: query gold")?;
        let qkind = q["kind"].as_str().context("fixture: query kind")?;
        ensure!(
            matches!(qkind, "paraphrase" | "known-item"),
            "fixture: unknown kind {qkind:?}"
        );
        let Some(i) = ids.iter().position(|id| id == gold) else {
            bail!("fixture: gold id {gold:?} not in docs");
        };
        ensure!(!taken[i], "fixture: doc {gold:?} is gold for two queries");
        taken[i] = true;
        if kind.is_none_or(|k| k == qkind) {
            included[i] = true;
            questions.push(text.to_string());
            ordered.push(texts[i].clone());
        }
    }
    for (i, t) in texts.iter().enumerate() {
        if !included[i] {
            ordered.push(t.clone());
        }
    }
    Ok(Pairs {
        questions,
        answers: ordered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_parses_and_gold_ids_resolve() {
        let p = load_fixture(None).expect("fixture must load");
        assert!(p.questions.len() >= 40, "want a meaningful query battery");
        assert!(p.answers.len() >= p.questions.len());
        // answers must be unique — duplicate docs would poison the gold labels
        let uniq: fxhash::FxHashSet<&String> = p.answers.iter().collect();
        assert_eq!(uniq.len(), p.answers.len());
    }

    #[test]
    fn fixture_kind_filter_keeps_full_corpus() {
        let all = load_fixture(None).unwrap();
        let para = load_fixture(Some("paraphrase")).unwrap();
        let known = load_fixture(Some("known-item")).unwrap();
        // every workload searches the same corpus, only the query set changes
        assert_eq!(para.answers.len(), all.answers.len());
        assert_eq!(known.answers.len(), all.answers.len());
        assert_eq!(
            para.questions.len() + known.questions.len(),
            all.questions.len()
        );
        assert!(known.questions.len() >= 20);
        // gold alignment survives the filter: query i's gold is answer i
        assert!(para.questions.len() < para.answers.len());
    }

    #[test]
    fn known_item_queries_share_words_with_gold_doc() {
        // the whole point of the known-item workload: its words appear in
        // the gold doc, so the lexical ranker has something to match
        let known = load_fixture(Some("known-item")).unwrap();
        for (q, gold) in known.questions.iter().zip(&known.answers) {
            let gl = gold.to_lowercase();
            let mut words = q.split_whitespace();
            assert!(
                words.all(|w| gl.contains(&w.to_lowercase())),
                "known-item query {q:?} has words missing from its gold doc"
            );
        }
    }

    #[test]
    fn sample_is_deterministic_and_deduped() {
        let all: Vec<(String, String)> = (0..100)
            .map(|i| (format!("q{i}"), format!("a{}", i % 50))) // every answer twice
            .collect();
        let a = sample_pairs(all.clone(), 20, 5, 42).unwrap();
        let b = sample_pairs(all, 20, 5, 42).unwrap();
        assert_eq!(a.questions, b.questions);
        assert_eq!(a.answers, b.answers);
        let uniq: fxhash::FxHashSet<&String> = a.answers.iter().collect();
        assert_eq!(uniq.len(), 20);
    }
}
