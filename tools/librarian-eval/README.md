# librarian-eval

A dev-only eval harness for the librarian chat agent. It runs four suites:

- `retrieval` — a query battery against `/api/search`, printed as markdown and optionally dumped to JSON.
- `probe` — capability probes (fixtures in `fixtures/*.json`) driven through the built Swift `librarian` sidecar.
- `regress` — hard assertions on the agent-tool behaviors the chat experience depends on (confidence tiers, collection scoping, sample determinism, dedup, legibility, no raw ids/newlines).
- `e2e` — canned questions through `POST /api/chat`, asserting the event shape (tool activity, then a grounded `done`).

## Requirements

- A running `library-server` on `http://127.0.0.1:8080`.
- The built Swift `librarian` sidecar (`probe` only). Override its path with `LIBRARIAN_BIN`; defaults to `apps/librarian/.build/release/librarian`.
- Apple Intelligence available on the host (for the probe/e2e suites).

## Usage

Run from the repo root:

```
cargo run -p librarian-eval -- retrieval|probe|regress|e2e
```

`retrieval` takes an optional output path; `probe` takes an optional id filter.

## Important: fixtures are illustrative placeholders

The committed fixtures and queries are ILLUSTRATIVE placeholders keyed to a
sample corpus vocabulary — collections `recipes` / `field-guides`, and sample
doc ids like `the-art-of-plain-cookery`, `journal-1891`, `catalog00vol`,
`1985-community-directory`. They will NOT pass against your library as-is.
Adapt the collection names, doc ids, and query terms in `src/main.rs` and
`fixtures/*.json` to your own ingested corpus's real ids and collections
before the assertions will hold.
