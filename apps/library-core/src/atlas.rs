//! Corpus atlas: recurring themes and cross-book throughlines, computed
//! from the store's own chunk embeddings and cached to `data/atlas.json`.
//!
//! The atlas is a *global* function of every vector in the library —
//! clustering, projection, trails — so it cannot ride the fold graph
//! (fold operators are per-delta with exact retraction). Instead the whole
//! thing recomputes from scratch: the k-NN sweep reuses the HNSW index, so
//! a full build is tens of seconds, cheap enough to run in the background
//! after an ingest sweep and lazily when the view first opens. Freshness is
//! a [`Fingerprint`] over (doc id, chunk count) pairs; a build that races a
//! commit records the pre-commit fingerprint and simply reads as stale.
//!
//! Term extraction uses a private tokenizer on purpose: the
//! `tokenize`/`lex_tokenize` pair in [`crate::text`] is a pinned contract
//! between the term dictionary and BM25 postings, and the atlas has no such
//! coupling — do not unify them.
//!
//! The build writes exactly one path, [`sidecar_path`], via tmp + rename.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::meta::Ctx;
use crate::records::is_reserved;
use crate::{ChunkKey, Emb, Library, perf, sidecar, tools};

/// Bump when the sidecar schema or the algorithm changes meaningfully; a
/// version mismatch reads as stale and triggers a rebuild.
pub const ATLAS_VERSION: u32 = 1;

const K: usize = 12; // neighbors kept per chunk (of TOP_K=40 HNSW results)
const MAP_POINTS: usize = 7000; // uniform sample target; theme members always included
const MIN_THEME_CHUNKS: usize = 25;
const MIN_THEME_DOCS: usize = 3;
const MAX_THEMES: usize = 24;
const TRAIL_STEPS: usize = 7; // beyond the seed
// a theme spans ≥ MIN_THEME_DOCS works, so 3 keeps a trail reachable for
// every theme (4 would silently exclude minimum-spread themes)
const MIN_TRAIL: usize = 3;
const DUP_SIM: f32 = 0.995; // near-verbatim duplicate cutoff (multi-copy OCR)
const LP_ROUNDS: usize = 20;
const PCA_ITERS: usize = 30;

// ---------------------------------------------------------------------------
// wire types (serialized to the sidecar and served verbatim to the frontend)
// ---------------------------------------------------------------------------

/// Cheap staleness stamp: doc set + per-doc chunk counts. Blind to content
/// changes that keep counts identical (an OCR redo) — `?refresh=1` on the
/// endpoint is the escape hatch.
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Debug)]
pub struct Fingerprint {
    pub version: u32,
    pub docs: u32,
    pub chunks: u64,
    pub hash: u64,
}

#[derive(Serialize, Deserialize)]
pub struct AtlasDoc {
    pub id: String,
    pub title: String,
    pub collection: String,
    pub chunks: u32,
}

/// One map dot. Compact field names on purpose: there are ~10-20k of these
/// and the sidecar/wire payload is dominated by them.
#[derive(Serialize, Deserialize)]
pub struct AtlasPoint {
    pub x: f32,
    pub y: f32,
    /// Index into [`Atlas::docs`].
    pub d: u32,
    /// Theme id, or -1.
    pub c: i32,
    /// Neighborhood doc-diversity: distinct *other* docs in the top-K.
    pub e: f32,
    pub p: u32,
    pub s: String,
}

#[derive(Serialize, Deserialize)]
pub struct Passage {
    pub d: u32,
    pub p: u32,
    pub s: String,
}

#[derive(Serialize, Deserialize)]
pub struct Theme {
    pub id: i32,
    pub size: u32,
    pub ndocs: u32,
    /// Short label from the local model (librarian probe); absent when the
    /// sidecar binary is unavailable or the probe failed — terms stand in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Distinctive terms, most distinctive first.
    pub terms: Vec<String>,
    /// Exemplar passages: nearest the theme centroid, distinct docs.
    pub top: Vec<Passage>,
}

#[derive(Serialize, Deserialize)]
pub struct TrailStep {
    pub d: u32,
    pub p: u32,
    pub s: String,
    /// Cosine similarity to the previous step (1.0 on the seed).
    pub sim: f32,
}

#[derive(Serialize, Deserialize)]
pub struct Trail {
    /// Theme id this trail walks.
    pub c: i32,
    pub steps: Vec<TrailStep>,
}

