# ANNy

**ANNy provides performance-oriented data structures for approximate nearest
neighbor (ANN) search.**

It currently implements HNSW (Hierarchical Navigable Small World) graphs with
pluggable distance metrics, generic over the vector element type. It is a small,
dependency-light building block used by the rest of this workspace (see `fold`
for the streaming search pipeline built on top of it).

## Usage

Add it as a path or git dependency:

```toml
[dependencies]
anny = { path = "../anny" }
```

See `src/hnsw.rs` for the index API and `benches/hnsw.rs` for a worked example.

## License

Licensed under the Apache License, Version 2.0. See the repository-root
`LICENSE`.
