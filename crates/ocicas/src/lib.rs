//! Content-addressable store with content-defined-chunk deduplication over an OCI
//! registry, plus the vk-registry HTTP lock client. Vendored from virtkit and
//! decoupled from its microVM bundle specifics for reuse by `task`.
//!
//! A file set is concatenated into one content stream, cut into content-defined
//! chunks ([`build`]), each chunk zstd-compressed and stored as one OCI blob
//! keyed by digest. A JSON index maps the file list and their concatenated
//! contents to the chunk list. Identical chunks across cache entries share one
//! blob — the registry deduplicates by digest, the chunker makes the digests
//! line up. [`assemble`] restores the set, verifying every chunk.

// Tests index directly and use unwrap/expect on known-good fixtures; these are
// denied in library code but are fine in test scaffolding.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

mod cas;
mod chunker;
mod error;
mod index;
mod lock;
mod store;

pub use cas::{assemble, build, digest_hex};
pub use chunker::{AVG_CHUNK_SIZE, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};
pub use error::{Error, Result};
pub use index::{
    ARTIFACT_TYPE, ChunkRef, ChunkerParams, Compression, FileEntry, INDEX_VERSION, Index,
    MEDIA_TYPE_CHUNK, MEDIA_TYPE_INDEX, unmarshal_index,
};
pub use lock::{Lease, Locker};
pub use store::{PushStats, RemoteOptions, Store};
