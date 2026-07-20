//! The shared ingest loop: drive one doc from whatever state it's in to
//! `ready`, from any host.
//!
//! Two rules make multi-process coordination safe:
//!
//! 1. **The filesystem is the queue.** [`pending`] derives the work list
//!    from `data/pdfs/` + the status files; nothing is queued in memory.
//! 2. **Whoever holds the fjall store owns ingestion.** The store is
//!    single-process (fjall's directory lock), so commits arbitrate
//!    naturally: the app commits through its live engine, the CLI worker
//!    opens the store per-commit and holds it only for the swap. When the
//!    CLI's commit finds the store locked (the app launched mid-prepare),
//!    the prepared records are *staged* to disk and the worker exits — the
//!    app's next sweep commits them without recomputing.
//!
//! Concurrent prepares on the same doc are prevented by per-doc claim
//! files ([`claim`]); the documented race is: CLI mid-prepare → app
//! launches → app sweep sees a live claim and skips → CLI commit hits
//! `Locked`, stages, exits (claim dropped) → app's *periodic* sweep picks
//! up the staged doc. This is why hosts must sweep periodically, not just
//! at startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use library_core::perf::IngestMetrics;
use library_core::{ChunkRec, ImageRec, Images, Library};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::status::{self, DocState, DocStatus};
use crate::{IngestCtx, Progress, ProgressFn};

// ---------------------------------------------------------------------------
// per-doc claims
// ---------------------------------------------------------------------------

