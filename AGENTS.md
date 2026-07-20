# Agent guide — The Library

Guidance for coding agents (and new contributors) working in this repo.
The README covers what the project is; this file covers how to work on it
safely and idiomatically.

## Architecture map

Reusable, cross-platform crates (the "Bog" stack):

- `anny` — HNSW approximate-nearest-neighbor index. Leaf crate.
- `ese` — static text embeddings compiled into the crate. Leaf crate.
  `build.rs` downloads model weights from HuggingFace on first build and
  caches them in `target/ese-cache/` (network once; wiped by `cargo clean`).
- `fold` — incremental dataflow over a fjall LSM store: streams of deltas
  pushed through typed pipeline operators into sinks (tables, BM25, HNSW).
  Depends on `anny`.

The Library app (macOS-only, in `apps/`):

- `library-core` — shared records, the fold search graph, hybrid
  lexical+semantic ranking, typeahead/fuzzy correction, agent tools.
  **No Apple deps — builds and tests on any platform.**
- `library-ingest` — OCR/layout/figure ingestion (Vision, PDFKit via
  `objc2-*`) + background worker. macOS-only to *compile*, but its unit
  tests exercise pure logic.
- `library-server` — WebTransport (QUIC) + HTTP search server.
- `library-app` — Tauri desktop app; owns stores, models, and the ingest
  worker in one process. Serves `apps/web/dist`.
- `apps/web` — TypeScript/Vite frontend (not a Cargo crate).
- `apps/librarian` — Swift sidecar wrapping Apple Foundation Models
  (SwiftPM, not Cargo).

Dependency layering: `anny`/`ese` → `fold` → `library-core` →
`{library-ingest, library-server, library-app}`. Keep it acyclic.

## Build & test

```sh
cargo test -p fold -p anny -p ese -p library-core   # cross-platform core
cargo test --workspace                              # everything (macOS only)
cargo test -p ese --features tests                  # ese golden-vector tests
npm --prefix apps/web run typecheck                 # web: tsc --noEmit
npm --prefix apps/web test                          # web: vitest
npm --prefix apps/web run build                     # web: tsc + vite build
```

- **Never build `--release` just to verify a change** — the workspace uses
  fat LTO and single codegen unit; release builds take minutes. Debug is
  fast enough: the hot-path crates are compiled at `opt-level = 2` in dev.
- First build downloads the ese model into `target/ese-cache/` (needs
  network once).
- Run binaries from the repo root — `data/` and `tools/clean-pages/`
  resolve relative to CWD.
- For end-to-end verification (boot the server/app and drive the UI), use
  the `verify` skill in `.claude/skills/verify/`.

## Data safety

- `data/` at the repo root is a **live personal library** (databases, PDFs,
  OCR output). Never write to it from tests, experiments, or scripts; never
  delete or "clean it up".
- Tests must be hermetic: temp dirs only, no network, no `data/`. See the
  fixture patterns below.
- fjall stores are **single-process** — a second opener panics with
  `Locked`. Stop `library-server` before running the CLI search or the app.

## Testing conventions

- Unit tests live in inline `#[cfg(test)] mod tests` next to the code they
  cover (crate-private access is the point). `fold` collects its tests
  under `fold/src/tests/`; integration tests use `tests/` dirs.
- Fixture patterns to reuse rather than reinvent:
  - `fold/src/tests/mod.rs::fresh_db` — temp-dir fjall store per test.
  - `apps/library-core/src/tools.rs::sample_fixture` — synthetic on-disk
    library (text pages + collections manifest).
  - `apps/library-core/tests/common/` — `synthetic_library`, a temp-dir
    fold graph populated with hand-built chunks and embeddings for
    end-to-end search tests. Pass a synthetic `qemb` — never load a real
    model in tests.
- `library_core::tokenize` and `lex_tokenize` **must agree** (TermDict
  completion terms only match BM25 postings tokenized the same way). The
  agreement test in `library-core` pins this contract — if you change
  either function, extend that test, don't delete it.
- New logic lands with its tests in the same commit. Bug fixes include a
  regression test that fails before the fix.
- Behavior-preserving refactors move code *and its tests* verbatim; a green
  suite before and after is the proof of preservation.

## Commit convention

`subsystem: imperative summary` — lowercase after the colon, no period.

Known prefixes: `fold:`, `anny:`, `ese:`, `library-core:`,
`library-ingest:`, `library-server:`, `library-app:`, `librarian:`,
`web:`, `search:` (cross-crate search behavior), `perf:`, `workspace:`
(cross-cutting: CI, lints, deps, docs).

The body explains the *why* — motivation, root cause, tradeoffs — not a
list of edits. See `git log` for the house style (e.g. the RCA-style
bodies on the search perf fixes). No Conventional Commits types or
footers. Keep commits single-purpose; formatting-only or mechanical
changes go in their own commit so review diffs stay readable.

## Lint policy

- CI runs `cargo fmt --all --check` and `cargo clippy --all-targets -- -D
  warnings`. Fix warnings, don't `#[allow]` them away without a reason.
- `clippy::unwrap_used` is warn workspace-wide (tests are exempt via
  `clippy.toml`):
  - Library crates (`fold`, `anny`, `ese`, `library-core`): no bare
    `unwrap()` outside tests. Use `expect("context: what invariant
    failed")` for true invariants (poisoned locks, postcard round-trips of
    our own types); propagate errors where the signature allows.
  - App crates: command handlers and worker loops must not panic the
    process — return errors to the caller. Audited, genuinely-infallible
    survivors carry `#[expect(clippy::unwrap_used)]` with a reason.

## Gotchas

- **tauri v2 hook CWD is `apps/`** (the crate dir's parent), not the
  config dir — `beforeDevCommand` is `npm --prefix web run dev` for this
  layout; `frontendDist` resolves from the config dir (`../web/dist`).
- A panicked fjall write tx **poisons the write lock** for the process;
  reads still work, writes need a reopen. Don't chase this as a
  regression.
- Playwright cannot drive the Tauri WKWebView — use the `library-server`
  web UI for interaction coverage; treat `cargo tauri dev` as a boot
  check.
- `ese` is built with different feature sets standalone vs. via the
  workspace dep (`dim-512, quant-8, rayon`) — both share the same model
  cache.
- For representative `ese` bench numbers, use
  `RUSTFLAGS="-Ctarget-cpu=native" cargo bench -p ese`.

## CI

`.github/workflows/ci.yml`, three jobs:

- **rust-linux** — fmt check, clippy, tests for `fold`/`anny`/`ese`/
  `library-core`, ese golden vectors, `cargo check` of the examples.
- **rust-macos** — clippy + tests for the `library-*` app crates
  (creates an empty `apps/web/dist` first for tauri-build).
- **web** — `npm ci`, typecheck, vitest, vite build.

Reproduce any job locally with the commands above. The ese model download
is cached in CI keyed on `ese/build.rs`; the first run after changing that
file re-downloads.
