//! `render-probe`: what would it cost to re-render a page on demand?
//!
//! `data/pages` holds a permanent JPEG of every page of every document —
//! several times the size of the source files it was derived from. Treating
//! it as an evictable cache instead only works if a cold re-render is fast
//! enough to happen inside a page request while someone is scrolling. This
//! measures that, against a real library, before anything deletes a byte.
//!
//! Three phases are timed separately because they fail differently:
//!
//!   open    CGPDFDocument::with_url. The serve path has no handle cache, so
//!           it would pay this on *every* page request, on a file that may
//!           be hundreds of megabytes. If this dominates, the answer is a
//!           handle LRU, not abandoning the design — hence `--warm-handle`,
//!           which reuses one open document across a doc's sampled pages and
//!           gives the counterfactual.
//!   render  the rasterize itself.
//!   encode  JPEG encode + write, at `--quality`.
//!
//! **Strictly read-only against the library.** The fjall stores are never
//! opened (which is what lets this run while the app is up), `meta.db` is
//! opened through [`Meta::open_readonly`], and renders go to a temp dir.
//! The report carries a `no_writes` field backed by a before/after snapshot
//! of the data dir rather than a promise.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use library_core::meta::Meta;
use serde_json::{Value, json};

use crate::status::{self, DocState};
use crate::{Source, source_state};

/// One sampled page.
struct PageSample {
    open_ms: f64,
    render_ms: f64,
    encode_ms: f64,
    bytes_out: u64,
}

impl PageSample {
    /// What a cold request pays: open + render + encode.
    fn cold_ms(&self) -> f64 {
        self.open_ms + self.render_ms + self.encode_ms
    }

    /// What it pays with the document already open.
    fn warm_ms(&self) -> f64 {
        self.render_ms + self.encode_ms
    }
}

/// One document's outcome: either samples, or why it has none.
struct DocResult {
    doc: String,
    pages: usize,
    src_bytes: u64,
    page_bytes: u64,
    samples: Vec<PageSample>,
    /// `None` when the doc rendered; otherwise the cause label.
    failed: Option<String>,
}

pub struct Args {
    pub data: PathBuf,
    pub docs: Option<usize>,
    pub only: Vec<String>,
    pub pages_per_doc: usize,
    pub width: u32,
    pub quality: f64,
    pub warm_handle: bool,
    pub out: Option<PathBuf>,
    pub hot: bool,
}

/// Pick `n` page numbers (1-based) spread across `total`, always including
/// the first and last — a book's first pages are often a scanned cover and
/// its last a colophon, and both render unlike the body.
fn sample_pages(total: usize, n: usize) -> Vec<usize> {
    if total == 0 || n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![1];
    }
    if total <= n {
        return (1..=total).collect();
    }
    let mut out: Vec<usize> = (0..n).map(|i| 1 + (i * (total - 1)) / (n - 1)).collect();
    out.dedup();
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (
        percentile(&v, 50.0),
        percentile(&v, 95.0),
        percentile(&v, 99.0),
    )
}

/// Total bytes and newest mtime under `path` — the before/after snapshot
/// that backs the report's `no_writes` claim.
fn tree_stamp(path: &Path) -> (u64, u64) {
    let (mut bytes, mut newest) = (0u64, 0u64);
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if let Ok(md) = e.metadata() {
                bytes += md.len();
                let secs = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                newest = newest.max(secs);
            }
        }
    }
    (bytes, newest)
}

fn dir_bytes(path: &Path) -> u64 {
    tree_stamp(path).0
}

