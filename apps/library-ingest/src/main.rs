//! Offline ingest + query CLI for The Library. The pipeline itself lives in
//! the library crate (src/lib.rs) so the desktop app can run it in-process;
//! this binary parses args, prints progress, and composes the phases.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use library_core::{Word, tokenize};
use library_ingest::{
    IngestCtx, Progress, add_pdf, collect, layout, prepare_figures, prepare_text, subdivide,
};

/// Drop the whole process (Vision OCR, ort's worker threads) to background
/// QoS + nice 15 + throttled disk I/O so a long ingest never starves the
/// machine. fastembed spins one ort thread per core with no way to cap it,
/// so priority is the only lever. BACKGROUND (E-cores only) over UTILITY:
/// on the 8GB machine a slower ingest beats a swapping one, and the user
/// keeps their P-cores.
fn be_gentle() {
    // not in the libc crate: <sys/resource.h> IOPOL_TYPE_DISK=0,
    // IOPOL_SCOPE_PROCESS=0, IOPOL_THROTTLE=3
    unsafe extern "C" {
        fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
    }
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 15);
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
        setiopolicy_np(0, 0, 3);
    }
}

#[derive(Parser)]
enum Cli {
    /// OCR a PDF, chunk + embed it, and load it into the fold store.
    Ingest {
        pdf: PathBuf,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Only process the first N pages (for quick runs).
        #[arg(long)]
        limit: Option<usize>,
        /// Rendered page-image width in pixels.
        #[arg(long, default_value_t = 1600)]
        width: u32,
        /// Doc id override (default: slugified file stem).
        #[arg(long)]
        name: Option<String>,
        /// Also add the doc to this collection (see `collect`).
        #[arg(long)]
        collection: Option<String>,
        /// Run at full priority instead of background QoS.
        #[arg(long)]
        hot: bool,
        /// Run the model-backed OCR cleanup (tools/clean-pages) as part of
        /// the ingest. Keeps the ~2GB on-device model resident for the
        /// whole pass (about an hour per book) — cached edits are applied
        /// even without this flag.
        #[arg(long)]
        clean: bool,
        /// Skip the figure/CLIP rebuild. Use when only the text changed
        /// (e.g. re-ingesting after `clean`): the figure pipeline reruns
        /// YOLO layout over every page and is the most expensive stage.
        #[arg(long)]
        text_only: bool,
        /// OCR every page even when the PDF embeds a text layer (for PDFs
        /// whose producer embedded garbage OCR).
        #[arg(long)]
        no_text_layer: bool,
    },
    /// Add an already-ingested doc to a collection (creates it if needed).
    Collect {
        collection: String,
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
    },
    /// Rebuild a doc's full index (text + figures + markdown) from its
    /// cached OCR/page files alone — no source PDF needed. For docs whose
    /// caches survive a store-schema change but whose PDF is gone.
    Reindex {
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        #[arg(long)]
        hot: bool,
    },
    /// Rank ingested docs by OCR legibility, worst first — the shortlist
    /// for `re-ocr`. Scores what search/chat actually serve (cached OCR
    /// with clean overlays applied).
    Audit {
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Only docs in this collection (fuzzy, e.g. "whole-earth").
        #[arg(long)]
        col: Option<String>,
        /// Worst pages to list per doc.
        #[arg(long, default_value_t = 3)]
        worst: usize,
    },
    /// Force re-OCR of a doc from its source PDF with Apple Vision,
    /// ignoring any embedded text layer, then rebuild its index. For docs
    /// whose producer embedded garbage OCR (e.g. Internet Archive scans,
    /// whose text layer is decades-old multi-column OCR). Clears the doc's
    /// ocr/clean/edits caches first; run with the app closed (store lock).
    ReOcr {
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Rendered page-image width in pixels.
        #[arg(long, default_value_t = 1600)]
        width: u32,
        /// Run at full priority instead of background QoS.
        #[arg(long)]
        hot: bool,
    },
    /// (Re)build the figure index for an already-ingested doc from its
    /// cached OCR + page images. `ingest` runs this automatically.
    Images {
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        #[arg(long)]
        hot: bool,
    },
    /// Run the model-backed OCR cleanup for an already-ingested doc:
    /// tools/clean-pages proposes edits (cached in data/edits/<doc>), gated
    /// + applied to data/clean/<doc>. `ingest` runs this automatically.
    /// Re-run `ingest` (or `text`) afterwards to pick up the cleaned pages.
    Clean {
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Skip the model: just re-apply cached edits (e.g. after a gate
        /// change or to rebuild data/clean from scratch).
        #[arg(long)]
        apply_only: bool,
    },
    /// (Re)write the markdown edition (`data/text/<doc>.md`) from cached
    /// OCR/cleaned pages. `ingest` runs this automatically.
    Text {
        /// Doc id, or omit with --all for every ingested doc.
        doc: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "data")]
        data: PathBuf,
    },
    /// Open both stores and write fresh HNSW graph blobs if stale, so the
    /// next open loads instead of rebuilding. (Ingest does this itself.)
    Checkpoint {
        #[arg(long, default_value = "data")]
        data: PathBuf,
    },
    /// Remove a doc from the library: retract its index entries, delete its
    /// pages/ocr/text derivatives, clear collection membership + title, and
    /// mark it Deleted so the worker never re-ingests it. The source PDF in
    /// data/pdfs is left in place. Refuses while the doc is mid-ingest.
    /// (Same semantics as the desktop app's delete, runnable offline.)
    Delete {
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
    },
    /// Run the layout model on specific pages and write annotated JPEGs,
    /// for tuning thresholds/classes before a re-ingest.
    LayoutDebug {
        doc: String,
        /// Comma-separated page numbers, e.g. "249,254,149".
        #[arg(long)]
        pages: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Where annotated images go.
        #[arg(long, default_value = "layout-debug")]
        out: PathBuf,
    },
    /// Process every pending PDF in data/pdfs (safe to run from launchd:
    /// exits immediately if the app holds the stores). A PDF is pending
    /// when its status file (data/status/<doc>.json) is absent or
    /// non-terminal — drop a PDF into data/pdfs and run this.
    Worker {
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Run at full priority instead of background QoS.
        #[arg(long)]
        hot: bool,
    },
    /// Install (or repair) the launchd agent that runs `worker` in the
    /// background: on login, every 15 minutes, and whenever a file lands
    /// in data/pdfs. The app does this automatically on startup.
    InstallAgent {
        #[arg(long, default_value = "data")]
        data: PathBuf,
    },
    /// Hybrid search against the store.
    Search {
        query: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        #[arg(short, default_value_t = 10)]
        k: usize,
        /// Skip the embedding model: lexical-only, cold start in milliseconds.
        #[arg(long)]
        lex_only: bool,
    },
}

