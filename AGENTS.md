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
  `objc2-*`) + the `worker` CLI that drains the queue when the app isn't
  running. macOS-only to *compile*, but its unit tests exercise pure logic.
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

## Where the data lives

Two halves, and the split is load-bearing:

- **The user's folders** — watched *roots*. The default is `~/The Library`
  (created on first launch); users link their own folders in Settings (⌘S).
  Files are never moved, renamed or reorganized: the filesystem is the
  truth and the index is a reflection of it. A depth-1 folder is a shelf.
- **The app-support dir** (`~/Library/Application Support/dev.thelibrary/`,
  or the repo's `data/` in dev builds) — everything derived or private:
  `meta.db`, the two fjall stores, page renders, OCR, markdown, models, and
  `run/` (cross-process locks only).

`meta.db` is SQLite in WAL mode and holds all library metadata: roots,
files, docs (incl. ingest status), collections, cards, settings. It is the
one place three processes can write concurrently. Legacy
`data/annotations/*.json` are **read-only migration input** (see
`annots.rs`), not a live table.

**`data/pages` is a bounded cache, not storage.** Page renders are ~5× the
size of the sources they come from and cost ~160ms each to remake, so
`cache.rs` sweeps them to a budget (pref `cache.pages.budget_bytes`) and
`serve.rs` re-renders on a miss. Three consequences worth knowing before
touching anything nearby:

- Never derive a document's *existence* or *page count* from `data/pages`
  — use `meta.db` and `wire::count_pages` (which counts OCR sidecars). A
  swept book must not vanish from the library or open to an empty reader.
- `data/ocr`, `data/text` and `<doc>/cover.jpg` are the floor and are
  never evicted; the OCR in particular is what makes a re-render cheap.
- **A render is only cache if it can be made again.** `source_state`
  decides, and the sweep pins anything it cannot re-render — a document
  whose file is gone has renders that are the *only* copy of it. That
  guard is load-bearing; keep it and its tests.

## Data safety

- `data/` at the repo root is a **live personal library** (databases, PDFs,
  OCR output). Never write to it from tests, experiments, or scripts; never
  delete or "clean it up".
- Tests must be hermetic: temp dirs only, no network, no `data/`. See the
  fixture patterns below.
- fjall stores are **single-process** — a second opener panics with
  `Locked`. Stop `library-server` before running the CLI search or the app.
  SQLite (`meta.db`) is *not* subject to this: that is why metadata lives
  there.
- **A disappearance is not a deletion.** `roots::reconcile` refuses to
  retract when a root is unreadable (unmounted volume), when >20% of a
  root's documents vanish at once, or when a file is an iCloud stub
  (`SF_DATALESS`). If you touch that code, keep the guards and their tests
  — losing a library is the one unrecoverable bug this app has.

## Testing conventions

- Unit tests live in inline `#[cfg(test)] mod tests` next to the code they
  cover (crate-private access is the point). `fold` collects its tests
  under `fold/src/tests/`; integration tests use `tests/` dirs.
- Fixture patterns to reuse rather than reinvent:
  - `fold/src/tests/mod.rs::fresh_db` — temp-dir fjall store per test.
  - `apps/library-core/tests/roots_sync.rs::Lib` — a real temp folder plus
    a real (in-memory) `meta.db`, for anything touching the scanner.
  - `library_core::meta::Ctx::in_memory(dir)` — a `Ctx` over a real cache
    directory with an in-memory database. The default for new tests.
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
- The bundle identifier is **`dev.thelibrary`**, which also fixes the
  app-support dir (`~/Library/Application Support/dev.thelibrary/`). Releases
  up to 0.1.1 used `computer.flower.library` and installed a launchd agent
  (`computer.flower.library.ingest`) for background ingest. That agent is
  gone — indexing happens only while the app runs — and
  `ingest.rs::uninstall_legacy_agent` boots out the orphan on launch, since
  launchd would otherwise keep waking it every 15 minutes forever. Don't
  delete that cleanup until well past the point where 0.1.x installs are
  plausible.
- **The updater's signing key is unrecoverable.** `plugins.updater.pubkey`
  in `tauri.conf.json` is compiled into every copy that ships; an update
  is only installed if the `.app.tar.gz` carries a minisign signature made
  by the matching private key (`~/.tauri/thelibrary.key`, password in the
  keychain). Lose it and no installed copy can ever be updated again —
  every user would have to download a new build by hand. Rotating it only
  helps copies installed *after* the rotation. Note this is Tauri's
  signature, entirely separate from Apple's codesigning identity;
  `release.sh` applies both, for different checks.
- Verify the update tarball before publishing — `scripts/verify-update.sh`.
  The updater unpacks it over a running bundle and Gatekeeper judges the
  result on next launch, so a signature or notarization ticket lost in
  `tar` produces an app macOS refuses to open and no way back but a manual
  download.
- `tauri.conf.json`'s `minimumSystemVersion` is not just `Info.plist`:
  `cargo tauri build` also exports it as `MACOSX_DEPLOYMENT_TARGET`, so it
  sets the `library-app` binary's `LC_BUILD_VERSION minos` too (check with
  `vtool -show-build`). It is **14.0** — the Apple calls in ingest are
  10.15-era and ORT links statically at 11.0, so the floor is a tested
  claim, not a technical limit. Only the two Swift binaries (`librarian`,
  `tools/clean-pages`) need 26, for FoundationModels, and both are optional
  at runtime; `chat.rs::probe_chat` refuses below `CHAT_MIN_MACOS` so the
  chat surface is hidden rather than failing on first use.