/// Render `pages` of one document into `tmp`, timing each phase.
fn probe_doc(
    src: &Path,
    pages: &[usize],
    width: u32,
    quality: f64,
    warm_handle: bool,
    tmp: &Path,
) -> Result<Vec<PageSample>> {
    let mut out = Vec::with_capacity(pages.len());

    // --warm-handle opens once for the whole document; the cost is charged
    // to the first page so the totals stay honest, and every page after it
    // measures what a serve path with a handle cache would pay.
    let mut warm_open_ms = 0.0;
    let held = if warm_handle {
        let t = Instant::now();
        let d = crate::ocr::open_pdf(src)?.0;
        warm_open_ms = t.elapsed().as_secs_f64() * 1000.0;
        Some(d)
    } else {
        None
    };

    for (i, &page) in pages.iter().enumerate() {
        // Cold: reopen per page, which is exactly what the serve path does
        // today — no handle cache anywhere.
        let mut open_ms = 0.0;
        let cold = if held.is_some() {
            if i == 0 {
                open_ms = warm_open_ms;
            }
            None
        } else {
            let t = Instant::now();
            let d = crate::ocr::open_pdf(src)?.0;
            open_ms = t.elapsed().as_secs_f64() * 1000.0;
            Some(d)
        };
        let doc_ref = held
            .as_ref()
            .or(cold.as_ref())
            .expect("one of the two handles is always open");

        let t = Instant::now();
        let img = crate::ocr::render_page(doc_ref, page, width)?;
        let render_ms = t.elapsed().as_secs_f64() * 1000.0;

        let jpg = tmp.join(format!("probe-{i:04}.jpg"));
        let t = Instant::now();
        crate::ocr::save_jpeg_at(&img, &jpg, quality)?;
        let encode_ms = t.elapsed().as_secs_f64() * 1000.0;

        let bytes_out = std::fs::metadata(&jpg).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&jpg);

        out.push(PageSample {
            open_ms,
            render_ms,
            encode_ms,
            bytes_out,
        });
    }
    Ok(out)
}

