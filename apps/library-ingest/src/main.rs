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
    },
    /// Add an already-ingested doc to a collection (creates it if needed).
    Collect {
        collection: String,
        doc: String,
        #[arg(long, default_value = "data")]
        data: PathBuf,
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
    IngestCtx { data: data.to_path_buf(), width, clean: false }
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
    }
}

fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Ingest { pdf, data, limit, width, name, collection, hot, clean, text_only } => {
            if !hot {
                be_gentle();
            }
            ingest(&pdf, &data, limit, width, name, collection, clean, text_only)
        }
        Cli::Collect { collection, doc, data } => {
            collect(&data, &collection, &doc)?;
            println!("collection '{collection}' += '{doc}'");
            Ok(())
        }
        Cli::Images { doc, data, hot } => {
            if !hot {
                be_gentle();
            }
            ingest_images(&doc, &data)
        }
        Cli::Clean { doc, data, apply_only } => {
            let changed = if apply_only {
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
        Cli::LayoutDebug { doc, pages, data, out } => layout_debug(&doc, &pages, &data, &out),
        Cli::Search { query, data, k, lex_only } => search(&query, &data, k, lex_only),
    }
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
) -> Result<()> {
    let ctx = ctx(data, width);
    let (doc, pdf) = add_pdf(&ctx, pdf, name)?;

    // The model pass runs before anything else touches ort: prepare_text
    // then just re-applies the cached edits.
    if clean {
        library_ingest::ocr::ocr_pdf(
            &pdf,
            &data.join("pages").join(&doc),
            &data.join("ocr").join(&doc),
            width,
            limit,
            &mut print_progress,
        )?;
        library_ingest::clean::clean_doc(data, &doc, &mut print_progress)?;
    }

    let t = Instant::now();
    let recs = prepare_text(&ctx, &pdf, &doc, limit, &mut print_progress)?;
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

    let md = library_ingest::textout::write_doc(&data, &doc)?;
    println!("text edition: {}", md.display());

    if let Some(col) = collection {
        collect(data, &col, &doc)?;
    }
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
        let mut parts: Vec<library_core::Bbox> = Vec::new();
        for d in &dets {
            if d.class.is_figure() && d.bbox[2] * d.bbox[3] >= layout::AREA_MIN {
                parts.extend(subdivide::subdivide(&img, d.bbox));
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
    let hits = st.rtx(|r| library_core::search(&r, query, qemb.as_ref(), k, None));
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
