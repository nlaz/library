//! Admission control for on-demand page renders.
//!
//! Page images are a bounded cache now, so a request can miss and have to
//! rasterize from the source file — ~160ms p50, 256ms p95 on a real library.
//! That is affordable one at a time and ruinous unbounded: a single search
//! response carries 20-26 hits, each firing a page-image request, and the
//! protocol handler runs on tokio's shared blocking pool that ingest and
//! every other spawn_blocking also draw from.
//!
//! So three rules, in the order a request meets them:
//!
//!   coalesce  two requests for the same page produce one render. Six hits
//!             from one book, or two panes showing the same page, collapse.
//!   limit     at most RENDER_WORKERS rasterize at once — each holds a full
//!             page bitmap, and they compete with the search the user is
//!             actually waiting on.
//!   shed      past RENDER_QUEUE_MAX, refuse immediately rather than queue.
//!
//! Shedding is only safe because the reader retries (see reader-model.ts).
//! A refused request that nobody comes back for is a lost page, not
//! backpressure.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Renders running at once. Two rather than three: unlike the ingest's OCR
/// pool, these run while the user is searching and scrolling, and each holds
/// a page bitmap.
pub(crate) const RENDER_WORKERS: usize = 2;

/// Requests in the system — running plus waiting — before new ones are shed.
pub(crate) const RENDER_QUEUE_MAX: usize = 8;

/// How long a follower waits for the render it is piggybacking on. Past
/// this it gives up and lets the caller answer as a miss; a render that has
/// taken this long is stuck, and holding the blocking-pool thread hoping
/// otherwise helps nobody.
const FOLLOW_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) type PageKey = (String, u32);

#[derive(Default)]
struct GateState {
    /// Requests admitted and not yet finished, plus those waiting for a slot.
    in_system: usize,
    running: usize,
    /// Pages currently being rendered, so duplicates can wait rather than
    /// duplicate the work.
    inflight: HashMap<PageKey, ()>,
}

pub(crate) struct RenderGate {
    state: Mutex<GateState>,
    /// A worker slot came free.
    room: Condvar,
    /// An in-flight render finished.
    finished: Condvar,
}

/// What a request may do.
pub(crate) enum Admit<'a> {
    /// Render it. The guard releases the slot when dropped.
    Go(Slot<'a>),
    /// Someone else rendered this exact page while we waited — re-check the
    /// file before deciding anything.
    Followed,
    /// Too much in flight. Answer 503 and let the client come back.
    Busy,
}

pub(crate) struct Slot<'a> {
    gate: &'a RenderGate,
    key: PageKey,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut st = self.gate.state.lock().expect("render gate poisoned");
        st.running -= 1;
        st.in_system -= 1;
        st.inflight.remove(&self.key);
        drop(st);
        // a slot freed wakes one waiter; a finished page may wake several
        self.gate.room.notify_one();
        self.gate.finished.notify_all();
    }
}

impl RenderGate {
    pub(crate) fn new() -> RenderGate {
        RenderGate {
            state: Mutex::new(GateState::default()),
            room: Condvar::new(),
            finished: Condvar::new(),
        }
    }

    /// Ask to render `key`.
    pub(crate) fn admit(&self, key: PageKey) -> Admit<'_> {
        let mut st = self.state.lock().expect("render gate poisoned");

        // Someone is already making this exact page. Wait for them rather
        // than doing it twice — but count as in-system while waiting, or a
        // hot page could park unbounded threads here.
        if st.inflight.contains_key(&key) {
            if st.in_system >= RENDER_QUEUE_MAX {
                return Admit::Busy;
            }
            st.in_system += 1;
            let mut waited = Duration::ZERO;
            while st.inflight.contains_key(&key) && waited < FOLLOW_TIMEOUT {
                let (next, timeout) = self
                    .finished
                    .wait_timeout(st, FOLLOW_TIMEOUT - waited)
                    .expect("render gate poisoned");
                st = next;
                if timeout.timed_out() {
                    waited = FOLLOW_TIMEOUT;
                }
            }
            st.in_system -= 1;
            return Admit::Followed;
        }

        if st.in_system >= RENDER_QUEUE_MAX {
            return Admit::Busy;
        }
        st.in_system += 1;

        while st.running >= RENDER_WORKERS {
            st = self.room.wait(st).expect("render gate poisoned");
            // Another waiter may have claimed this page while we slept. Do
            // not start a second render of it; report the wait instead and
            // let the caller re-check the file.
            if st.inflight.contains_key(&key) {
                st.in_system -= 1;
                return Admit::Followed;
            }
        }
        st.running += 1;
        st.inflight.insert(key.clone(), ());
        drop(st);
        Admit::Go(Slot { gate: self, key })
    }

    #[cfg(test)]
    fn depth(&self) -> (usize, usize) {
        let st = self.state.lock().expect("render gate poisoned");
        (st.in_system, st.running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(n: u32) -> PageKey {
        ("doc".to_string(), n)
    }

    #[test]
    fn a_slot_is_released_when_its_guard_drops() {
        let gate = RenderGate::new();
        {
            let a = gate.admit(key(1));
            assert!(matches!(a, Admit::Go(_)));
            assert_eq!(gate.depth(), (1, 1));
        }
        assert_eq!(gate.depth(), (0, 0));
    }

    #[test]
    fn concurrent_renders_are_capped() {
        let gate = RenderGate::new();
        let mut held = Vec::new();
        for i in 0..RENDER_WORKERS {
            held.push(gate.admit(key(i as u32)));
        }
        assert!(held.iter().all(|a| matches!(a, Admit::Go(_))));
        assert_eq!(gate.depth().1, RENDER_WORKERS);
    }

    // The rule that keeps a search's 20-odd page requests from becoming
    // 20-odd concurrent rasterizations on the pool that ingest shares.
    // Shedding is safe only because the reader retries.
    #[test]
    fn a_full_queue_sheds_instead_of_growing() {
        let gate = Arc::new(RenderGate::new());
        // fill every worker slot and hold them
        let mut held = Vec::new();
        for i in 0..RENDER_WORKERS {
            held.push(gate.admit(key(i as u32)));
        }

        // park waiters on distinct pages until the system is full
        let waiting = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for i in 0..(RENDER_QUEUE_MAX - RENDER_WORKERS) {
            let (g, w) = (Arc::clone(&gate), Arc::clone(&waiting));
            threads.push(std::thread::spawn(move || {
                w.fetch_add(1, Ordering::SeqCst);
                let _ = g.admit(key(100 + i as u32));
            }));
        }
        // let them all reach the gate
        while waiting.load(Ordering::SeqCst) < RENDER_QUEUE_MAX - RENDER_WORKERS {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            matches!(gate.admit(key(999)), Admit::Busy),
            "past the cap a request must be refused, not queued"
        );

        drop(held);
        for t in threads {
            let _ = t.join();
        }
    }

    // Six search hits from the same book must cost one render, not six.
    #[test]
    fn a_duplicate_request_follows_instead_of_rendering_again() {
        let gate = Arc::new(RenderGate::new());
        let leader = gate.admit(key(7));
        assert!(matches!(leader, Admit::Go(_)));

        let g = Arc::clone(&gate);
        let follower = std::thread::spawn(move || matches!(g.admit(key(7)), Admit::Followed));
        std::thread::sleep(Duration::from_millis(50));

        // the follower is still parked: it has not been told to render
        assert_eq!(gate.depth().1, 1, "only the leader is running");
        drop(leader);
        assert!(follower.join().unwrap(), "the duplicate must follow");
        assert_eq!(gate.depth(), (0, 0));
    }
}