#[derive(Serialize, Deserialize)]
pub struct Atlas {
    pub fingerprint: Fingerprint,
    pub built_ms: u64,
    pub build_ms: u64,
    pub docs: Vec<AtlasDoc>,
    pub points: Vec<AtlasPoint>,
    pub themes: Vec<Theme>,
    pub trails: Vec<Trail>,
}

// ---------------------------------------------------------------------------
// cache + build-once claim
// ---------------------------------------------------------------------------

pub fn sidecar_path(data: &Path) -> PathBuf {
    data.join("atlas.json")
}

/// Real (non-reserved) doc ids, sorted — the same enumeration the overview
/// tool uses: one markdown edition per ingested doc.
fn list_docs(data: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(data.join("text"))
        .map(|it| {
            it.flatten()
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let id = name.strip_suffix(".md")?;
                    (!is_reserved(id)).then(|| id.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

pub fn fingerprint(lib: &Library, data: &Path) -> Fingerprint {
    let ids = list_docs(data);
    let counts: Vec<(String, u64)> = lib.rtx(|(_, (manifest, _))| {
        ids.iter()
            .map(|id| (id.clone(), manifest.search(id).len() as u64))
            .collect()
    });
    Fingerprint {
        version: ATLAS_VERSION,
        docs: counts.len() as u32,
        chunks: counts.iter().map(|(_, n)| n).sum(),
        hash: fxhash::hash64(&counts),
    }
}

/// The cached atlas, if present and exactly as fresh as `fp` (which carries
/// the version, so schema bumps also read as stale).
pub fn load_fresh(data: &Path, fp: &Fingerprint) -> Option<Atlas> {
    let atlas: Atlas = sidecar::read_json(&sidecar_path(data))?;
    (atlas.fingerprint == *fp).then_some(atlas)
}

/// (started_ms, stage) of the in-flight build, if any.
static BUILD: Mutex<Option<(u64, &'static str)>> = Mutex::new(None);

fn build_state() -> std::sync::MutexGuard<'static, Option<(u64, &'static str)>> {
    // a poisoned guard still holds valid state; recover rather than panic
    BUILD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Exclusive permission to run [`build`]. Released on drop (including
/// unwind), so a panicking build never wedges the claim.
pub struct BuildClaim {
    _priv: (),
}

impl BuildClaim {
    fn stage(&self, s: &'static str) {
        if let Some(e) = build_state().as_mut() {
            e.1 = s;
        }
    }
}

impl Drop for BuildClaim {
    fn drop(&mut self) {
        *build_state() = None;
    }
}

/// `Some` iff no build is in flight process-wide. Every build path — route,
/// desktop command, post-ingest trigger — must claim before spawning.
pub fn try_claim() -> Option<BuildClaim> {
    let mut g = build_state();
    if g.is_some() {
        return None;
    }
    *g = Some((perf::now_ms(), "starting"));
    Some(BuildClaim { _priv: () })
}

/// (started_ms, stage) if a build is in flight — the "building" wire body.
pub fn building() -> Option<(u64, &'static str)> {
    *build_state()
}

// ---------------------------------------------------------------------------
// the build
// ---------------------------------------------------------------------------

struct ChunkMeta {
    doc: u32,
    page: u32,
    text: String,
}

/// Full pipeline: snapshot chunks, k-NN via the HNSW index, cluster, term,
/// trail, project, sample, label (when a librarian sidecar binary is
/// given), write the sidecar. Holds host read locks only through `lib`'s
/// own short transactions; the claim is consumed and released on return or
/// unwind. Labeling happens *before* the write so the sidecar always lands
/// fully labeled — the "building" status covers the probe calls too.
pub fn build(
    claim: BuildClaim,
    lib: &Library,
    ctx: &Ctx,
    librarian: Option<&Path>,
) -> io::Result<Atlas> {
    let data = &ctx.data;
    let t0 = std::time::Instant::now();

    claim.stage("loading");
    // fingerprint first: if a commit lands mid-build, the recorded
    // fingerprint predates it and the sidecar reads as stale — self-healing
    let fp = fingerprint(lib, data);
    let doc_ids = list_docs(data);
    let titles = ctx.titles();
    // BTreeMap iteration order makes "first collection wins" deterministic
    let coll_of: HashMap<String, String> = {
        let mut m = HashMap::new();
        for (name, ids) in ctx.collections() {
            for id in ids {
                m.entry(id).or_insert_with(|| name.clone());
            }
        }
        m
    };

    let mut metas: Vec<ChunkMeta> = Vec::new();
    let mut embs: Vec<Emb> = Vec::new();
    let mut keys: Vec<ChunkKey> = Vec::new();
    let mut index_of: HashMap<ChunkKey, u32> = HashMap::new();
    for (di, id) in doc_ids.iter().enumerate() {
        let mut doc_keys = lib.rtx(|(_, (manifest, _))| manifest.search(id));
        doc_keys.sort_by_key(|k| (k.page, k.idx));
        for key in doc_keys {
            let Some(rec) = lib.get(&key) else { continue };
            let mut e = rec.emb;
            normalize(&mut e);
            index_of.insert(key.clone(), metas.len() as u32);
            metas.push(ChunkMeta {
                doc: di as u32,
                page: key.page,
                text: rec.text(),
            });
            embs.push(e);
            keys.push(key);
        }
    }
    let n = metas.len();

    claim.stage("knn");
    // cosine is scale-invariant, so querying with the normalized copy is
    // exact; sim = 1 - distance. Self and reserved neighbors drop here.
    let knn: Vec<Vec<(f32, u32)>> = lib.rtx(|((_, vec), _)| {
        (0..n)
            .map(|i| {
                vec.search(&embs[i])
                    .into_iter()
                    .filter_map(|s| {
                        let j = *index_of.get(&s.val)?;
                        (j as usize != i).then_some((1.0 - s.score, j))
                    })
                    .take(K)
                    .collect()
            })
            .collect()
    });

    claim.stage("clustering");
    let entropy = entropy(&knn, &metas);
    let edges = mutual_edges(&knn);
    let adj = adjacency(n, &edges);
    let labels = label_propagation(&adj);
    let communities = kept_communities(&labels, &metas);
    let comm_of: Vec<i32> = {
        let mut v = vec![-1i32; n];
        for (ci, (_, members)) in communities.iter().enumerate() {
            for &i in members {
                v[i as usize] = ci as i32;
            }
        }
        v
    };

    claim.stage("themes");
    let chunk_tokens: Vec<Vec<String>> = metas.iter().map(|m| term_tokens(&m.text)).collect();
    let mut global_tf: HashMap<&str, f64> = HashMap::new();
    for toks in &chunk_tokens {
        for t in toks {
            *global_tf.entry(t).or_insert(0.0) += 1.0;
        }
    }
    let doc_base: Vec<&str> = doc_ids.iter().map(|id| base_id(id)).collect();

    let mut themes = Vec::new();
    let mut trails = Vec::new();
    for (ci, (_, members)) in communities.iter().enumerate() {
        let cent = centroid(members, &embs);
        let terms = theme_terms(members, &chunk_tokens, &global_tf);
        let top = exemplars(members, &embs, &cent, &metas);
        let mut docs: Vec<u32> = members.iter().map(|&i| metas[i as usize].doc).collect();
        docs.sort_unstable();
        docs.dedup();
        themes.push(Theme {
            id: ci as i32,
            size: members.len() as u32,
            ndocs: docs.len() as u32,
            title: None,
            terms,
            top,
        });
        if let Some(t) = trail(
            ci as i32, members, &cent, &embs, &knn, &comm_of, &metas, &doc_base,
        ) {
            trails.push(t);
        }
    }

    claim.stage("projecting");
    let (mean, pc1, pc2) = pca2(&embs);
    let points = sample_points(&metas, &embs, &entropy, &comm_of, &mean, &pc1, &pc2);

    if let Some(bin) = librarian.filter(|b| b.exists()) {
        claim.stage("labeling");
        label_themes(bin, &mut themes);
    }

    claim.stage("writing");
    let docs = doc_ids
        .iter()
        .enumerate()
        .map(|(di, id)| AtlasDoc {
            id: id.clone(),
            title: titles
                .get(id)
                .cloned()
                .unwrap_or_else(|| tools::derive_title(id)),
            collection: coll_of.get(id).cloned().unwrap_or_default(),
            chunks: metas.iter().filter(|m| m.doc == di as u32).count() as u32,
        })
        .collect();
    let atlas = Atlas {
        fingerprint: fp,
        built_ms: perf::now_ms(),
        build_ms: t0.elapsed().as_millis() as u64,
        docs,
        points,
        themes,
        trails,
    };
    sidecar::write_json_atomic_compact(&sidecar_path(data), &atlas)?;
    Ok(atlas)
}

// ---------------------------------------------------------------------------
// theme labeling via the librarian sidecar (best-effort)
// ---------------------------------------------------------------------------

/// One schema-constrained `librarian probe` per theme. Any failure —
/// missing binary, model refusal, junk output — leaves `title` as `None`
/// and the terms carry the UI instead. Sequential on purpose: one AFM
/// session at a time keeps memory pressure flat during a background build.
fn label_themes(bin: &Path, themes: &mut [Theme]) {
    for theme in themes.iter_mut() {
        match probe_title(bin, theme) {
            Ok(t) => theme.title = t,
            Err(e) => eprintln!("atlas: label probe failed for theme {}: {e}", theme.id),
        }
    }
}

fn probe_title(bin: &Path, theme: &Theme) -> io::Result<Option<String>> {
    let passages = theme
        .top
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, p)| format!("Passage {}: “{}”", i + 1, p.s))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "Passages from {} different books share one topic.\nKeywords: {}.\n{passages}\nName the shared topic.",
        theme.ndocs,
        theme.terms.join(", "),
    );
    let fixture = serde_json::json!({
        "id": format!("theme-{}", theme.id),
        "prompt": prompt,
        "instructions": "You label groups of related passages from a personal \
            library. Answer with a short, plain noun phrase — 2 to 4 words, \
            no punctuation, no explanation.",
        "tools": false,
        "temperature": 0.2,
        "schema": {
            "name": "theme_label",
            "properties": [{
                "name": "title",
                "type": "string",
                "description": "2-4 word noun phrase naming the shared topic"
            }]
        }
    });
    let path = std::env::temp_dir().join(format!(
        "atlas-label-{}-{}.json",
        std::process::id(),
        theme.id
    ));
    std::fs::write(&path, serde_json::to_vec(&fixture)?)?;
    let out = std::process::Command::new(bin)
        .arg("probe")
        .arg(&path)
        .output();
    let _ = std::fs::remove_file(&path);
    let stdout_bytes = out?.stdout;
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["e"] == "result"
            && v["ok"] == true
            && let Some(content) = v["content"].as_str()
            && let Ok(obj) = serde_json::from_str::<serde_json::Value>(content)
            && let Some(t) = obj["title"].as_str()
        {
            return Ok(sanitize_title(t));
        }
    }
    Ok(None)
}

/// Trim wrapper punctuation and whitespace; reject empties and runaway
/// sentences the schema should have prevented.
fn sanitize_title(t: &str) -> Option<String> {
    let t = t
        .trim()
        .trim_matches(|c: char| "\"“”.:;,".contains(c))
        .trim();
    (!t.is_empty() && t.chars().count() <= 48).then(|| t.to_string())
}

// ---------------------------------------------------------------------------
// pure stages (testable without a store)
// ---------------------------------------------------------------------------

fn normalize(e: &mut Emb) {
    let norm = e.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in e.iter_mut() {
        *x /= norm;
    }
}

fn dot(a: &Emb, b: &Emb) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Distinct *other* docs among each chunk's neighbors.
fn entropy(knn: &[Vec<(f32, u32)>], metas: &[ChunkMeta]) -> Vec<f32> {
    knn.iter()
        .enumerate()
        .map(|(i, nbrs)| {
            let mut docs: Vec<u32> = nbrs
                .iter()
                .filter(|(_, j)| metas[*j as usize].doc != metas[i].doc)
                .map(|(_, j)| metas[*j as usize].doc)
                .collect();
            docs.sort_unstable();
            docs.dedup();
            docs.len() as f32
        })
        .collect()
}

/// Reciprocal k-NN pairs, each once (a < b), with their similarity.
fn mutual_edges(knn: &[Vec<(f32, u32)>]) -> Vec<(u32, u32, f32)> {
    let mut edges = Vec::new();
    for (i, nbrs) in knn.iter().enumerate() {
        for &(sim, j) in nbrs {
            if (j as usize) <= i {
                continue;
            }
            if knn[j as usize].iter().any(|(_, x)| *x as usize == i) {
                edges.push((i as u32, j, sim));
            }
        }
    }
    edges
}

fn adjacency(n: usize, edges: &[(u32, u32, f32)]) -> Vec<Vec<(u32, f32)>> {
    let mut adj = vec![Vec::new(); n];
    for &(a, b, s) in edges {
        adj[a as usize].push((b, s));
        adj[b as usize].push((a, s));
    }
    adj
}

/// Weighted label propagation over the mutual-kNN graph. Deterministic:
/// nodes update in index order; on a weight tie a node *keeps its current
/// label* if tied (the LPA stability rule — without it, near-uniform
/// weights let every anchor label abandon itself in round one and the
/// graph freezes into singleton islands), else adopts the smallest tied
/// label. Isolated nodes keep their own label.
fn label_propagation(adj: &[Vec<(u32, f32)>]) -> Vec<u32> {
    let n = adj.len();
    let mut label: Vec<u32> = (0..n as u32).collect();
    for _ in 0..LP_ROUNDS {
        let mut changed = 0usize;
        for i in 0..n {
            if adj[i].is_empty() {
                continue;
            }
            let mut votes: HashMap<u32, f32> = HashMap::new();
            for &(j, s) in &adj[i] {
                *votes.entry(label[j as usize]).or_insert(0.0) += s;
            }
            let max = votes.values().cloned().fold(f32::MIN, f32::max);
            if votes.get(&label[i]).copied().unwrap_or(f32::MIN) >= max {
                continue; // stability: current label ties for the max
            }
            let best = votes
                .iter()
                .filter(|(_, w)| **w >= max)
                .map(|(l, _)| *l)
                .min()
                .expect("non-empty adjacency has votes");
            if best != label[i] {
                label[i] = best;
                changed += 1;
            }
        }
        if changed == 0 {
            break;
        }
    }
    label
}

/// Communities spanning enough chunks and docs, largest first; ties break on
/// the smallest member index so HashMap order never leaks into the output.
fn kept_communities(labels: &[u32], metas: &[ChunkMeta]) -> Vec<(u32, Vec<u32>)> {
    let mut members: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, &l) in labels.iter().enumerate() {
        members.entry(l).or_default().push(i as u32);
    }
    let mut kept: Vec<(u32, Vec<u32>)> = members
        .into_iter()
        .filter(|(_, m)| {
            if m.len() < MIN_THEME_CHUNKS {
                return false;
            }
            let mut docs: Vec<u32> = m.iter().map(|&i| metas[i as usize].doc).collect();
            docs.sort_unstable();
            docs.dedup();
            docs.len() >= MIN_THEME_DOCS
        })
        .collect();
    kept.sort_by_key(|(_, m)| (Reverse(m.len()), m[0]));
    kept.truncate(MAX_THEMES);
    kept
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "are", "was", "were", "not", "you", "your",
    "from", "have", "has", "had", "but", "its", "can", "will", "all", "one", "two", "into", "when",
    "then", "them", "they", "their", "there", "which", "what", "who", "how", "why", "also", "more",
    "some", "such", "than", "these", "those", "each", "other", "may", "should", "would", "could",
    "about", "over", "under", "out", "off", "our", "his", "her", "she", "him", "been", "being",
    "does", "did", "just", "only", "very", "much", "many", "most", "any", "per", "page", "chapter",
];

/// Atlas-private tokenizer (see module docs: intentionally independent of
/// `crate::text::tokenize`).
fn term_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 3 && w.len() <= 20)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Distinctive terms by log-odds with an informative Dirichlet prior — the
/// prior pulls common words toward their corpus rate so only genuinely
/// over-represented terms surface.
fn theme_terms(
    members: &[u32],
    chunk_tokens: &[Vec<String>],
    global_tf: &HashMap<&str, f64>,
) -> Vec<String> {
    let g_total: f64 = global_tf.values().sum();
    let mut tf: HashMap<&str, f64> = HashMap::new();
    for &i in members {
        for t in &chunk_tokens[i as usize] {
            *tf.entry(t).or_insert(0.0) += 1.0;
        }
    }
    let c_total: f64 = tf.values().sum();
    let a0 = 500.0; // prior strength
    let mut scored: Vec<(f64, &str)> = tf
        .iter()
        .filter(|(t, f)| **f >= 4.0 && global_tf.get(**t).copied().unwrap_or(0.0) >= 8.0)
        .map(|(t, f)| {
            let f = *f;
            let g = global_tf[*t];
            let prior = a0 * g / g_total;
            let odds_c = (f + prior) / (c_total + a0 - f - prior);
            let odds_g = (g + prior) / (g_total + a0 - g - prior);
            let z = (odds_c / odds_g).ln() / (1.0 / (f + prior) + 1.0 / (g + prior)).sqrt();
            (z, *t)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .expect("log-odds scores are finite")
            .then(a.1.cmp(b.1))
    });
    scored.truncate(9);
    scored.into_iter().map(|(_, t)| t.to_string()).collect()
}

