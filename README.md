# BogKit

This repo contains some of the tooling we've been working on for building Bog style databases. We've collected these tools and examples in one cargo workspace, so you can start building immediately. 

The best way to create your project is to run this terminal command in the root of this repo:

```bash
./scripts/new-project.sh [project-name]
``` 

This creates a new binary crate in `examples/[project-name]`, wires it into the workspace, and adds local path dependencies on `fold`, `anny`, and `ese` (you may not necessarily use all of these).

Run your project with:

```bash
cargo run -p [project-name]
```

## In this workspace

### Fold
Fold is our take on an incremental programming framework, it's the engine that powers Bog. It’s a rust crate with iterator like primitives for materializing a stream of ever changing data into views. Statically typed and very, very fast.

### Embedded Static Embeddings (ESE)
ESE, our first take on a compiler oriented approach to static embedding. It’s a flattening of a tokenizer and map of embeddings into a perfect hash function. It’s also evidence that the approach is worth generalizing, and that there is much to be rethought about how embedding runtimes currently function.

### Approximate Nearest Neighbors... yeah (ANNy)
This is a very fast crate for creating HNSWs.

### Examples
In this directory you'll find a few examples that show bog style databases in various use cases.

- `starter` — the smallest possible fold database: a persistent count and bag, with inserts, reads, and retraction. `cargo run -p starter`
- `timeseries` — weather readings bucketed into hourly and daily aggregates, updated incrementally. `cargo run -p timeseries`
- `chat` — a chat backend where fold is the source of truth and every update is broadcast to clients over a websocket. `cargo run -p chat`, then open http://localhost:3000
- `search` — text search three ways over one document stream: BM25 keyword search, HNSW semantic search over ese embeddings, and hybrid rank fusion. A good base for agent memory or document search projects. `cargo run -p search`

## Background ingestion (The Library)

The Library keeps ingesting while the app is closed. The queue is the
filesystem: any PDF in `data/pdfs/` whose `data/status/<doc>.json` isn't
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

## More about Bog
Bog is a database runtime that makes every attempt to do as much work as possible as early as possible, to make reads incredibly fast. This means compiling queries into functions that eagerly update their output as mutations occur.
