//! ANNy — performance-oriented data structures for approximate nearest
//! neighbor (ANN) search.
//!
//! The crate centers on an HNSW (Hierarchical Navigable Small World) index
//! ([`hnsw`]) that is generic over the vector element type and the distance
//! [`metric`]. It is a small building block; higher-level streaming search is
//! built on top of it in the `fold` crate.
//!
//! Construct an [`hnsw::Hnsw`] with a [`metric::Distance`], insert vectors, and
//! query for nearest neighbors — see `src/hnsw.rs` and `benches/hnsw.rs` for a
//! worked example.

pub(crate) mod traits;

pub mod hnsw;

pub mod metric;