fn centroid(members: &[u32], embs: &[Emb]) -> Emb {
    let mut cent = [0f32; crate::EMB_DIM];
    for &i in members {
        for (c, x) in cent.iter_mut().zip(&embs[i as usize]) {
            *c += x;
        }
    }
    normalize(&mut cent);
    cent
}

/// Up to 5 members nearest the centroid, one per doc.
fn exemplars(members: &[u32], embs: &[Emb], cent: &Emb, metas: &[ChunkMeta]) -> Vec<Passage> {
    let mut scored: Vec<(f32, u32)> = members
        .iter()
        .map(|&i| (dot(&embs[i as usize], cent), i))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .expect("cosine similarities are finite")
            .then(a.1.cmp(&b.1))
    });
    let mut top = Vec::new();
    let mut seen = HashSet::new();
    for (_, i) in scored {
        let m = &metas[i as usize];
        if seen.insert(m.doc) {
            top.push(Passage {
                d: m.doc,
                p: m.page,
                s: snippet(&m.text, 220),
            });
            if top.len() == 5 {
                break;
            }
        }
    }
    top
}

/// Physical copies of one work carry `-N` id suffixes (see
/// `tools::dedup_doc_pages`); the atlas collapses them so a trail or bond
/// never "travels" between two scans of the same book.
fn base_id(id: &str) -> &str {
    match id.rfind('-') {
        Some(p) if !id[p + 1..].is_empty() && id[p + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            &id[..p]
        }
        _ => id,
    }
}

