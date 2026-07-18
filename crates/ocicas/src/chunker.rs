//! Content-defined chunking parameters, algorithm `fastcdc-v2020`.
//!
//! The chunk boundaries are part of the wire format: they determine how the
//! content stream is cut, hence which blobs dedup against each other. The
//! `fastcdc` crate's v2020 splitter does the cutting (see `cas`); this module
//! only pins the parameters and their recorded identity. Changing any of them
//! breaks deduplication against previously pushed entries — bump the algorithm
//! version instead.

/// FastCDC size parameters: a 1 MiB average keeps the per-entry descriptor
/// count reasonable while still deduplicating partial changes inside large
/// files.
pub const MIN_CHUNK_SIZE: usize = 256 << 10;
pub const AVG_CHUNK_SIZE: usize = 1 << 20;
pub const MAX_CHUNK_SIZE: usize = 4 << 20;

/// Recorded in the index's `chunker.algo`; an entry with a different value is
/// rejected on read, since re-chunking it would produce different digests.
pub(crate) const CHUNKER_ALGO: &str = "fastcdc-v2020";
