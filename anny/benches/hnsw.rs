//! Comparative HNSW benchmarks.
//!
//!   cargo bench                      # auto-downloads SIFT10K (cached in target/)
//!   cargo bench --features compare   # also bench instant-distance + hnsw_rs
//!
//! On first run the siftsmall set is fetched from the TEXMEX corpus via `curl`
//! and unpacked with `tar` into `target/sift-data/`; later runs reuse the cache.
//! If the download fails (offline, no curl/tar), it falls back to synthetic data
//! so the bench still runs.
//!
//! Fair-comparison note: ANN latency is only meaningful at a fixed recall, so the
//! harness prints each impl's recall@10 next to its timings. All impls are
//! configured to the same nominal params: M=16, ef_construction=128, ef_search=64.

use anny::hnsw::Hnsw;
use anny::metric::L2;
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;

const DIM: usize = 128;
const M0: usize = 32; // => M = 16
const TOPK: usize = 10; // => EF_BUILD = 128, EF_SEARCH = 64
const MAXL: usize = 16;
const EF_C: usize = 128;
const EF_S: usize = 64;

type AnnyDefault = Hnsw<f32, L2, DIM, M0, TOPK, EF_S, EF_C, MAXL>;

// --------------------------------------------------------------------------
// dataset: real .fvecs/.ivecs if SIFT_DIR is set, else synthetic clusters
// --------------------------------------------------------------------------
struct Dataset {
    base: Vec<[f32; DIM]>,
    query: Vec<[f32; DIM]>,
    gt: Vec<Vec<u32>>, // ground-truth neighbour ids per query (>= TOPK)
    source: &'static str,
}

#[inline]
fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn read_fvecs(path: &str) -> Vec<[f32; DIM]> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let dim = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        assert_eq!(dim, DIM, "fvecs dim {dim} != {DIM}");
        i += 4;
        let mut v = [0f32; DIM];
        for d in 0..DIM {
            v[d] = f32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
            i += 4;
        }
        out.push(v);
    }
    out
}

fn read_ivecs(path: &str) -> Vec<Vec<u32>> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let dim = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        let mut v = Vec::with_capacity(dim);
        for _ in 0..dim {
            v.push(u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()));
            i += 4;
        }
        out.push(v);
    }
    out
}

fn synthetic(n: usize, q: usize) -> Dataset {
    // gaussian-ish clusters via a cheap xorshift; brute-force ground truth.
    let mut s = 0x243F6A8885A308D3u64;
    let mut nf = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 11) as f32) / ((1u64 << 53) as f32)
    };
    let centers: Vec<[f32; DIM]> = (0..50).map(|_| std::array::from_fn(|_| nf())).collect();
    let mk = |nf: &mut dyn FnMut() -> f32| -> [f32; DIM] {
        let c = &centers[((nf() * 50.0) as usize).min(49)];
        std::array::from_fn(|d| c[d] + (nf() - 0.5) * 0.1)
    };
    let base: Vec<[f32; DIM]> = (0..n).map(|_| mk(&mut nf)).collect();
    let query: Vec<[f32; DIM]> = (0..q).map(|_| mk(&mut nf)).collect();
    let gt: Vec<Vec<u32>> = query
        .iter()
        .map(|qv| {
            let mut d: Vec<(f32, u32)> = base
                .iter()
                .enumerate()
                .map(|(i, p)| (l2(p, qv), i as u32))
                .collect();
            d.sort_by(|a, b| a.0.total_cmp(&b.0));
            d.into_iter().take(100).map(|(_, i)| i).collect()
        })
        .collect();
    Dataset {
        base,
        query,
        gt,
        source: "synthetic-50clusters",
    }
}