/// One throughline per theme: seed at the member nearest the centroid
/// (high-entropy seeds pick generic catalog listings), then follow nearest
/// neighbors under three rules — every step reaches a work the trail hasn't
/// visited, never a near-verbatim repeat, in-theme neighbors preferred.
#[expect(
    clippy::too_many_arguments,
    reason = "internal stage over the build's shared slices"
)]
fn trail(
    theme: i32,
    members: &[u32],
    cent: &Emb,
    embs: &[Emb],
    knn: &[Vec<(f32, u32)>],
    comm_of: &[i32],
    metas: &[ChunkMeta],
    doc_base: &[&str],
) -> Option<Trail> {
    let seed = *members.iter().max_by(|&&a, &&b| {
        dot(&embs[a as usize], cent)
            .partial_cmp(&dot(&embs[b as usize], cent))
            .expect("cosine similarities are finite")
            .then(b.cmp(&a))
    })?;
    let mut cur = seed as usize;
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(doc_base[metas[cur].doc as usize]);
    let mut steps = vec![TrailStep {
        d: metas[cur].doc,
        p: metas[cur].page,
        s: snippet(&metas[cur].text, 240),
        sim: 1.0,
    }];
    for _ in 0..TRAIL_STEPS {
        let fresh = |&&(sim, j): &&(f32, u32)| {
            sim < DUP_SIM && !visited.contains(doc_base[metas[j as usize].doc as usize])
        };
        let next = knn[cur]
            .iter()
            .filter(fresh)
            .find(|(_, j)| comm_of[*j as usize] == theme)
            .or_else(|| knn[cur].iter().find(|t| fresh(t)));
        let Some(&(sim, j)) = next else { break };
        cur = j as usize;
        visited.insert(doc_base[metas[cur].doc as usize]);
        steps.push(TrailStep {
            d: metas[cur].doc,
            p: metas[cur].page,
            s: snippet(&metas[cur].text, 240),
            sim,
        });
    }
    (steps.len() >= MIN_TRAIL).then_some(Trail { c: theme, steps })
}