fn ctx(data: &Path, width: u32) -> IngestCtx {
    IngestCtx { data: data.to_path_buf(), width, clean: false, text_layer: true }
}

/// Render pipeline progress the way the old monolithic CLI did.
fn print_progress(p: Progress) {
    match p {
        Progress::Log(line) => println!("{line}"),
        Progress::Ocr { done, total } => {
            if done % 5 == 0 || done == total {
                println!("  ocr {done}/{total}");
            }
        }
        Progress::Clean { done, total } => {
            if done % 5 == 0 || done == total {
                println!("  clean {done}/{total}");
            }
        }
        Progress::Embed { done, total } => {
            if done % (16 * 8) < 16 || done == total {
                println!("  embed {done}/{total}");
            }
        }
        Progress::Figures { .. } => {}
        Progress::Clip { done, total } => {
            if done % 64 < 8 || done == total {
                println!("  clip {done}/{total}");
            }
        }
        Progress::Indexing => println!("  indexing"),
    }
}

fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Ingest { pdf, data, limit, width, name, collection, hot, clean, text_only, no_text_layer } => {
            if !hot {
                be_gentle();
            }
            ingest(&pdf, &data, limit, width, name, collection, clean, text_only, no_text_layer)
        }
        Cli::Collect { collection, doc, data } => {
            collect(&data, &collection, &doc)?;
            println!("collection '{collection}' += '{doc}'");
            Ok(())
        }
        Cli::Reindex { doc, data, hot } => {
            if !hot {
                be_gentle();
            }
            reindex(&doc, &data)
        }
        Cli::Audit { data, col, worst } => audit(&data, col.as_deref(), worst),
        Cli::ReOcr { doc, data, width, hot } => {
            if !hot {
                be_gentle();
            }
            reocr(&doc, &data, width)
        }
        Cli::Images { doc, data, hot } => {
            if !hot {
                be_gentle();
            }
            ingest_images(&doc, &data)
        }
        Cli::Clean { doc, data, apply_only } => {
            let (changed, _) = if apply_only {
                library_ingest::clean::apply_edits(&data, &doc, &mut print_progress)?
            } else {
                library_ingest::clean::clean_doc(&data, &doc, &mut print_progress)?
            };
            if changed > 0 {
                println!("re-run `ingest` on '{doc}' (or `text {doc}`) to pick up the cleaned pages");
            }
            Ok(())
        }
        Cli::Text { doc, all, data } => {
            let docs: Vec<String> = match (doc, all) {
                (Some(d), false) => vec![d],
                (None, true) => {
                    let mut docs: Vec<String> = std::fs::read_dir(data.join("ocr"))
                        .context("no data/ocr directory")?
                        .filter_map(|e| {
                            let e = e.ok()?;
                            e.file_type().ok()?.is_dir().then(|| e.file_name().to_string_lossy().into_owned())
                        })
                        .collect();
                    docs.sort();
                    docs
                }
                _ => anyhow::bail!("pass a doc id or --all"),
            };
            for doc in docs {
                let path = library_ingest::textout::write_doc(&data, &doc)?;
                println!("wrote {}", path.display());
            }
            Ok(())
        }
        Cli::Delete { doc, data } => {
            use library_ingest::status::{self, DocState, DocStatus};
            use library_ingest::worker;
            if worker::claimed(&data, &doc)
                || status::read(&data, &doc).map(|s| s.state) == Some(DocState::Preparing)
            {
                anyhow::bail!("{doc}: still processing — try again when ingest finishes");
            }
            // retract from the stores first so nothing can hand out hits
            // whose page images are already gone (mirrors the app's delete)
            let t = Instant::now();
            {
                let mut st = library_core::open(data.join("library.db"));
                library_ingest::commit_text(&mut st, &doc, &[]);
            }
            {
                let mut ist = library_core::open_images(data.join("images.db"));
                library_ingest::commit_figures(&mut ist, &doc, &[]);
            }
            // derivatives; text/clean/edits too, or the chat tools' fuzzy
            // doc-id match keeps resolving a doc that no longer exists
            for dir in ["pages", "ocr", "clean", "edits"] {
                let p = data.join(dir).join(&doc);
                if let Err(e) = std::fs::remove_dir_all(&p) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        anyhow::bail!("removing {}: {e}", p.display());
                    }
                }
            }
            let md = data.join("text").join(format!("{doc}.md"));
            if let Err(e) = std::fs::remove_file(&md) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    anyhow::bail!("removing {}: {e}", md.display());
                }
            }
            worker::clear_staged(&data, &doc);
            status::write(&data, &doc, &DocStatus::new(DocState::Deleted))?;
            library_ingest::set_collections(&data, &doc, &[])?;
            let titles_path = data.join("titles.json");
            let mut titles: std::collections::BTreeMap<String, String> =
                std::fs::read(&titles_path)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default();
            if titles.remove(&doc).is_some() {
                std::fs::write(&titles_path, serde_json::to_vec_pretty(&titles)?)?;
            }
            println!("deleted {doc} in {:?} (source PDF kept in data/pdfs)", t.elapsed());
            Ok(())
        }
        Cli::Checkpoint { data } => {
            let t = Instant::now();
            let mut st = library_core::open(data.join("library.db"));
            st.checkpoint();
            println!("library.db checkpointed in {:?}", t.elapsed());
            let t = Instant::now();
            let mut ist = library_core::open_images(data.join("images.db"));
            ist.checkpoint();
            println!("images.db checkpointed in {:?}", t.elapsed());
            Ok(())
        }
        Cli::Worker { data, hot } => {
            if !hot {
                be_gentle();
            }
            worker(&data)
        }
        Cli::InstallAgent { data } => {
            // launchd needs absolute paths; "data" relative to the repo
            // would resolve against / when the agent fires
            let data = std::path::absolute(&data)?;
            let bin = std::env::current_exe()?;
            let path = library_ingest::agent::install(&bin, &data)?;
            println!("agent loaded: {}", path.display());
            println!("logs: {}/logs/ingest.log", data.display());
            println!("disable with: launchctl bootout gui/$UID/{}", library_ingest::agent::LABEL);
            Ok(())
        }
        Cli::LayoutDebug { doc, pages, data, out } => layout_debug(&doc, &pages, &data, &out),
        Cli::Search { query, data, k, lex_only } => search(&query, &data, k, lex_only),
    }
}

