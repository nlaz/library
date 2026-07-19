# The Library

The Library is an on-device search engine and chat agent for a personal
collection of scanned PDFs — books, catalogs, papers. It ingests your PDFs
(OCR, layout detection, figure extraction), indexes them for hybrid
full-text + semantic search, and answers questions about them with a local
chat agent. Everything runs on your machine: no cloud, no accounts, no data
leaves the device.

It is built on a small set of reusable Rust crates — `fold`, `ese`, and
`anny` (together, "Bog") — that are useful on their own for building fast,
incremental, searchable data stores.

## Requirements

The reusable library crates (`fold`, `ese`, `anny`) are cross-platform Rust.
**The Library app is macOS-only**, because ingestion and chat use Apple
frameworks:

- **macOS 26+** with **Apple Intelligence** enabled (the chat agent uses
  Apple Foundation Models; ingestion uses Vision OCR and PDFKit).
- A **Rust** toolchain for the workspace.
- A **Swift** toolchain for the `librarian` chat sidecar and the
  `clean-pages` helper. The Command Line Tools are sufficient — the code is
  written macro-free so full Xcode is not required (though it works too).

## Repository layout

Reusable crates (cross-platform):

- **`fold`** — an incremental programming framework: iterator-like primitives
  for materializing a stream of ever-changing data into views. Statically
  typed and very fast. It is the engine that powers Bog.
- **`ese`** (Embedded Static Embeddings) — a compiler-oriented static text
  embedding model, flattened together with its tokenizer into a perfect hash
  function and compiled into the crate. Fast, allocation-light text vectors
  with no model runtime.
- **`anny`** (Approximate Nearest Neighbors… yeah) — performance-oriented
  HNSW index for vector search.

The Library app (macOS):

- **`apps/library-core`** — shared types, the fold search graph, hybrid
  lexical + semantic ranking, and the agent-facing tools.
- **`apps/library-ingest`** — the ingestion pipeline (Vision OCR, YOLO layout
  detection, figure extraction, CLIP image embeddings) and a background
  worker + launchd agent.
- **`apps/library-server`** — a WebTransport (QUIC) search server plus HTTP
  static/asset routes; hosts the web UI.
- **`apps/library-app`** — the Tauri desktop app; runs the stores, models,
  and ingest worker in one process.
- **`apps/web`** — the TypeScript/Vite front-end (search, reader, chat).
- **`apps/librarian`** — a Swift sidecar wrapping Apple Foundation Models: an
  agentic chat loop whose tools call back into `library-core`.

## Quick start

Build the workspace and the Swift sidecar:

```bash
cargo build --release
(cd apps/librarian && swift build -c release)
```

Point `--data` at a data directory and drop PDFs or images (png/jpg) into
`data/pdfs/`, then run the desktop app (`cargo tauri dev` in
`apps/library-app`) or the server:

```bash
cargo run -p library-server -- --data /path/to/data --web apps/web/dist
```

The server generates a fresh self-signed identity per boot for
`localhost`/`127.0.0.1` and the web client pins its certificate hash — no
checked-in certs or keys.

## Background ingestion

The Library keeps ingesting while the app is closed. The queue is the
filesystem: any document in `data/pdfs/` whose `data/status/<doc>.json` isn't
terminal is pending, so dropping a file into that folder is enough. A
launchd agent runs `library-ingest worker` at login, every 15 minutes, and
whenever `data/pdfs` changes:

```bash
cargo run -p library-ingest -- install-agent --data /path/to/data
```

The desktop app installs/repairs the same agent on startup. Coordination
is by fjall's single-process store lock: the worker exits immediately when
the app is running (the app's own worker sweeps the same queue), and if the
app launches mid-run the worker stages its prepared records under
`data/staged/` for the app to commit — nothing is recomputed.

Logs land in `data/logs/ingest.log`. Disable with
`launchctl bootout gui/$UID/computer.flower.library.ingest`.

## Examples

Standalone examples of Bog-style databases (cross-platform):

- `starter` — the smallest possible fold database: a persistent count and
  bag, with inserts, reads, and retraction. `cargo run -p starter`
- `timeseries` — weather readings bucketed into hourly and daily aggregates,
  updated incrementally. `cargo run -p timeseries`
- `chat` — a chat backend where fold is the source of truth and every update
  is broadcast to clients over a websocket. `cargo run -p chat`
- `search` — text search three ways over one document stream: BM25 keyword
  search, HNSW semantic search over ese embeddings, and hybrid rank fusion.
  `cargo run -p search`

To scaffold a new example crate wired into the workspace:

```bash
./scripts/new-project.sh [project-name]
cargo run -p [project-name]
```

## Licensing

The Library is licensed under the Apache License, Version 2.0 (see
[`LICENSE`](LICENSE)).

Machine-learning models are **not** vendored in this repository; they are
downloaded by your own build or runtime from their upstream sources, and
their own licenses govern their use and any redistribution — see
[`NOTICE`](NOTICE). In particular, the document-layout model used during
ingestion (`yolov10m-doclaynet`) is **AGPL-3.0**; it is optional (ingestion
falls back to a heuristic when it is absent), but review its terms before
redistributing it or offering it as a network service.

## More about Bog

Bog is a database runtime that makes every attempt to do as much work as
possible as early as possible, to make reads incredibly fast. This means
compiling queries into functions that eagerly update their output as
mutations occur.
