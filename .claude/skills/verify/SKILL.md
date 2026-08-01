---
name: verify
description: Build, run, and drive The Library (workspace: fold/anny/ese libs + library-* app crates) to verify changes end-to-end.
---

# Verifying changes in this workspace

## Build & launch

```sh
cargo build --release -p library-ingest -p library-server   # fat LTO: minutes
./target/release/library-ingest search "some phrase"        # CLI surface (opens data/library.db)
./target/release/library-ingest search --lex-only "phrase"  # skips ese embed
./target/release/library-server                              # http://127.0.0.1:8080 + WebTransport :4433
```

Run everything from the repo root — `data/` and `tools/clean-pages/clean-pages`
resolve relative to CWD. `data/` is the *app-support* dir (caches, meta.db,
stores); the books live in watched folders and, in dev builds, the default
one is `data/Library`.

## Surfaces

- **CLI**: `library-ingest ingest <file> --hot` copies the file into the
  default watched folder and lets the scanner mint the document (OCR/pages
  are cached per doc; re-ingest = chunk + ese embed + CLIP figures).
  `search`, `checkpoint` for quick probes. `migrate --from <old-data-dir>
  --to <folder> --dry-run` reports what a pre-0.2 library would become
  without writing anything.
- **Server UI**: start library-server, drive http://127.0.0.1:8080 with the
  Playwright browser. The search box streams over WebTransport (status chip
  shows "ready", then "N hits · phase · ms"). Collection buttons exercise the
  filtered (`search_filtered`) BM25/HNSW paths; clicking a hit opens the
  reader with word-bbox highlights on the page scan.
- **Desktop app**: `cd apps/library-app && cargo tauri dev` (window appears;
  stdout prints "stores open in …" and "embedding model ready in …" when the
  engine is up). Playwright cannot drive the WKWebView — use the server UI for
  interaction coverage and treat tauri dev as a boot check.
- **Watched folders**: the parts worth driving by hand are the ones no test
  covers end to end — drop a *folder* on the window, rename a book in
  Finder and watch it stay one document, eject a linked volume and confirm
  the shelf survives, and open Settings with ⌘S. Point a run at a scratch
  library with `LIBRARY_DATA=/tmp/lib-scratch cargo tauri dev`; the app
  creates `<that dir>/Library` and watches it, so the real `data/` is never
  involved.

## Gotchas

See the **Gotchas** and **Data safety** sections of the repo-root
[`AGENTS.md`](../../../AGENTS.md) — fjall single-process locking, tauri hook
CWD, ese model cache, poisoned write locks.

Skill-specific: Playwright screenshots can only be saved under its own temp
root — take them unnamed and Read the returned path.