/// Run the probe. The caller applies `be_gentle()` for a non-`hot` run, as
/// every other long subcommand does — `args.hot` is recorded here only so
/// the report says which machine it was measuring.
pub fn run(args: Args) -> Result<()> {
    let data = &args.data;
    let meta = Meta::open_readonly(data)
        .with_context(|| format!("opening {}/meta.db read-only", data.display()))?;

    let before = tree_stamp(data);

    // Ready docs only: a queued or failed doc has no settled page set to
    // compare against, and a deleted one is not something anybody will ask
    // to read.
    let statuses = status::scan(&meta);
    let mut docs: Vec<String> = if args.only.is_empty() {
        statuses
            .iter()
            .filter(|(_, s)| s.state == DocState::Ready)
            .map(|(id, _)| id.clone())
            .collect()
    } else {
        args.only.clone()
    };
    docs.sort();
    if let Some(n) = args.docs {
        docs.truncate(n);
    }

    let tmp = std::env::temp_dir().join(format!("render-probe-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;

    let mut results = Vec::new();
    for doc in &docs {
        let pages_dir = data.join("pages").join(doc);
        let ocr_dir = data.join("ocr").join(doc);
        // page count from the OCR sidecars: they are what survives eviction,
        // so they are what the cache design will count pages from too
        let total = std::fs::read_dir(&ocr_dir)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_str()
                            .is_some_and(|n| n.starts_with("page-") && n.ends_with(".json"))
                    })
                    .count()
            })
            .unwrap_or(0);
        let page_bytes = dir_bytes(&pages_dir);

        let state = source_state(&meta, doc);
        let Source::Ready(src) = &state else {
            results.push(DocResult {
                doc: doc.clone(),
                pages: total,
                src_bytes: 0,
                page_bytes,
                samples: Vec::new(),
                failed: Some(state.cause().into()),
            });
            continue;
        };
        let src_bytes = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);

        // images are one page and go through a different decoder; the cache
        // question is about books, so they are counted and skipped
        if crate::SourceKind::of(src) != Some(crate::SourceKind::Pdf) {
            results.push(DocResult {
                doc: doc.clone(),
                pages: total,
                src_bytes,
                page_bytes,
                samples: Vec::new(),
                failed: Some("not_a_pdf".into()),
            });
            continue;
        }

        let pages = sample_pages(total, args.pages_per_doc);
        let probed = probe_doc(
            src,
            &pages,
            args.width,
            args.quality,
            args.warm_handle,
            &tmp,
        );
        match probed {
            Ok(samples) => results.push(DocResult {
                doc: doc.clone(),
                pages: total,
                src_bytes,
                page_bytes,
                samples,
                failed: None,
            }),
            Err(e) => {
                eprintln!("  {doc}: {e:#}");
                results.push(DocResult {
                    doc: doc.clone(),
                    pages: total,
                    src_bytes,
                    page_bytes,
                    samples: Vec::new(),
                    failed: Some("render_failed".into()),
                })
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    let after = tree_stamp(data);
    let no_writes = before == after;

    print_report(&args, &results, no_writes);
    if let Some(path) = &args.out {
        let payload = json_report(&args, &results, no_writes);
        std::fs::write(path, serde_json::to_string_pretty(&payload)?)?;
        println!("\nwrote {}", path.display());
    }
    Ok(())
}

fn git_rev() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn timestamp() -> String {
    // date -u keeps us out of chrono for one field, as retrieval-eval does
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn print_report(args: &Args, results: &[DocResult], no_writes: bool) {
    let ok: Vec<&DocResult> = results.iter().filter(|r| r.failed.is_none()).collect();
    let bad: Vec<&DocResult> = results.iter().filter(|r| r.failed.is_some()).collect();

    println!(
        "\nrender-probe · {} · {} · width={} quality={} warm_handle={} hot={}",
        git_rev(),
        timestamp(),
        args.width,
        args.quality,
        args.warm_handle,
        args.hot
    );
    println!(
        "data={} · docs={} probed, {} un-renderable · no_writes={no_writes}",
        args.data.display(),
        ok.len(),
        bad.len()
    );

    println!("\n| doc | pages | src MiB | pages MiB | p50 ms | p95 ms | KiB/pg |");
    println!("|---|---|---|---|---|---|---|");
    for r in &ok {
        let cold: Vec<f64> = r.samples.iter().map(PageSample::cold_ms).collect();
        let (p50, p95, _) = stats(cold);
        let bytes: f64 = r.samples.iter().map(|s| s.bytes_out as f64).sum();
        let per = if r.samples.is_empty() {
            0.0
        } else {
            bytes / r.samples.len() as f64 / 1024.0
        };
        println!(
            "| {} | {} | {:.0} | {:.0} | {p50:.0} | {p95:.0} | {per:.0} |",
            r.doc,
            r.pages,
            mib(r.src_bytes),
            mib(r.page_bytes),
        );
    }

    let all_cold: Vec<f64> = ok
        .iter()
        .flat_map(|r| r.samples.iter().map(PageSample::cold_ms))
        .collect();
    let all_warm: Vec<f64> = ok
        .iter()
        .flat_map(|r| r.samples.iter().map(PageSample::warm_ms))
        .collect();
    let all_open: Vec<f64> = ok
        .iter()
        .flat_map(|r| r.samples.iter().map(|s| s.open_ms))
        .collect();
    let n = all_cold.len();
    let (c50, c95, c99) = stats(all_cold);
    let (w50, w95, w99) = stats(all_warm);
    let (o50, _, _) = stats(all_open);

    println!("\n{n} pages sampled");
    println!("| phase | p50 | p95 | p99 |");
    println!("|---|---|---|---|");
    println!("| open+render+encode | {c50:.0}ms | {c95:.0}ms | {c99:.0}ms |");
    println!("| render+encode | {w50:.0}ms | {w95:.0}ms | {w99:.0}ms |");
    println!("| open (median) | {o50:.0}ms | | |");

    let src_total: u64 = ok.iter().map(|r| r.src_bytes).sum();
    let page_total: u64 = results.iter().map(|r| r.page_bytes).sum();
    let pinned: u64 = bad.iter().map(|r| r.page_bytes).sum();

    // projected: mean bytes/page from the samples, times the real page count
    let projected: f64 = ok
        .iter()
        .map(|r| {
            if r.samples.is_empty() {
                return 0.0;
            }
            let mean: f64 =
                r.samples.iter().map(|s| s.bytes_out as f64).sum::<f64>() / r.samples.len() as f64;
            mean * r.pages as f64
        })
        .sum();

    println!(
        "\nsources {:.0} MiB · renders {:.0} MiB",
        mib(src_total),
        mib(page_total)
    );
    println!(
        "projected re-render {:.0} MiB ({:+.0}% vs the renders on disk)",
        projected / (1024.0 * 1024.0),
        if page_total > 0 {
            (projected / page_total as f64 - 1.0) * 100.0
        } else {
            0.0
        }
    );
    println!(
        "pinned (un-renderable) {:.0} MiB = {:.0}% of renders",
        mib(pinned),
        if page_total > 0 {
            pinned as f64 / page_total as f64 * 100.0
        } else {
            0.0
        }
    );

    if !bad.is_empty() {
        println!("\n| un-renderable doc | pages | renders MiB | cause |");
        println!("|---|---|---|---|");
        let mut by_cause: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &bad {
            let cause = r.failed.as_deref().unwrap_or("?");
            *by_cause.entry(cause).or_default() += 1;
            println!(
                "| {} | {} | {:.0} | {cause} |",
                r.doc,
                r.pages,
                mib(r.page_bytes)
            );
        }
        println!("\ncauses: {by_cause:?}");
    }
}

fn json_report(args: &Args, results: &[DocResult], no_writes: bool) -> Value {
    let ok: Vec<&DocResult> = results.iter().filter(|r| r.failed.is_none()).collect();
    let sampled: usize = ok.iter().map(|r| r.samples.len()).sum();
    let (c50, c95, c99) = stats(
        ok.iter()
            .flat_map(|r| r.samples.iter().map(PageSample::cold_ms))
            .collect(),
    );
    let (w50, w95, w99) = stats(
        ok.iter()
            .flat_map(|r| r.samples.iter().map(PageSample::warm_ms))
            .collect(),
    );

    json!({
        // provenance: what produced these numbers, so two runs are comparable
        "meta": {
            "subcommand": "render-probe",
            "git_rev": git_rev(),
            "timestamp": timestamp(),
            "data_path": args.data.display().to_string(),
            "docs_probed": ok.len(),
            "pages_sampled": sampled,
            "pages_per_doc": args.pages_per_doc,
            "width": args.width,
            "quality": args.quality,
            "warm_handle": args.warm_handle,
            // be_gentle() drops to background QoS, which makes latency
            // pessimistic by a large factor — a gate read off a non-hot run
            // is measuring the wrong machine
            "hot": args.hot,
            "no_writes": no_writes,
        },
        "cold_ms": { "p50": c50, "p95": c95, "p99": c99 },
        "warm_ms": { "p50": w50, "p95": w95, "p99": w99 },
        "docs": results.iter().map(|r| json!({
            "doc": r.doc,
            "pages": r.pages,
            "src_bytes": r.src_bytes,
            "page_bytes": r.page_bytes,
            "sampled": r.samples.len(),
            "cause": r.failed,
            "mean_bytes_out": if r.samples.is_empty() { 0.0 } else {
                r.samples.iter().map(|s| s.bytes_out as f64).sum::<f64>()
                    / r.samples.len() as f64
            },
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_pages_always_spans_the_book() {
        let s = sample_pages(1219, 5);
        assert_eq!(s.first(), Some(&1), "must include the first page");
        assert_eq!(s.last(), Some(&1219), "must include the last page");
        assert_eq!(s.len(), 5);
        // and be spread, not clustered
        assert!(s.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn sample_pages_handles_short_and_degenerate_books() {
        assert_eq!(sample_pages(3, 5), vec![1, 2, 3]);
        assert_eq!(sample_pages(1, 5), vec![1]);
        assert_eq!(sample_pages(0, 5), Vec::<usize>::new());
        assert_eq!(sample_pages(100, 0), Vec::<usize>::new());
        assert_eq!(sample_pages(100, 1), vec![1]);
    }

    #[test]
    fn percentiles_interpolate_and_survive_one_sample() {
        let v = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&v, 50.0), 2.5);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 100.0), 4.0);
        assert_eq!(percentile(&[7.0], 95.0), 7.0);
        assert_eq!(percentile(&[], 95.0), 0.0);
    }

    // The classifier the probe, the serve path and the eviction sweep all
    // share, so its arms are worth pinning: a doc with no row must not read
    // as "missing", because the two mean different things to a sweep — one
    // is un-renderable forever, the other might come back.
    #[test]
    fn source_state_distinguishes_no_row_from_missing() {
        let dir = std::env::temp_dir().join(format!("probe-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = Meta::open_in_memory().unwrap();

        assert_eq!(source_state(&meta, "ghost"), Source::NoRow);
        assert_eq!(Source::NoRow.cause(), "no_source_row");
        assert!(Source::NoRow.path().is_none());

        let present = dir.join("book.pdf");
        std::fs::write(&present, b"%PDF-1.4\n").unwrap();
        assert_eq!(
            Source::Ready(present.clone()).path(),
            Some(present.as_path())
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
