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
resolve relative to CWD.

## Surfaces

- **CLI**: `library-ingest ingest data/pdfs/<doc>.pdf --hot` (OCR/pages are
  cached per doc; re-ingest = chunk + ese embed + CLIP figures). `search`,
  `checkpoint`, `collect` for quick probes.
- **Server UI**: start library-server, drive http://127.0.0.1:8080 with the
  Playwright browser. The search box streams over WebTransport (status chip
  shows "ready", then "N hits · phase · ms"). Collection buttons exercise the
  filtered (`search_filtered`) BM25/HNSW paths; clicking a hit opens the
  reader with word-bbox highlights on the page scan.
- **Desktop app**: `cd apps/library-app && cargo tauri dev` (window appears;
  stdout prints "stores open in …" and "embedding model ready in …" when the
  engine is up). Playwright cannot drive the WKWebView — use the server UI for
  interaction coverage and treat tauri dev as a boot check.

## Gotchas

- **fjall stores are single-process.** Stop library-server before running the
  CLI search or the app, or the second opener panics with `Locked`.
- **tauri v2 hook CWD is `apps/` (the crate dir's parent), not the config
  dir** — `beforeDevCommand` is `npm --prefix web run dev` for this layout.
  `frontendDist` resolves from the config dir instead (`../web/dist`).
- First workspace build downloads the ese model into `target/ese-cache/`
  (network once; wiped by `cargo clean`).
- Playwright screenshots can only be saved under its own temp root — take
  them unnamed and Read the returned path.
- A panicked write tx poisons the fjall write lock for the process; reads
  still work, writes need a reopen. Don't chase this as a regression.