/// Drain the pending queue. Exit 0 without touching anything when another
/// process (the app) holds the stores — the lock holder owns ingestion and
/// runs this same loop itself.
fn worker(data: &Path) -> Result<()> {
    use library_ingest::worker::{self, Outcome, ProcessCommitter};

    let mut pend = worker::pending(data);
    if pend.is_empty() {
        println!("nothing to ingest");
        return Ok(());
    }

    // Pre-status-era docs are already indexed but have no status file;
    // mark them ready before treating "no status" as work. This open also
    // doubles as the cheap lock probe: locked -> the app is running.
    if !worker::backfill_ready(data, &pend)? {
        println!("stores locked (app running) — its worker owns the queue");
        return Ok(());
    }
    pend = worker::pending(data);

    let mut committer = ProcessCommitter { data: data.to_path_buf() };
    for doc in pend {
        println!("→ {doc}");
        match worker::process_doc(&ctx(data, 1600), &doc, &mut committer, &mut print_progress) {
            Outcome::Ready => println!("done: {doc}"),
            Outcome::Staged => {
                println!("stores locked mid-run — staged '{doc}' for the app; exiting");
                return Ok(());
            }
            Outcome::Skipped => println!("skipped (another process has it): {doc}"),
            // keep going: one bad PDF must not wedge the queue
            Outcome::Failed => eprintln!("failed: {doc} (see data/status/{doc}.json)"),
        }
    }
    Ok(())
}

