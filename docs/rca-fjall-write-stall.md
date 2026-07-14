# RCA: Indefinite write-stall hang in fjall after macOS deep sleep

- **Date of incident:** 2026-07-13
- **Component:** `fjall 3.1.5` (LSM storage engine under the fold store), as used by `library-ingest worker`
- **Impact:** background ingestion silently frozen for ~10 hours; no data loss or corruption
- **Status:** recovered (process restart); root cause identified in fjall source; hardening proposed below

## Summary

A `library-ingest worker` (launchd background agent) hung for ~10 hours inside a
fjall write-transaction commit. The committing thread was parked in fjall's
write-stall loop — an unbounded sleep-poll on a backpressure counter that only
background flush/compaction workers can decrement — after a macOS deep-sleep
cycle interrupted the flush hand-off. The condition never became true again,
the loop has no timeout, no health check, and no re-arm path, so the process
polled forever at ~0% CPU while appearing alive to launchd and to `ps`.

The store was never at risk: the commit was unacknowledged, so killing the
process aborted the transaction cleanly; the restarted worker re-prepared the
document from caches and committed in seconds.

## Timeline (local time, 2026-07-13)

| Time | Event |
| --- | --- |
| 10:05 | launchd agent bootstrapped; worker (PID 96386) begins draining the ingest queue |
| ~10:27 | doc `il-cucchiaio-dargento-primi-piatti`: text embedding completes (`embed 1183/1183`); commit begins |
| ~10:40 | lid closed; machine enters Deep Idle (battery, `pmset -g log`), mid-commit |
| 10:40–20:28 | machine mostly asleep (periodic DarkWake maintenance windows of 20–500s) |
| 20:28 | real wake (lid open, user activity) |
| 20:28–20:49 | worker runnable but makes no progress: `SN` state, ~0% CPU, log frozen at `embed 1183/1183` |
| 20:49 | anomaly detected: elapsed 10h39m vs 36m CPU; stack sample taken |
| 20:52 | worker killed; re-triggered via `data/pdfs` WatchPaths |
| 20:53 | new worker re-prepares from caches (~20 s) and **the same commit succeeds** |
| ~21:00 | document fully indexed (`ready`) |

## Detection

Nothing detected it. The hang was found manually after a progress check showed
the same log line ten hours apart. Contributing blind spot: fjall reports
worker/flush failures via `log::error!`, and **no logger is initialized** in
`library-ingest` or the app — any error fjall did emit was dropped on the
floor.

## Evidence

`sample <pid>` (two runs, consistent):

```
main thread                                   all 4 "fjall:worker" threads
───────────                                   ────────────────────────────
library_ingest::commit_text                   fjall::worker_pool … closure
 └─ fold WriteTx::commit (inlined fjall)       └─ _dispatch_semaphore_wait_slow
     └─ nanosleep  ◄── the stall poll              └─ semaphore_wait_trap ◄── idle,
                                                       empty queue
```

The signature that distinguishes this from "slow disk": the **committer is in
`nanosleep`** while **every background worker is idle at its semaphore with no
queued work**. Genuine I/O pressure would show workers busy in write/fsync
stacks.

## Root cause

Three properties of fjall 3.1.5 combine into an unbounded hang; the sleep/wake
cycle is only the trigger.

### 1. Backpressure loops are unbounded, blind sleep-polls

`src/keyspace/mod.rs` (3.1.5):

```rust
fn check_write_halt(&self) {
    while self.tree.l0_run_count() >= 30 {
        std::thread::sleep(Duration::from_millis(10));      // no exit condition
    }
}

// local_backpressure():
while self.tree.sealed_memtable_count() >= 4 {
    std::thread::sleep(Duration::from_millis(100));         // no exit condition
}
```

No timeout, no progress detection, no poison check, no re-enqueue. The loop
*assumes* a flush/compaction is in flight and will eventually move the counter.

### 2. The waiter and the flusher are connected only by a counter