/// Top two principal axes by power iteration: deterministic hash-seeded
/// start, Gram-Schmidt keeps the second axis orthogonal to the first.
fn pca2(embs: &[Emb]) -> (Emb, Emb, Emb) {
    let n = embs.len().max(1);
    let mut mean = [0f32; crate::EMB_DIM];
    for e in embs {
        for (m, x) in mean.iter_mut().zip(e) {
            *m += x;
        }
    }
    for m in mean.iter_mut() {
        *m /= n as f32;
    }
    let power = |ortho: Option<&Emb>| -> Emb {
        let mut v = [0f32; crate::EMB_DIM];
        for (i, x) in v.iter_mut().enumerate() {
            *x = ((i.wrapping_mul(2654435761)) % 1000) as f32 / 1000.0 - 0.5;
        }
        for _ in 0..PCA_ITERS {
            let mut w = [0f32; crate::EMB_DIM];
            for e in embs {
                let mut d = 0f32;
                for k in 0..crate::EMB_DIM {
                    d += (e[k] - mean[k]) * v[k];
                }
                for k in 0..crate::EMB_DIM {
                    w[k] += d * (e[k] - mean[k]);
                }
            }
            if let Some(o) = ortho {
                let d = dot(&w, o);
                for (wk, ok) in w.iter_mut().zip(o) {
                    *wk -= d * ok;
                }
            }
            normalize(&mut w);
            v = w;
        }
        v
    };
    let pc1 = power(None);
    let pc2 = power(Some(&pc1));
    (mean, pc1, pc2)
}

