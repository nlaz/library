//! FSEvents watchers over the linked folders.
//!
//! The sweep already reconciles every root once a launch and once every
//! thirty seconds, which is correct but slow enough to feel broken: you
//! drop a PDF in Finder, switch to the app, and nothing is there. This
//! turns the wait into about a second.
//!
//! It is strictly a latency improvement, never a source of truth. The
//! watcher's only job is to wake the sweep, which then does the same full
//! reconcile it would have done anyway — so a missed event costs at most
//! one sweep interval, and a spurious one costs a scan. That is why nothing
//! here inspects *which* paths changed: acting on an event's contents would
//! make the watcher a second, subtly different implementation of the thing
//! `roots::reconcile` already does properly.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

/// How long to wait for a burst to settle before waking the sweep.
///
/// Copying a folder of scans in Finder is hundreds of events over several
/// seconds, and a scan per event would be pointless work on a moving
/// target. Long enough to coalesce a copy, short enough that a single drop
/// still feels immediate.
const DEBOUNCE: Duration = Duration::from_millis(700);

/// How often the root list is re-read, so a folder linked in Settings
/// starts being watched without a relaunch.
const ROOT_RECHECK: Duration = Duration::from_secs(5);

/// Watch every linked folder and wake the ingest sweep when one changes.
///
/// Runs until the app exits. Re-reads the root list periodically so a
/// folder linked in Settings starts being watched without a relaunch;
/// re-watching is cheap and idempotent, and a root that has gone away
/// (ejected volume) simply fails to register until it comes back.
pub(crate) fn watch_roots(ctx: library_core::meta::Ctx, wake: Sender<()>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            // any event at all is the same signal: something moved, go look
            if res.is_ok() {
                let _ = tx.send(());
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                // no watcher means the periodic sweep is the only mechanism —
                // slower, still correct, and worth saying out loud
                eprintln!(
                    "could not start the folder watcher ({e}); falling back to the 30s sweep"
                );
                return;
            }
        };

    let mut watched: Vec<PathBuf> = Vec::new();
    // in the past, so the first pass registers immediately rather than
    // leaving the folders unwatched for the first interval
    let mut last_roots_check = Instant::now() - ROOT_RECHECK;
    let mut pending_since: Option<Instant> = None;

    loop {
        // pick up newly linked folders without needing a relaunch
        if last_roots_check.elapsed() >= ROOT_RECHECK {
            last_roots_check = Instant::now();
            let current: Vec<PathBuf> = ctx
                .roots()
                .into_iter()
                .filter(|r| r.path.is_dir())
                .map(|r| r.path)
                .collect();
            for path in &current {
                if !watched.contains(path) && watcher.watch(path, RecursiveMode::Recursive).is_ok()
                {
                    watched.push(path.clone());
                }
            }
            for path in watched.clone() {
                if !current.contains(&path) {
                    let _ = watcher.unwatch(&path);
                    watched.retain(|p| p != &path);
                }
            }
        }

        // Wait briefly so the debounce can expire even when events stop
        // arriving — the last event of a burst is exactly the one whose
        // deadline nobody would otherwise come back to check.
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(()) => pending_since = Some(Instant::now()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // the watcher was dropped: nothing left to wait on
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }

        if let Some(since) = pending_since
            && since.elapsed() >= DEBOUNCE
        {
            pending_since = None;
            if wake.send(()).is_err() {
                return; // the worker is gone; so is the reason to watch
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spike this feature was supposed to start with: does FSEvents
    /// actually fire for a file written into a watched folder, and does the
    /// debounce let exactly one wake through for a burst?
    ///
    /// Real filesystem, real watcher, real timing — the parts that can't be
    /// reasoned about. Generous deadlines: this asserts "eventually", which
    /// is the only thing FSEvents promises.
    #[test]
    fn a_file_appearing_wakes_the_sweep_once_per_burst() {
        let dir = std::env::temp_dir().join(format!("watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = dir.join("Library");
        std::fs::create_dir_all(&lib).expect("root dir");

        let ctx = library_core::meta::Ctx::in_memory(&dir).expect("meta");
        ctx.add_root(&lib, 1).expect("link");

        let (wake, woke) = std::sync::mpsc::channel();
        let watcher_ctx = ctx.clone();
        std::thread::spawn(move || watch_roots(watcher_ctx, wake));

        // let the watcher register before touching anything; without the
        // fix that made the first registration immediate, this is where a
        // 5-second stall would show up
        std::thread::sleep(Duration::from_millis(400));

        // a burst: several files, as copying a folder of scans would be
        for i in 0..5 {
            std::fs::write(lib.join(format!("book-{i}.pdf")), b"%PDF-").expect("write");
            std::thread::sleep(Duration::from_millis(40));
        }

        woke.recv_timeout(Duration::from_secs(10))
            .expect("a file appearing must wake the sweep");

        // the burst coalesces: no second wake follows immediately after
        assert!(
            woke.recv_timeout(DEBOUNCE + Duration::from_millis(300))
                .is_err(),
            "one burst must not wake the sweep repeatedly"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