fn ingest(
    pdf: &Path,
    data: &Path,
    limit: Option<usize>,
    width: u32,
    name: Option<String>,
    collection: Option<String>,
    clean: bool,
    text_only: bool,
    no_text_layer: bool,
) -> Result<()> {
    let mut ctx = ctx(data, width);
    ctx.clean = clean;
    ctx.text_layer = !no_text_layer;
    let (doc, pdf) = add_pdf(&ctx, pdf, name)?;

    let t = Instant::now();
    let (recs, pages) = prepare_text(&ctx, &pdf, &doc, limit, &mut print_progress)?;
    println!("prepared: {} chunks in {:?}", recs.len(), t.elapsed());

    let t = Instant::now();
    let mut st = library_core::open(data.join("library.db"));
    println!("open store: {:?}", t.elapsed());
    let t = Instant::now();
    let (removed, added) = library_ingest::commit_text(&mut st, &doc, &recs);
    println!("index: -{removed} +{added} chunks in {:?}", t.elapsed());
    drop(st);

    if text_only {
        println!("figures skipped (--text-only)");
    } else {
        ingest_images(&doc, data)?;
    }

    let md = library_ingest::textout::write_doc_pages(data, &doc, &pages)?;
    println!("text edition: {}", md.display());

    if let Some(col) = collection {
        collect(data, &col, &doc)?;
    }
    println!("done: doc '{doc}'");
    Ok(())
}