// Locate the cargo `target` dir from the running bench binary
// (.../target/release/deps/compare-HASH) so the dataset is cached there.
fn sift_cache_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let target = exe
        .ancestors()
        .find(|p| p.file_name() == Some(std::ffi::OsStr::new("target")))
        .map(|p| p.to_path_buf())
        .or_else(|| exe.ancestors().nth(3).map(|p| p.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    target.join("sift-data")
}

const SIFT_URL: &str = "ftp://ftp.irisa.fr/local/texmex/corpus/siftsmall.tar.gz";

// Download + extract siftsmall on first run (curl + tar), cached under target/.
// Returns the folder holding the .fvecs/.ivecs, or None if anything failed.
fn ensure_sift() -> Option<std::path::PathBuf> {
    let dir = sift_cache_dir();
    let root = dir.join("siftsmall");
    let base = root.join("siftsmall_base.fvecs");
    if base.exists() {
        return Some(root);
    }
    std::fs::create_dir_all(&dir).ok()?;

    let tgz = dir.join("siftsmall.tar.gz");
    if !tgz.exists() {
        eprintln!("fetching siftsmall (~5 MB) via curl -> {}", tgz.display());
        let ok = std::process::Command::new("curl")
            .args(["-fSL", "--retry", "3", "--connect-timeout", "30", "-o"])
            .arg(&tgz)
            .arg(SIFT_URL)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&tgz); // don't cache a partial file
            return None;
        }
    }
    let ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && base.exists() {
        Some(root)
    } else {
        None
    }
}

fn load() -> Dataset {
    match ensure_sift() {
        Some(root) => {
            let p = |f: &str| root.join(f).to_string_lossy().into_owned();
            Dataset {
                base: read_fvecs(&p("siftsmall_base.fvecs")),
                query: read_fvecs(&p("siftsmall_query.fvecs")),
                gt: read_ivecs(&p("siftsmall_groundtruth.ivecs")),
                source: "SIFT10K",
            }
        }
        None => {
            eprintln!("WARNING: siftsmall download/extract failed; using synthetic data");
            synthetic(10_000, 100)
        }
    }
}

fn recall(got: &[Vec<u32>], gt: &[Vec<u32>], k: usize) -> f64 {
    let (mut hit, mut tot) = (0usize, 0usize);
    for (g, truth) in got.iter().zip(gt) {
        let want: std::collections::HashSet<u32> = truth.iter().take(k).copied().collect();
        for id in g.iter().take(k) {
            tot += 1;
            if want.contains(id) {
                hit += 1;
            }
        }
    }
    hit as f64 / tot as f64
}

// --------------------------------------------------------------------------
// our impl
// --------------------------------------------------------------------------
fn build_ours(base: &[[f32; DIM]]) -> AnnyDefault {
    let mut ix: AnnyDefault = Hnsw::new(L2, 0x51ED);
    for v in base {
        ix.insert(*v);
    }
    ix
}
fn query_ours(ix: &AnnyDefault, q: &[f32; DIM]) -> Vec<u32> {
    ix.search(&q[..]).into_iter().map(|(_, i)| i).collect()
}

// --------------------------------------------------------------------------
// competitors (enabled with --features compare). APIs reflect instant-distance
// 0.6 / hnsw_rs 0.3; adjust if you pin different versions.
// --------------------------------------------------------------------------
#[cfg(feature = "bench_compare")]
mod compare {
    use super::*;

    use anny::metric::Metric;

    // ---- instant-distance (HnswMap: values carry the original base index) ----
    #[derive(Clone)]
    pub struct P(pub [f32; DIM]);
    impl instant_distance::Point for P {
        fn distance(&self, o: &Self) -> f32 {
            L2::distance(&self.0, &o.0).sqrt()
        }
    }
    pub type IdMap = instant_distance::HnswMap<P, u32>;

    pub fn build_id(base: &[[f32; DIM]]) -> IdMap {
        let pts: Vec<P> = base.iter().map(|v| P(*v)).collect();
        let vals: Vec<u32> = (0..base.len() as u32).collect();
        instant_distance::Builder::default()
            .ef_construction(EF_C)
            .ef_search(EF_S)
            .build(pts, vals)
    }

    pub fn query_id(h: &IdMap, q: &[f32; DIM]) -> Vec<u32> {
        let mut s = instant_distance::Search::default();
        h.search(&P(*q), &mut s)
            .take(TOPK)
            .map(|item| *item.value) // original base index — no remap
            .collect()
    }

    // ---- hnsw_rs ----
    use hnsw_rs::prelude::*;
    pub fn build_rs(base: &[[f32; DIM]]) -> hnsw_rs::hnsw::Hnsw<'static, f32, DistL2> {
        let h = hnsw_rs::hnsw::Hnsw::<f32, DistL2>::new(M0 / 2, base.len(), MAXL, EF_C, DistL2 {});
        for (i, v) in base.iter().enumerate() {
            h.insert((&v[..], i));
        }
        h
    }
    pub fn query_rs(h: &hnsw_rs::hnsw::Hnsw<'static, f32, DistL2>, q: &[f32; DIM]) -> Vec<u32> {
        h.search(&q[..], TOPK, EF_S)
            .into_iter()
            .map(|n| n.d_id as u32)
            .collect()
    }
}