/// ~[`MAP_POINTS`] uniformly strided chunks plus every theme member (theme
/// highlighting needs them all on the map).
fn sample_points(
    metas: &[ChunkMeta],
    embs: &[Emb],
    entropy: &[f32],
    comm_of: &[i32],
    mean: &Emb,
    pc1: &Emb,
    pc2: &Emb,
) -> Vec<AtlasPoint> {
    let n = metas.len();
    let stride = (n as f32 / MAP_POINTS as f32).max(1.0);
    let mut points = Vec::new();
    let mut acc = 0f32;
    for i in 0..n {
        acc += 1.0;
        let sampled = acc >= stride;
        if sampled {
            acc -= stride;
        }
        if !sampled && comm_of[i] < 0 {
            continue;
        }
        let mut centered = embs[i];
        for (c, m) in centered.iter_mut().zip(mean) {
            *c -= m;
        }
        points.push(AtlasPoint {
            x: dot(&centered, pc1),
            y: dot(&centered, pc2),
            d: metas[i].doc,
            c: comm_of[i],
            e: entropy[i],
            p: metas[i].page,
            s: snippet(&metas[i].text, 130),
        });
    }
    points
}

/// First `max` chars, cut back to a word boundary, with an ellipsis when
/// truncated. Char-based, so multibyte input never splits.
fn snippet(text: &str, max: usize) -> String {
    let mut s: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        if let Some(cut) = s.rfind(' ') {
            s.truncate(cut);
        }
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(hot: usize) -> Emb {
        let mut e = [0.0f32; crate::EMB_DIM];
        e[hot % crate::EMB_DIM] = 1.0;
        e
    }

    fn meta(doc: u32, page: u32, text: &str) -> ChunkMeta {
        ChunkMeta {
            doc,
            page,
            text: text.to_string(),
        }
    }

    #[test]
    fn base_id_collapses_numeric_copy_suffixes() {
        assert_eq!(base_id("moby-dick-2"), "moby-dick");
        assert_eq!(base_id("moby-dick"), "moby-dick");
        // documented over-collapse: a trailing year reads as a copy suffix
        assert_eq!(base_id("war-1914"), "war");
        assert_eq!(base_id("x-"), "x-");
        assert_eq!(base_id("1985"), "1985");
    }

    #[test]
    fn mutual_knn_keeps_only_reciprocal_pairs() {
        // 0↔1 reciprocal; 0→2 one-way
        let knn = vec![
            vec![(0.9, 1u32), (0.8, 2u32)],
            vec![(0.9, 0u32)],
            vec![(0.7, 1u32)],
        ];
        let edges = mutual_edges(&knn);
        assert_eq!(edges, vec![(0, 1, 0.9)]);
    }

    #[test]
    fn label_propagation_is_deterministic_and_converges() {
        // two disjoint triangles
        let mut edges = Vec::new();
        for (a, b) in [(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5)] {
            edges.push((a as u32, b as u32, 1.0));
        }
        let adj = adjacency(6, &edges);
        let l1 = label_propagation(&adj);
        let l2 = label_propagation(&adj);
        assert_eq!(l1, l2);
        assert_eq!(l1[0], l1[1]);
        assert_eq!(l1[1], l1[2]);
        assert_eq!(l1[3], l1[4]);
        assert_eq!(l1[4], l1[5]);
        assert_ne!(l1[0], l1[3]);
    }

    #[test]
    fn trail_visits_new_works_and_skips_near_verbatim() {
        // works: a, a-2 (copy of a), b, c, d — chunk 5 is out-of-theme in b
        let doc_ids = ["a", "a-2", "b", "c", "d"];
        let doc_base: Vec<&str> = doc_ids.iter().map(|id| base_id(id)).collect();
        let metas = vec![
            meta(0, 1, "seed passage"),
            meta(1, 1, "verbatim copy in the second scan"),
            meta(2, 1, "in-theme continuation"),
            meta(3, 1, "third work"),
            meta(4, 1, "fourth work"),
            meta(2, 9, "unrelated passage in b"),
        ];
        let embs = vec![dir(0), dir(0), dir(1), dir(1), dir(1), dir(2)];
        let comm_of = vec![0, 0, 0, 0, 0, -1];
        let knn = vec![
            // dup+copy (skipped), out-of-theme fresh at higher sim,
            // in-theme fresh at lower sim — in-theme must win
            vec![(0.999, 1u32), (0.9, 5u32), (0.85, 2u32)],
            vec![],
            vec![(0.8, 0u32), (0.7, 3u32)], // visited, then fresh
            vec![(0.6, 4u32)],
            vec![], // dead end: trail stops
            vec![],
        ];
        let t = trail(
            0,
            &[0, 2, 3, 4],
            &dir(0),
            &embs,
            &knn,
            &comm_of,
            &metas,
            &doc_base,
        )
        .expect("trail long enough");
        let docs: Vec<u32> = t.steps.iter().map(|s| s.d).collect();
        assert_eq!(docs, vec![0, 2, 3, 4]);
        let sims: Vec<f32> = t.steps.iter().map(|s| s.sim).collect();
        assert_eq!(sims, vec![1.0, 0.85, 0.7, 0.6]);
        // works all distinct after copy-collapse
        let mut bases: Vec<&str> = docs.iter().map(|&d| doc_base[d as usize]).collect();
        bases.sort_unstable();
        bases.dedup();
        assert_eq!(bases.len(), t.steps.len());
    }

    #[test]
    fn trail_shorter_than_minimum_is_dropped() {
        let metas = vec![meta(0, 1, "alone"), meta(1, 1, "pair")];
        let embs = vec![dir(0), dir(1)];
        let knn = vec![vec![(0.5, 1u32)], vec![]];
        let t = trail(
            0,
            &[0, 1],
            &dir(0),
            &embs,
            &knn,
            &[0, 0],
            &metas,
            &["a", "b"],
        );
        assert!(t.is_none());
    }

    #[test]
    fn theme_terms_prefer_distinctive_over_common() {
        // members say "gears" often; "make" is everywhere in the corpus
        let member_toks: Vec<String> = ["gears", "gears", "gears", "gears", "gears", "make"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let chunk_tokens = vec![member_toks, vec!["make".to_string(); 40]];
        let mut global_tf: HashMap<&str, f64> = HashMap::new();
        global_tf.insert("gears", 8.0);
        global_tf.insert("make", 100.0);
        let terms = theme_terms(&[0], &chunk_tokens, &global_tf);
        assert_eq!(terms.first().map(String::as_str), Some("gears"));
    }

    #[test]
    fn term_tokens_drop_stopwords_and_short_words() {
        let toks = term_tokens("The gears of AN old Lathe turn, x y!");
        assert_eq!(toks, vec!["gears", "old", "lathe", "turn"]);
    }

    #[test]
    fn snippet_truncates_on_char_boundary_with_ellipsis() {
        assert_eq!(snippet("short text", 20), "short text");
        let s = snippet("héllo wörld ünïcode teasers", 15);
        assert!(s.ends_with('…'), "{s:?}");
        assert!(s.chars().count() <= 16);
    }

    #[test]
    fn power_iteration_axes_are_orthogonal_and_deterministic() {
        // variance concentrated on dim 0, secondary on dim 1
        let mut embs = Vec::new();
        for i in 0..40 {
            let mut e = [0.0f32; crate::EMB_DIM];
            e[0] = (i as f32) - 20.0;
            e[1] = ((i % 5) as f32) - 2.0;
            embs.push(e);
        }
        let (m1, p1a, p2a) = pca2(&embs);
        let (m2, p1b, p2b) = pca2(&embs);
        assert_eq!(m1, m2);
        assert_eq!(p1a, p1b);
        assert_eq!(p2a, p2b);
        assert!(p1a[0].abs() > 0.99, "pc1 should align with dim 0");
        assert!(p2a[1].abs() > 0.99, "pc2 should align with dim 1");
        assert!(dot(&p1a, &p2a).abs() < 1e-4);
    }

    #[test]
    fn sanitize_title_trims_wrappers_and_rejects_junk() {
        assert_eq!(
            sanitize_title("  “Bread and ovens.”  "),
            Some("Bread and ovens".to_string())
        );
        assert_eq!(sanitize_title("\"\""), None);
        assert_eq!(sanitize_title("   "), None);
        assert_eq!(sanitize_title(&"x".repeat(60)), None);
    }

    #[test]
    fn claim_is_exclusive_and_released_on_drop() {
        let claim = try_claim().expect("first claim succeeds");
        assert!(try_claim().is_none(), "second claim excluded");
        let (_, stage) = building().expect("build state visible");
        assert_eq!(stage, "starting");
        claim.stage("knn");
        assert_eq!(building().expect("still building").1, "knn");
        drop(claim);
        assert!(building().is_none());
        drop(try_claim().expect("claim reusable after release"));
    }
}