struct DocAudit {
    mean: f32,
    median: f32,
    /// Fraction of scored pages with an unquotable stretch (min_window
    /// below NOISY_MIN) — the number that actually predicts the chat
    /// agent quoting garbage, since column-interleaved salad hides inside
    /// pages whose *average* legibility looks fine.
    noisy: f32,
    scored: usize,
    total: usize,
    worst: Vec<(u32, f32)>,
}

/// Per-doc legibility from the same caches read_pages serves (clean
/// overlays applied).
fn audit_doc(data: &Path, doc: &str) -> Result<DocAudit> {
    use library_core::legibility::{NOISY_MIN, legibility, min_window};
    let pages = library_ingest::read_pages(data, doc)?;
    let total = pages.len();
    let mut scores: Vec<(u32, f32)> = Vec::new();
    let mut noisy_pages = 0usize;
    for p in &pages {
        let text: String =
            p.words.iter().map(|w| w.t.as_str()).collect::<Vec<_>>().join(" ");
        if text.len() < library_core::tools::BLANK_CHARS {
            continue;
        }
        scores.push((p.page, legibility(&text)));
        if min_window(&text) < NOISY_MIN {
            noisy_pages += 1;
        }
    }
    if scores.is_empty() {
        return Ok(DocAudit { mean: 0.0, median: 0.0, noisy: 0.0, scored: 0, total, worst: vec![] });
    }
    let mean = scores.iter().map(|(_, s)| s).sum::<f32>() / scores.len() as f32;
    let mut by_score = scores.clone();
    by_score.sort_by(|a, b| a.1.total_cmp(&b.1));
    let median = by_score[by_score.len() / 2].1;
    let noisy = noisy_pages as f32 / scores.len() as f32;
    Ok(DocAudit { mean, median, noisy, scored: scores.len(), total, worst: by_score })
}

fn audit(data: &Path, col: Option<&str>, worst: usize) -> Result<()> {
    let member = match library_core::tools::resolve_collection(data, col.unwrap_or("")) {
        Ok(m) => m,
        Err(e) => anyhow::bail!("{e}"),
    };
    let mut docs: Vec<String> = std::fs::read_dir(data.join("ocr"))
        .context("no data/ocr directory")?
        .filter_map(|e| {
            let e = e.ok()?;
            e.file_type().ok()?.is_dir().then(|| e.file_name().to_string_lossy().into_owned())
        })
        .collect();
    if let Some(m) = &member {
        docs.retain(|d| m.contains(d));
    }
    docs.sort();

    let mut rows = Vec::new();
    for doc in &docs {
        match audit_doc(data, doc) {
            Ok(r) => rows.push((doc.clone(), r)),
            Err(e) => eprintln!("{doc}: {e}"),
        }
    }
    // worst first = most unquotable pages first (see DocAudit::noisy)
    rows.sort_by(|a, b| b.1.noisy.total_cmp(&a.1.noisy).then(a.1.mean.total_cmp(&b.1.mean)));
    println!("{:>6}  {:>5}  {:>6}  {:>6}/{:<6}  worst pages", "noisy%", "mean", "median", "scored", "pages");
    for (doc, a) in &rows {
        let worst_pages: Vec<String> =
            a.worst.iter().take(worst).map(|(p, s)| format!("p.{p}={s:.2}")).collect();
        println!(
            "{:>6.1}  {:>5.2}  {:>6.2}  {:>6}/{:<6}  {doc}  {}",
            a.noisy * 100.0,
            a.mean,
            a.median,
            a.scored,
            a.total,
            worst_pages.join(" ")
        );
    }
    Ok(())
}