/// Exclusive right to work on one doc, backed by `data/status/<doc>.lock`
/// holding the owner's PID. Removed on drop; a claim whose PID is dead is
/// stale and gets broken by the next claimant.
pub struct Claim {
    path: PathBuf,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn claim_path(data: &Path, doc: &str) -> PathBuf {
    status::dir(data).join(format!("{doc}.lock"))
}

fn pid_alive(pid: i32) -> bool {
    // signal 0: existence check only. EPERM still means "exists".
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether a live process (possibly this one) currently owns `doc`.
pub fn claimed(data: &Path, doc: &str) -> bool {
    match std::fs::read_to_string(claim_path(data, doc)) {
        Ok(s) => match s.trim().parse::<i32>() {
            Ok(pid) => pid_alive(pid),
            Err(_) => false, // unreadable claim: stale
        },
        Err(_) => false,
    }
}

/// Try to claim `doc`, breaking a stale (dead-PID) claim once.
pub fn claim(data: &Path, doc: &str) -> Option<Claim> {
    let path = claim_path(data, doc);
    std::fs::create_dir_all(status::dir(data)).ok()?;
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
                return Some(Claim { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if claimed(data, doc) {
                    return None; // live owner
                }
                let _ = std::fs::remove_file(&path); // stale: break and retry
            }
            Err(_) => return None,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// staged records: prepared but uncommitted
// ---------------------------------------------------------------------------

fn staged_dir(data: &Path, doc: &str) -> PathBuf {
    data.join("staged").join(doc)
}

fn stage<T: Serialize>(data: &Path, doc: &str, file: &str, recs: &T) -> Result<()> {
    let dir = staged_dir(data, doc);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file);
    let tmp = dir.join(format!("{file}.tmp"));
    std::fs::write(&tmp, postcard::to_stdvec(recs)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// A staged file that fails to read or decode is treated as absent — the
/// prepare phase just runs again (page caches make that cheap).
fn staged<T: DeserializeOwned>(data: &Path, doc: &str, file: &str) -> Option<T> {
    let bytes = std::fs::read(staged_dir(data, doc).join(file)).ok()?;
    postcard::from_bytes(&bytes).ok()
}

pub fn clear_staged(data: &Path, doc: &str) {
    let _ = std::fs::remove_dir_all(staged_dir(data, doc));
}

// ---------------------------------------------------------------------------
// commit seam: the only host-specific part
// ---------------------------------------------------------------------------

pub enum CommitErr {
    /// Another process holds the store; stage and let it finish the doc.
    Locked,
    Other(anyhow::Error),
}

/// How a host applies a prepared batch to the stores. The app commits
/// through its live engine (never `Locked`); the CLI opens the store per
/// commit, so the store is held only for the swap, never across a prepare.
pub trait Committer {
    fn text(&mut self, doc: &str, recs: &[ChunkRec]) -> Result<(usize, usize), CommitErr>;
    fn figures(&mut self, doc: &str, recs: &[ImageRec]) -> Result<(usize, usize), CommitErr>;
}

/// Committer for a process that doesn't hold the stores open (the CLI
/// worker): open → commit (incl. checkpoint) → drop, per call.
pub struct ProcessCommitter {
    pub data: PathBuf,
}

fn open_err(e: fjall::Error) -> CommitErr {
    match e {
        fjall::Error::Locked => CommitErr::Locked,
        e => CommitErr::Other(e.into()),
    }
}

impl Committer for ProcessCommitter {
    fn text(&mut self, doc: &str, recs: &[ChunkRec]) -> Result<(usize, usize), CommitErr> {
        let mut st: Library =
            library_core::try_open(self.data.join("library.db")).map_err(open_err)?;
        Ok(crate::commit_text(&mut st, doc, recs))
    }

    fn figures(&mut self, doc: &str, recs: &[ImageRec]) -> Result<(usize, usize), CommitErr> {
        let mut st: Images =
            library_core::try_open_images(self.data.join("images.db")).map_err(open_err)?;
        Ok(crate::commit_figures(&mut st, doc, recs))
    }
}

// ---------------------------------------------------------------------------
// the queue
// ---------------------------------------------------------------------------

/// The work list: every source file in `data/pdfs/` whose status is absent
/// or non-terminal. `preparing` counts only when its claim is dead — a live
/// claim means some process is already on it.
pub fn pending(data: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data.join("pdfs")) else {
        return Vec::new();
    };
    let mut docs: Vec<String> = entries
        .filter_map(|e| {
            let p = e.ok()?.path();
            crate::SourceKind::of(&p)?;
            let doc = p.file_stem()?.to_string_lossy().into_owned();
            let wanted = match status::read(data, &doc) {
                None => true,
                Some(st) => match st.state {
                    DocState::Queued | DocState::Staged | DocState::TextReady => true,
                    DocState::Preparing => !claimed(data, &doc),
                    DocState::Ready | DocState::Failed | DocState::Deleted => false,
                },
            };
            wanted.then_some(doc)
        })
        .collect();
    docs.sort();
    // a cross-format collision on disk (photo.pdf + photo.png) must not
    // double-process the doc; add/move refuse to create one, but the folder
    // is user-visible and files can land there by hand
    docs.dedup();
    docs
}

/// Backfill core: docs with no status file at all that `indexed` confirms
/// are already in the library — pre-status-era ingests. Writes `ready` for
/// them so they never get pointlessly re-ingested.
pub fn backfill_ready_with(
    data: &Path,
    docs: &[String],
    mut indexed: impl FnMut(&str) -> bool,
) -> Result<()> {
    for doc in docs {
        if status::read(data, doc).is_none() && indexed(doc) {
            status::write(data, doc, &DocStatus::new(DocState::Ready))?;
        }
    }
    Ok(())
}

/// Whether `doc` has chunks in the library's manifest.
pub fn manifest_has(st: &Library, doc: &str) -> bool {
    st.rtx(|(_, (manifest, _))| !manifest.search(&doc.to_string()).is_empty())
}

/// [`backfill_ready_with`] for a process that doesn't hold the store open.
/// Returns `false` without writing anything if the store is locked (the
/// lock holder runs its own backfill).
pub fn backfill_ready(data: &Path, docs: &[String]) -> Result<bool> {
    if docs.iter().all(|d| status::read(data, d).is_some()) {
        return Ok(true); // nothing to backfill: skip the store open
    }
    let st = match library_core::try_open(data.join("library.db")) {
        Ok(st) => st,
        Err(fjall::Error::Locked) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    backfill_ready_with(data, docs, |doc| manifest_has(&st, doc))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// process one doc
// ---------------------------------------------------------------------------

pub enum Outcome {
    /// Fully indexed.
    Ready,
    /// Store locked at commit; records staged for the lock holder. The CLI
    /// worker should exit on this — every later commit would block too.
    Staged,
    /// Another live process has the doc claimed.
    Skipped,
    /// Prepare or commit failed; status holds the error.
    Failed,
}

/// Mirror `Progress` into the doc's status file, throttled so OCR of a
/// 400-page book doesn't fsync 400 times.
struct StatusMirror<'a> {
    data: &'a Path,
    doc: &'a str,
    last: std::time::Instant,
    stage: &'static str,
}

impl StatusMirror<'_> {
    fn update(&mut self, p: &Progress) {
        let (stage, done, total) = match *p {
            Progress::Log(_) | Progress::OcrSummary { .. } => return,
            Progress::Ocr { done, total } => ("ocr", done as u64, total as u64),
            Progress::Clean { done, total } => ("clean", done as u64, total as u64),
            Progress::Embed { done, total } => ("embed", done as u64, total as u64),
            Progress::Figures { done, total } => ("figures", done as u64, total as u64),
            Progress::Clip { done, total } => ("clip", done as u64, total as u64),
            Progress::Indexing => ("indexing", 0, 0),
        };
        if stage == self.stage && self.last.elapsed().as_millis() < 1000 {
            return;
        }
        self.stage = stage;
        self.last = std::time::Instant::now();
        let _ = status::write(
            self.data,
            self.doc,
            &DocStatus {
                stage: Some(stage.to_string()),
                done,
                total,
                ..DocStatus::new(DocState::Preparing)
            },
        );
    }
}

/// Accumulates the persisted ingest metrics: per-stage wall-clock from the
/// `Progress` stream (a stage's time runs from its first event to the next
/// stage's first event), counts from stage totals and commit returns, plus
/// explicitly timed sections (commits, legibility). Seeded with a prior
/// run's metrics so a resumed doc sums across runs rather than losing the
/// committed stages' numbers.
struct MetricsClock {
    m: IngestMetrics,
    stage: &'static str,
    started: std::time::Instant,
}

impl MetricsClock {
    fn new(prior: Option<IngestMetrics>) -> Self {
        MetricsClock {
            m: prior.unwrap_or_default(),
            stage: "",
            started: std::time::Instant::now(),
        }
    }

    fn add(&mut self, name: &str, ms: u64) {
        *self
            .m
            .timings_ms
            .get_or_insert_default()
            .entry(name.to_string())
            .or_insert(0) += ms;
    }

    /// Close the open stage, attributing its elapsed time.
    fn close(&mut self) {
        if !self.stage.is_empty() {
            let ms = self.started.elapsed().as_millis() as u64;
            let stage = self.stage;
            self.add(stage, ms);
            self.stage = "";
        }
    }

    fn flip(&mut self, stage: &'static str) {
        if stage != self.stage {
            self.close();
            self.stage = stage;
            self.started = std::time::Instant::now();
        }
    }

    fn update(&mut self, p: &Progress) {
        match *p {
            Progress::Log(_) => {}
            Progress::OcrSummary {
                text_layer,
                vision,
                cached,
            } => {
                self.m.ocr = Some((text_layer, vision, cached));
            }
            Progress::Ocr { total, .. } => {
                self.m.pages = Some(total);
                self.flip("ocr");
            }
            Progress::Clean { .. } => self.flip("clean"),
            Progress::Embed { .. } => self.flip("embed"),
            Progress::Figures { .. } => self.flip("figures"),
            Progress::Clip { .. } => self.flip("clip"),
            Progress::Indexing => self.flip("indexing"),
        }
    }

    /// Metrics as of now (open stage closed, `total` and stamp refreshed),
    /// for attaching to a status transition.
    fn snapshot(&mut self, t0: std::time::Instant) -> IngestMetrics {
        self.close();
        let m = self.m.timings_ms.get_or_insert_default();
        *m.entry("total".to_string()).or_insert(0) = m
            .iter()
            .filter(|(k, _)| *k != "total")
            .map(|(_, v)| v)
            .sum::<u64>()
            .max(t0.elapsed().as_millis() as u64);
        self.m.at = library_core::perf::now_ms();
        self.m.clone()
    }
}

/// Drive `doc` from whatever state it's in toward `ready`. Idempotent and
/// resume-safe: staged records commit without recompute, page caches make
/// a redone prepare cheap, and commits are diff-based upserts.
pub fn process_doc(
    ctx: &IngestCtx,
    doc: &str,
    committer: &mut dyn Committer,
    progress: ProgressFn,
) -> Outcome {
    let data = &ctx.data;
    let Some(_claim) = claim(data, doc) else {
        return Outcome::Skipped;
    };
    let prior_status = status::read(data, doc);
    let prior = prior_status.as_ref().map(|s| s.state);
    if prior == Some(DocState::Ready) || prior == Some(DocState::Deleted) {
        return Outcome::Ready; // nothing to do; don't resurrect tombstones
    }

    let t0 = std::time::Instant::now();
    let mut clock = MetricsClock::new(prior_status.and_then(|s| s.metrics));
    let mut mirror = StatusMirror {
        data,
        doc,
        last: std::time::Instant::now(),
        stage: "",
    };

    let indexing = || DocStatus {
        stage: Some("indexing".to_string()),
        ..DocStatus::new(DocState::Preparing)
    };

    // -- text ---------------------------------------------------------------
    if prior != Some(DocState::TextReady) {
        // pages come back from a fresh prepare; a staged commit regenerates
        // the markdown edition from the page caches instead
        let (recs, pages): (Vec<ChunkRec>, Option<Vec<crate::PageOcr>>) =
            match staged(data, doc, "text.postcard") {
                Some(recs) => (recs, None),
                None => {
                    let _ = status::write(data, doc, &DocStatus::new(DocState::Preparing));
                    let Some(src) = crate::source_path(data, doc) else {
                        let _ = status::write(
                            data,
                            doc,
                            &DocStatus::failed(format!(
                                "source file for '{doc}' missing from data/pdfs"
                            )),
                        );
                        return Outcome::Failed;
                    };
                    let res = crate::prepare_text(ctx, &src, doc, None, &mut |p| {
                        mirror.update(&p);
                        clock.update(&p);
                        progress(p);
                    });
                    match res {
                        Ok((recs, pages)) => (recs, Some(pages)),
                        Err(e) => {
                            let _ = status::write(data, doc, &DocStatus::failed(format!("{e:#}")));
                            return Outcome::Failed;
                        }
                    }
                }
            };

        let _ = status::write(data, doc, &indexing());
        let t = std::time::Instant::now();
        match committer.text(doc, &recs) {
            Ok((removed, added)) => {
                clock.add("commit_text", t.elapsed().as_millis() as u64);
                clock.m.chunks = Some((added as u32, removed as u32));
                let _ = std::fs::remove_file(staged_dir(data, doc).join("text.postcard"));
                let md = match &pages {
                    Some(pages) => crate::textout::write_doc_pages(data, doc, pages),
                    None => crate::textout::write_doc(data, doc),
                };
                if let Err(e) = md {
                    progress(Progress::Log(format!("text edition failed: {e:#}")));
                }
                // OCR-quality summary while the page caches are warm; the
                // perf view reads it off the status file
                let t = std::time::Instant::now();
                clock.m.legibility = library_core::perf::legibility_summary(data, doc);
                clock.add("legibility", t.elapsed().as_millis() as u64);
                let _ = status::write(
                    data,
                    doc,
                    &DocStatus {
                        stage: Some("figures".to_string()),
                        metrics: Some(clock.snapshot(t0)),
                        ..DocStatus::new(DocState::TextReady)
                    },
                );
            }
            Err(CommitErr::Locked) => {
                if stage(data, doc, "text.postcard", &recs).is_err() {
                    // can't stage either: leave `preparing` (stale claim),
                    // the next sweep redoes prepare from the caches
                    return Outcome::Failed;
                }
                let _ = status::write(data, doc, &DocStatus::new(DocState::Staged));
                return Outcome::Staged;
            }
            Err(CommitErr::Other(e)) => {
                let _ = status::write(data, doc, &DocStatus::failed(format!("{e:#}")));
                return Outcome::Failed;
            }
        }
    }

    // -- figures --------------------------------------------------------------
    let figs: Vec<ImageRec> = match staged(data, doc, "figures.postcard") {
        Some(figs) => figs,
        None => {
            let res = crate::prepare_figures(ctx, doc, &mut |p| {
                mirror.update(&p);
                clock.update(&p);
                progress(p);
            });
            match res {
                Ok(figs) => figs,
                Err(e) => {
                    let _ = status::write(data, doc, &DocStatus::failed(format!("{e:#}")));
                    return Outcome::Failed;
                }
            }
        }
    };

    let _ = status::write(data, doc, &indexing());
    let t = std::time::Instant::now();
    match committer.figures(doc, &figs) {
        Ok((removed, added)) => {
            clock.add("commit_figures", t.elapsed().as_millis() as u64);
            clock.m.figures = Some((added as u32, removed as u32));
            clear_staged(data, doc);
            let _ = status::write(
                data,
                doc,
                &DocStatus {
                    metrics: Some(clock.snapshot(t0)),
                    ..DocStatus::new(DocState::Ready)
                },
            );
            Outcome::Ready
        }
        Err(CommitErr::Locked) => {
            if stage(data, doc, "figures.postcard", &figs).is_err() {
                return Outcome::Failed;
            }
            // stay `text_ready`, not `staged`: text is committed, and the
            // resume path must skip straight to the staged figures
            let _ = status::write(
                data,
                doc,
                &DocStatus {
                    stage: Some("staged".to_string()),
                    ..DocStatus::new(DocState::TextReady)
                },
            );
            Outcome::Staged
        }
        Err(CommitErr::Other(e)) => {
            let _ = status::write(data, doc, &DocStatus::failed(format!("{e:#}")));
            Outcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("worker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pdfs")).unwrap();
        dir
    }

    fn ctx(data: &Path) -> IngestCtx {
        IngestCtx {
            data: data.to_path_buf(),
            width: 1600,
            clean: false,
            text_layer: true,
        }
    }

    fn touch_pdf(data: &Path, doc: &str) {
        std::fs::write(data.join("pdfs").join(format!("{doc}.pdf")), b"%PDF-").unwrap();
    }

    fn touch_src(data: &Path, name: &str) {
        std::fs::write(data.join("pdfs").join(name), b"bytes").unwrap();
    }

    fn set(data: &Path, doc: &str, state: DocState) {
        status::write(data, doc, &DocStatus::new(state)).unwrap();
    }

    /// A PID that certainly ran and certainly exited.
    fn dead_pid() -> i32 {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let mut child = child;
        child.wait().unwrap();
        pid
    }

    /// Scripted committer: each call pops the next result.
    #[derive(Default)]
    struct Mock {
        text: Vec<Result<(), ()>>,    // Err(()) => Locked
        figures: Vec<Result<(), ()>>, // Err(()) => Locked
        text_calls: usize,
        figures_calls: usize,
    }

    impl Committer for Mock {
        fn text(&mut self, _doc: &str, _recs: &[ChunkRec]) -> Result<(usize, usize), CommitErr> {
            self.text_calls += 1;
            match self.text.remove(0) {
                Ok(()) => Ok((0, 0)),
                Err(()) => Err(CommitErr::Locked),
            }
        }
        fn figures(&mut self, _doc: &str, _recs: &[ImageRec]) -> Result<(usize, usize), CommitErr> {
            self.figures_calls += 1;
            match self.figures.remove(0) {
                Ok(()) => Ok((0, 0)),
                Err(()) => Err(CommitErr::Locked),
            }
        }
    }

    fn seed_staged(data: &Path, doc: &str, text: bool, figures: bool) {
        let empty_text: Vec<ChunkRec> = Vec::new();
        let empty_figs: Vec<ImageRec> = Vec::new();
        if text {
            stage(data, doc, "text.postcard", &empty_text).unwrap();
        }
        if figures {
            stage(data, doc, "figures.postcard", &empty_figs).unwrap();
        }
    }

    fn nop(_: Progress) {}

    #[test]
    fn claim_is_exclusive_and_breaks_stale() {
        let data = tmp("claim");
        let c = claim(&data, "a").expect("first claim");
        assert!(claimed(&data, "a"));
        assert!(claim(&data, "a").is_none(), "live claim must hold");
        drop(c);
        assert!(!claimed(&data, "a"));

        // a dead PID's claim is stale: broken and re-taken
        std::fs::write(claim_path(&data, "a"), dead_pid().to_string()).unwrap();
        assert!(!claimed(&data, "a"));
        assert!(claim(&data, "a").is_some(), "stale claim must break");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn pending_truth_table() {
        let data = tmp("pending");
        for doc in [
            "absent",
            "queued",
            "staged",
            "textready",
            "prep-stale",
            "prep-live",
            "ready",
            "failed",
            "deleted",
        ] {
            touch_pdf(&data, doc);
        }
        set(&data, "queued", DocState::Queued);
        set(&data, "staged", DocState::Staged);
        set(&data, "textready", DocState::TextReady);
        set(&data, "ready", DocState::Ready);
        set(&data, "failed", DocState::Failed);
        set(&data, "deleted", DocState::Deleted);
        set(&data, "prep-stale", DocState::Preparing);
        std::fs::write(claim_path(&data, "prep-stale"), dead_pid().to_string()).unwrap();
        set(&data, "prep-live", DocState::Preparing);
        let _live = claim(&data, "prep-live").unwrap();

        assert_eq!(
            pending(&data),
            vec!["absent", "prep-stale", "queued", "staged", "textready"]
        );
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn pending_accepts_images_and_dedups_collisions() {
        let data = tmp("pending-img");
        touch_src(&data, "photo.png");
        touch_src(&data, "scan.JPG");
        touch_src(&data, "note.txt"); // not ingestible
        // a cross-format collision that landed on disk by hand: one entry
        touch_src(&data, "both.pdf");
        touch_src(&data, "both.png");

        assert_eq!(pending(&data), vec!["both", "photo", "scan"]);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn staged_records_commit_to_ready() {
        let data = tmp("ready");
        touch_pdf(&data, "a");
        set(&data, "a", DocState::Staged);
        seed_staged(&data, "a", true, true);

        let mut mock = Mock {
            text: vec![Ok(())],
            figures: vec![Ok(())],
            ..Default::default()
        };
        assert!(matches!(
            process_doc(&ctx(&data), "a", &mut mock, &mut nop),
            Outcome::Ready
        ));
        assert_eq!((mock.text_calls, mock.figures_calls), (1, 1));
        assert_eq!(status::read(&data, "a").unwrap().state, DocState::Ready);
        assert!(!staged_dir(&data, "a").exists(), "staged dir cleared");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn locked_text_commit_stages_and_exits() {
        let data = tmp("locktext");
        touch_pdf(&data, "a");
        set(&data, "a", DocState::Queued);
        seed_staged(&data, "a", true, false);

        let mut mock = Mock {
            text: vec![Err(())],
            ..Default::default()
        };
        assert!(matches!(
            process_doc(&ctx(&data), "a", &mut mock, &mut nop),
            Outcome::Staged
        ));
        assert_eq!(status::read(&data, "a").unwrap().state, DocState::Staged);
        assert!(staged_dir(&data, "a").join("text.postcard").exists());
        assert_eq!(mock.figures_calls, 0, "must stop before figures");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn locked_figures_commit_stays_text_ready() {
        let data = tmp("lockfigs");
        touch_pdf(&data, "a");
        set(&data, "a", DocState::Staged);
        seed_staged(&data, "a", true, true);

        let mut mock = Mock {
            text: vec![Ok(())],
            figures: vec![Err(())],
            ..Default::default()
        };
        assert!(matches!(
            process_doc(&ctx(&data), "a", &mut mock, &mut nop),
            Outcome::Staged
        ));
        // text committed: resume must skip to the staged figures
        let st = status::read(&data, "a").unwrap();
        assert_eq!(st.state, DocState::TextReady);
        assert!(staged_dir(&data, "a").join("figures.postcard").exists());
        assert!(!staged_dir(&data, "a").join("text.postcard").exists());

        // resume: only the figures commit runs
        let mut mock = Mock {
            figures: vec![Ok(())],
            ..Default::default()
        };
        assert!(matches!(
            process_doc(&ctx(&data), "a", &mut mock, &mut nop),
            Outcome::Ready
        ));
        assert_eq!((mock.text_calls, mock.figures_calls), (0, 1));
        assert_eq!(status::read(&data, "a").unwrap().state, DocState::Ready);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn claimed_doc_is_skipped() {
        let data = tmp("skip");
        touch_pdf(&data, "a");
        let _held = claim(&data, "a").unwrap();
        let mut mock = Mock::default();
        assert!(matches!(
            process_doc(&ctx(&data), "a", &mut mock, &mut nop),
            Outcome::Skipped
        ));
        assert_eq!((mock.text_calls, mock.figures_calls), (0, 0));
        std::fs::remove_dir_all(&data).unwrap();
    }
}