// --------------------------------------------------------------------------
// benchmark entry
// --------------------------------------------------------------------------
fn benches(c: &mut Criterion) {
    let ds = load();
    eprintln!(
        "dataset = {} | base={} query={} dim={}",
        ds.source,
        ds.base.len(),
        ds.query.len(),
        DIM
    );

    // ---- recall (printed so latencies are interpretable) ----
    let ours = build_ours(&ds.base);
    let got: Vec<Vec<u32>> = ds.query.iter().map(|q| query_ours(&ours, q)).collect();
    eprintln!(
        "recall@{TOPK}  ours              = {:.3}",
        recall(&got, &ds.gt, TOPK)
    );

    #[cfg(feature = "bench_compare")]
    {
        let idh = compare::build_id(&ds.base);
        let g: Vec<Vec<u32>> = ds
            .query
            .iter()
            .map(|q| compare::query_id(&idh, q))
            .collect();
        eprintln!(
            "recall@{TOPK}  instant-distance  = {:.3}",
            recall(&g, &ds.gt, TOPK)
        );

        let rsh = compare::build_rs(&ds.base);
        let g: Vec<Vec<u32>> = ds
            .query
            .iter()
            .map(|q| compare::query_rs(&rsh, q))
            .collect();
        eprintln!(
            "recall@{TOPK}  hnsw_rs           = {:.3}",
            recall(&g, &ds.gt, TOPK)
        );
    }

    // ---- build throughput (whole index) ----
    let mut g = c.benchmark_group("build");
    g.sample_size(10).measurement_time(Duration::from_secs(15));
    g.throughput(Throughput::Elements(ds.base.len() as u64)); // per-insert amortized
    g.bench_function("ANNy", |b| b.iter(|| black_box(build_ours(&ds.base))));
    #[cfg(feature = "bench_compare")]
    {
        g.bench_function("instant-distance", |b| {
            b.iter(|| black_box(compare::build_id(&ds.base)))
        });
        g.bench_function("hnsw_rs", |b| {
            b.iter(|| black_box(compare::build_rs(&ds.base)))
        });
    }
    g.finish();

    // ---- single-query latency (index built once) ----
    let mut g = c.benchmark_group("query");
    g.throughput(Throughput::Elements(1));
    let mut qi = 0usize;
    g.bench_function("ANNy", |b| {
        b.iter(|| {
            let q = &ds.query[qi % ds.query.len()];
            qi += 1;
            black_box(query_ours(&ours, q))
        })
    });
    #[cfg(feature = "bench_compare")]
    {
        let idh = compare::build_id(&ds.base);
        let mut qi = 0usize;
        g.bench_function("instant-distance", |b| {
            b.iter(|| {
                let q = &ds.query[qi % ds.query.len()];
                qi += 1;
                black_box(compare::query_id(&idh, q))
            })
        });
        let rsh = compare::build_rs(&ds.base);
        let mut qi = 0usize;
        g.bench_function("hnsw_rs", |b| {
            b.iter(|| {
                let q = &ds.query[qi % ds.query.len()];
                qi += 1;
                black_box(compare::query_rs(&rsh, q))
            })
        });
    }
    g.finish();

    // ---- removal latency (fresh index per batch; only ours supports delete) ----
    // instant-distance and hnsw_rs build immutable graphs with no public delete,
    // so there's nothing to compare against here.
    let mut g = c.benchmark_group("remove");
    g.sample_size(10).measurement_time(Duration::from_secs(15));
    let n_del = (ds.base.len() / 10).max(1); // remove 10% of the index
    let del_ids: Vec<u32> = (0..n_del as u32).collect();
    g.throughput(Throughput::Elements(n_del as u64)); // per-remove amortized
    g.bench_function("ANNy", |b| {
        b.iter_batched(
            || build_ours(&ds.base),
            |mut ix| {
                for &id in &del_ids {
                    ix.remove(black_box(id));
                }
                black_box(ix)
            },
            BatchSize::SmallInput,
        )
    });
    g.finish();
}

criterion_group!(b, benches);
criterion_main!(b);