/// Vision-forced re-OCR from the source PDF, then a full per-doc reindex.
fn reocr(doc: &str, data: &Path, width: u32) -> Result<()> {
    let pdf = data.join("pdfs").join(format!("{doc}.pdf"));
    if !pdf.exists() {
        anyhow::bail!(
            "no source PDF at {} — re-OCR needs the original; `reindex` only rebuilds from caches",
            pdf.display()
        );
    }
    // clear every derivative of the old OCR: raw pages, and the clean/edits
    // overlays — prepare_text re-applies data/edits/<doc> whenever that dir
    // exists, and read_pages prefers data/clean/<doc>, so stale overlays
    // would resurrect the garbage this command exists to purge
    for sub in ["ocr", "clean", "edits"] {
        let dir = data.join(sub).join(doc);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => println!("cleared {}", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context(format!("clearing {}", dir.display())),
        }
    }
    ingest(
        &pdf,
        data,
        None,
        width,
        Some(doc.to_owned()),
        None,
        /*clean*/ false,
        /*text_only*/ false,
        /*no_text_layer*/ true,
    )?;
    let a = audit_doc(data, doc)?;
    println!(
        "legibility after re-OCR: mean {:.2} median {:.2} noisy {:.1}% ({}/{} pages scored)",
        a.mean,
        a.median,
        a.noisy * 100.0,
        a.scored,
        a.total
    );
    Ok(())
}

/// Rebuild text + figure indexes and the markdown edition from caches.
fn reindex(doc: &str, data: &Path) -> Result<()> {
    let ctx = ctx(data, 1600);
    let t = Instant::now();
    let (recs, pages) = library_ingest::prepare_text_cached(&ctx, doc, None, &mut print_progress)?;
    println!("prepared: {} chunks in {:?}", recs.len(), t.elapsed());

    let mut st = library_core::open(data.join("library.db"));
    let (removed, added) = library_ingest::commit_text(&mut st, doc, &recs);
    println!("index: -{removed} +{added} chunks");
    drop(st);

    ingest_images(doc, data)?;
    let md = library_ingest::textout::write_doc_pages(data, doc, &pages)?;
    println!("text edition: {}", md.display());
    println!("done: doc '{doc}'");
    Ok(())
}

fn ingest_images(doc: &str, data: &Path) -> Result<()> {
    let ctx = ctx(data, 1600);
    let t = Instant::now();
    let recs = prepare_figures(&ctx, doc, &mut print_progress)?;
    println!("figures: {} regions in {:?}", recs.len(), t.elapsed());

    let mut st = library_core::open_images(data.join("images.db"));
    let (removed, added) = library_ingest::commit_figures(&mut st, doc, &recs);
    println!("figure index: -{removed} +{added}");
    Ok(())
}

/// Run the layout model on chosen pages, print detections, and write
/// annotated JPEGs so thresholds/classes can be tuned by eye.
fn layout_debug(doc: &str, pages: &str, data: &Path, out: &Path) -> Result<()> {
    let model = layout::LayoutModel::load(data)?
        .context(format!("no layout model at {}", layout::LayoutModel::model_path(data).display()))?;
    std::fs::create_dir_all(out)?;

    for spec in pages.split(',') {
        let page: u32 = spec.trim().parse().context(format!("bad page number '{spec}'"))?;
        let jpg = data.join("pages").join(doc).join(format!("page-{page:04}.jpg"));
        let img = image::open(&jpg).context(format!("cannot open {}", jpg.display()))?;

        let t = Instant::now();
        let dets = model.detect(&img)?;
        println!("\n{doc} p.{page} — {} detections in {:?}", dets.len(), t.elapsed());

        // subdivision preview for each figure (needs &img before into_rgb8)
        let luma = img.thumbnail(library_ingest::PAGE_LUMA_PX, library_ingest::PAGE_LUMA_PX).into_luma8();
        let mut parts: Vec<library_core::Bbox> = Vec::new();
        for d in &dets {
            if d.class.is_figure() && d.bbox[2] * d.bbox[3] >= layout::AREA_MIN {
                parts.extend(subdivide::subdivide(&luma, (img.width(), img.height()), d.bbox));
            }
        }

        let mut canvas = img.into_rgb8();
        for d in &dets {
            let figure = d.class.is_figure() && d.bbox[2] * d.bbox[3] >= layout::AREA_MIN;
            println!(
                "  {:<14} {:.2}  [{:.3} {:.3} {:.3} {:.3}]{}",
                d.class.name(),
                d.score,
                d.bbox[0], d.bbox[1], d.bbox[2], d.bbox[3],
                if figure { "  <- figure" } else { "" },
            );
            let color = match d.class {
                layout::Class::Picture => [220, 40, 40],
                layout::Class::Table => [40, 90, 220],
                layout::Class::Formula => [30, 160, 60],
                layout::Class::Caption => [230, 150, 20],
                _ => [150, 150, 150],
            };
            draw_rect(&mut canvas, d.bbox, color, if figure { 4 } else { 2 });
        }
        for p in &parts {
            println!("  part            --  [{:.3} {:.3} {:.3} {:.3}]", p[0], p[1], p[2], p[3]);
            draw_rect(&mut canvas, *p, [40, 200, 220], 2);
        }
        let path = out.join(format!("{doc}-p{page:04}.jpg"));
        canvas.save(&path)?;
        println!("  -> {}", path.display());
    }
    Ok(())
}