There is no signaling primitive (condvar/event) between flush completion and
the stalled writer — completion decrements a counter that the writer polls.
Additionally, flush work travels on **two loosely-coupled channels**
(`WorkerMessage::Flush` on the worker-pool channel; the actual `Task` on
`FlushManager`'s flume channel, popped with `try_recv`), so a consumed message
whose task is lost — or a task whose completion accounting is interrupted —
strands the counter with no pending work anywhere. A lost hand-off is
*invisible*: queues empty, workers idle, counter high, writer asleep.

```
 poller (write path)                      flusher (background)
 ───────────────────                      ────────────────────
 read counter ── high ── sleep ──┐        flush … ✗ interrupted by
        ▲                        │        system sleep; completion
        └────────────────────────┘        accounting / hand-off lost
        nothing ever connects these two again
```

### 3. Failure makes it worse: workers die permanently and poison is never re-checked

`src/worker_pool.rs`: any error in a worker tick logs, poisons, and **exits the
thread** — the pool shrinks for the life of the process, and the failed flush
task is not retried or re-queued. `src/poison_dart.rs`: poisoning just stores
an `AtomicBool`. That flag is checked **once at write entry**
(`keyspace/mod.rs:241`) — a thread already inside the stall loop never
re-checks it, so even an officially-poisoned database leaves stalled writers
sleeping forever.

### Trigger mechanism (confidence: medium on the specific path)

The deep-sleep cycle interrupted the flush hand-off between counter-increment
and completion-accounting. Two candidate paths fit the evidence; both are real
defects regardless of which fired here:

- **(a) Post-wake I/O error killed the flush**: macOS can surface transient
  errors on in-flight I/O across Deep Idle; `flush::worker::run` propagates the
  error, the worker poisons and exits, the task is dropped. (Weakly
  contradicted by 4 live workers in the sample — unless the pool started
  larger; with no logger, the error trail is unrecoverable.)
- **(b) Lost hand-off across the dual-channel design**: the `Flush` message was
  consumed and its effect (or its completion accounting) was swallowed by the
  sleep transition, leaving the sealed-memtable/L0 counter permanently above
  threshold with empty queues.

The unbounded loop (defect 1) converts either transient fault into a permanent
hang. That is the root cause; the trigger is weather.

## Why recovery was safe

fjall is journaled and transactional. The commit was never acknowledged, so
`kill` aborted the transaction; on next open, journal replay stopped at the
last complete record. The re-run re-prepared the document from page caches
(~20 s) and committed successfully — zero data loss, zero corruption.

## Hardening recommendations

### Upstream (fjall) — ordered by cost/benefit

1. **Bound the stall loops with a poison check** (~20 lines; converts hang → error):

   ```rust
   while self.tree.sealed_memtable_count() >= 4 {
       if self.is_poisoned.load(Ordering::Acquire) {
           return Err(Error::Poisoned);
       }
       std::thread::sleep(Duration::from_millis(100));
   }
   ```

   `check_write_halt` likewise (requires it to return `Result`).

2. **Self-heal on no-progress: re-arm the work.** Track the counter across
   iterations; if unchanged for N seconds, send another (idempotent)
   `WorkerMessage::Flush`/`Compact` and log a warning. This cures lost
   hand-offs from *any* cause — sleep/wake, channel races, dropped tasks —
   because the stall loop stops assuming work is in flight:

   ```rust
   if stalled_for > Duration::from_secs(5) && count == last_count {
       supervisor.worker_pool.send(WorkerMessage::Flush);
       log::warn!("write stall not progressing (sealed={count}); re-armed flush");
       stalled_for = Duration::ZERO;
   }
   ```

3. **Make stalls observable.** `log::warn!` after ~1 s of stall and
   periodically thereafter, including `l0_run_count`, `sealed_memtable_count`,
   and both queue lengths. Expected stalls are fine; *silent indefinite* stalls
   are the bug.

4. **Retry transient worker errors instead of thread suicide.** Bounded
   retries with backoff for I/O errors; poison only on persistent failure.
   And make `poison()` wake stalled writers (see 5) rather than only setting a
   flag nobody re-reads.

5. **(Structural) Replace the sleep-polls with `Condvar` signaling.** Flush
   completion `notify_all`s; the stall loop `wait_timeout`s and re-checks under
   the lock. Lost wakeups become impossible by construction, and the timeout
   doubles as the self-heal hook for 2. Touches the concurrency design, so
   maintainer's call — 1–3 stand alone without it.

   Suggested PR shape: **1 + 2 + 3** as "bounded, self-healing, observable
   write stalls", with 4 and 5 as follow-ups.

### Local (this repo) — independent of upstream

- **Initialize a logger** in `library-ingest` main and the app engine init
  (e.g. `env_logger`, `RUST_LOG=fjall=warn`), so fjall's existing error
  reporting lands in `data/logs/ingest.log` instead of vanishing. One line;
  removes the blind spot that made this a mystery.
- **Hold a power assertion across prepare→commit** (`IOPMAssertionCreate`
  "PreventUserIdleSystemSleep", or wrap the worker in `caffeinate -i`), so the
  machine cannot deep-sleep inside the commit window. Sleeps during
  OCR/embed/figures pause harmlessly; only the commit window is exposed.
- **Watchdog in the worker loop:** if a doc's status stays `preparing` with no
  stage progress for > 30 min while the process is alive, log loudly (and
  optionally abort the doc → `failed` so the queue keeps moving; the claim/
  status machinery already supports re-queueing).

## Diagnostic runbook (if it recurs)

1. `ps -o pid,etime,time,%cpu,stat -p <pid>` — long elapsed, tiny CPU, `SN`.
2. `sample <pid> 2` — committer in `nanosleep` under `WriteTx::commit` **and**
   all `fjall:worker` threads in `semaphore_wait` ⇒ this bug (workers busy in
   fsync/write stacks ⇒ genuinely slow I/O instead).
3. `kill <pid>` — safe; the uncommitted transaction aborts.
4. Re-trigger: `touch data/pdfs` (WatchPaths) or wait for the 15-min
   StartInterval; caches make the re-run cheap.

See also: project memory `fjall-sleep-stall`.