fn draw_rect(img: &mut image::RgbImage, bbox: library_core::Bbox, color: [u8; 3], px: u32) {
    let (iw, ih) = (img.width(), img.height());
    let x0 = (bbox[0] * iw as f32) as u32;
    let y0 = (bbox[1] * ih as f32) as u32;
    let x1 = (((bbox[0] + bbox[2]) * iw as f32) as u32).min(iw - 1);
    let y1 = (((bbox[1] + bbox[3]) * ih as f32) as u32).min(ih - 1);
    for x in x0..=x1 {
        for t in 0..px {
            img.put_pixel(x, (y0 + t).min(ih - 1), image::Rgb(color));
            img.put_pixel(x, y1.saturating_sub(t), image::Rgb(color));
        }
    }
    for y in y0..=y1 {
        for t in 0..px {
            img.put_pixel((x0 + t).min(iw - 1), y, image::Rgb(color));
            img.put_pixel(x1.saturating_sub(t), y, image::Rgb(color));
        }
    }
}

fn search(query: &str, data: &Path, k: usize, lex_only: bool) -> Result<()> {
    let t = Instant::now();
    let st = library_core::open(data.join("library.db"));
    println!("open store (incl. hnsw rebuild): {:?}", t.elapsed());

    let qemb = if lex_only {
        None
    } else {
        // ese embeds at call time — no model load, cold start included
        Some(ese::encode_single(query))
    };

    let t = Instant::now();
    let hits = st.rtx(|r| {
        library_core::search(&r, query, qemb.as_ref(), k, None, true, |key| {
            st.get(key).map(|rec| rec.words)
        })
    });
    let dur = t.elapsed();

    let qtoks = tokenize(query);
    for (i, hit) in hits.iter().enumerate() {
        println!(
            "\n#{} score={:.4} {} p.{} (chunk {})",
            i + 1,
            hit.score,
            hit.key.doc,
            hit.key.page,
            hit.key.idx
        );
        println!("   {}", snippet(&hit.words, &qtoks));
    }
    println!("\nsearch: {} hits in {dur:?}", hits.len());
    Ok(())
}

/// A window of words around the first query-term match, match in brackets.
fn snippet(words: &[Word], qtoks: &[String]) -> String {
    let is_match = |w: &Word| {
        let t = tokenize(&w.t);
        t.iter().any(|t| qtoks.iter().any(|q| t.starts_with(q.as_str())))
    };
    let center = words.iter().position(is_match).unwrap_or(0);
    let lo = center.saturating_sub(10);
    let hi = (center + 15).min(words.len());
    words[lo..hi]
        .iter()
        .map(|w| {
            if is_match(w) {
                format!("[{}]", w.t)
            } else {
                w.t.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
