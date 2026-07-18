//! The artifact index: its on-registry schema, deterministic serialization, and
//! the validation applied on read.
//!
//! Format v2: all file contents are concatenated into one continuous stream (in
//! index order — a tar without the header noise) and that stream is chunked, so
//! a tree of many small files becomes ~1 MiB chunks rather than a chunk per
//! file. The index blob is itself zstd-compressed. Any incompatible change to
//! this schema or the chunking/compression parameters bumps [`INDEX_VERSION`].

use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::chunker::{AVG_CHUNK_SIZE, CHUNKER_ALGO, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};
use crate::error::{Error, Result};

/// Bumped on any incompatible change to the index schema or the
/// chunking/compression parameters. Entries of another version are rejected on
/// read (the cache treats them as misses and overwrites them).
pub const INDEX_VERSION: i64 = 2;

/// zstd level. Part of the format only weakly: a different level yields
/// different compressed bytes, which costs deduplication misses against older
/// entries, never correctness.
pub(crate) const ZSTD_LEVEL: i32 = 3;

/// Artifact and blob media types identifying the format on the registry.
pub const ARTIFACT_TYPE: &str = "application/vnd.wallix.cas.v2";
pub const MEDIA_TYPE_INDEX: &str = "application/vnd.wallix.cas.index.v2+zstd";
pub const MEDIA_TYPE_CHUNK: &str = "application/vnd.wallix.cas.chunk.v1+zstd";

/// Bounds the decompressed index. A legitimate index never approaches this; the
/// cap only guards decompression against a zstd bomb.
const MAX_INDEX_SIZE: usize = 1 << 30;

/// References one compressed chunk blob of the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    /// `sha256:<hex>` of the COMPRESSED bytes — the blob digest the registry
    /// stores and verifies (of the RAW bytes under the transparent scheme).
    pub digest: String,
    /// Compressed (blob) size.
    pub size: i64,
    /// Uncompressed size; bounds decompression on restore.
    #[serde(rename = "rawSize")]
    pub raw_size: i64,
}

/// One file of the set. Parent directories are implicit (created 0755 on
/// restore). The file's bytes are the next `size` bytes of the stream — files
/// are concatenated in index order — not referenced here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Slash-separated, relative, without "." or ".." components.
    pub path: String,
    /// Permission bits only.
    pub mode: u32,
    /// File size in bytes (0 for symlinks).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: i64,
    /// Symlink target; a symlink contributes nothing to the stream.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub symlink: String,
}

/// Pins the boundary algorithm; entries with unknown params are rejected
/// (re-chunking them would produce different digests).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerParams {
    pub algo: String,
    pub min: i64,
    pub avg: i64,
    pub max: i64,
}

/// Pins the per-chunk compression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compression {
    pub algo: String,
    pub level: i64,
}

/// The artifact's table of contents: the ordered file list and the ordered
/// chunk list of their concatenated contents. Files are sorted by path, so the
/// serialization is deterministic for a given tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub version: i64,
    pub chunker: ChunkerParams,
    pub compression: Compression,
    /// The vk-registry transparent-zstd scheme: each chunk's `digest` is over
    /// its UNCOMPRESSED bytes and the blob was uploaded as a zstd frame, so the
    /// registry serves canonical (raw) bytes and restore skips decompression.
    /// Absent (false) = the classic compressed-digest scheme any OCI registry
    /// stores as-is.
    #[serde(default, skip_serializing_if = "is_false")]
    pub transparent: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkRef>,
    pub files: Vec<FileEntry>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Index {
    /// A fresh index carrying the current format parameters.
    pub(crate) fn new() -> Self {
        Index {
            version: INDEX_VERSION,
            chunker: ChunkerParams {
                algo: CHUNKER_ALGO.to_string(),
                min: MIN_CHUNK_SIZE as i64,
                avg: AVG_CHUNK_SIZE as i64,
                max: MAX_CHUNK_SIZE as i64,
            },
            compression: Compression {
                algo: "zstd".to_string(),
                level: ZSTD_LEVEL as i64,
            },
            transparent: false,
            chunks: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Total length of the concatenated contents (symlinks contribute nothing).
    fn stream_size(&self) -> i64 {
        self.files
            .iter()
            .filter(|f| f.symlink.is_empty())
            .map(|f| f.size)
            .sum()
    }

    /// Render the index as zstd-compressed deterministic JSON.
    pub fn marshal(&self) -> Result<Vec<u8>> {
        let raw = serde_json::to_vec(self)?;
        Ok(zstd::bulk::compress(&raw, ZSTD_LEVEL)?)
    }
}

/// Parse and validate an index, rejecting unknown versions, chunking
/// parameters, unsafe paths, and a chunk list inconsistent with the file sizes.
pub fn unmarshal_index(data: &[u8]) -> Result<Index> {
    let raw = zstd_decode_bounded(data, MAX_INDEX_SIZE)?;
    let idx: Index = serde_json::from_slice(&raw)?;
    if idx.version != INDEX_VERSION {
        return Err(Error::format(format!(
            "unsupported index version {}",
            idx.version
        )));
    }
    let want = Index::new();
    if idx.chunker != want.chunker {
        return Err(Error::format(format!(
            "unsupported chunker params {:?}",
            idx.chunker
        )));
    }
    if idx.compression.algo != "zstd" {
        return Err(Error::format(format!(
            "unsupported compression {:?}",
            idx.compression.algo
        )));
    }
    let mut chunked: i64 = 0;
    for c in &idx.chunks {
        if c.raw_size < 0 || c.size < 0 {
            return Err(Error::format("negative chunk size"));
        }
        // The chunker never emits more: a larger raw size is a forged index
        // trying to force a huge allocation on restore.
        if c.raw_size > MAX_CHUNK_SIZE as i64 {
            return Err(Error::format(format!(
                "chunk {}: raw size {} exceeds max {}",
                c.digest, c.raw_size, MAX_CHUNK_SIZE
            )));
        }
        chunked = chunked.saturating_add(c.raw_size);
    }
    let need = idx.stream_size();
    if chunked != need {
        return Err(Error::format(format!(
            "chunks cover {chunked} bytes, files need {need}"
        )));
    }
    for f in &idx.files {
        if !safe_rel_path(&f.path) {
            return Err(Error::format(format!("unsafe path {:?} in index", f.path)));
        }
        if f.size < 0 {
            return Err(Error::format(format!("{}: negative size", f.path)));
        }
        // Mode carries permission bits only; setuid/setgid/type bits are a
        // forged index.
        if f.mode & !0o777 != 0 {
            return Err(Error::format(format!(
                "{}: invalid mode {:o}",
                f.path, f.mode
            )));
        }
    }
    Ok(idx)
}

/// Accepts slash-separated relative paths without empty, "." or ".."
/// components — the zip-slip guard of the format.
pub(crate) fn safe_rel_path(p: &str) -> bool {
    if p.is_empty() || p.starts_with('/') {
        return false;
    }
    !p.split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
}

/// Decode a zstd frame, capping the output at `max` bytes so a bomb fails
/// instead of exhausting memory.
pub(crate) fn zstd_decode_bounded(data: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut dec = zstd::stream::read::Decoder::new(data)?;
    let mut out = Vec::new();
    let limit = (max as u64).saturating_add(1);
    dec.by_ref().take(limit).read_to_end(&mut out)?;
    if out.len() > max {
        return Err(Error::format(format!(
            "decompressed data exceeds {max} bytes"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(digest: &str, size: i64, raw: i64) -> ChunkRef {
        ChunkRef {
            digest: digest.to_string(),
            size,
            raw_size: raw,
        }
    }

    #[test]
    fn round_trips_through_marshal() {
        let mut idx = Index::new();
        idx.chunks.push(chunk("sha256:aa", 10, 20));
        idx.files.push(FileEntry {
            path: "a/b.txt".to_string(),
            mode: 0o644,
            size: 20,
            symlink: String::new(),
        });
        let blob = idx.marshal().expect("marshal");
        let back = unmarshal_index(&blob).expect("unmarshal");
        assert_eq!(back.files, idx.files);
        assert_eq!(back.chunks, idx.chunks);
        assert_eq!(back.version, INDEX_VERSION);
    }

    #[test]
    fn rejects_unknown_version() {
        let mut idx = Index::new();
        idx.version = 1;
        let blob = idx.marshal().expect("marshal");
        assert!(unmarshal_index(&blob).is_err());
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(!safe_rel_path(""));
        assert!(!safe_rel_path("/etc/passwd"));
        assert!(!safe_rel_path("a/../b"));
        assert!(!safe_rel_path("./a"));
        assert!(!safe_rel_path("a//b"));
        assert!(safe_rel_path("a/b/c.txt"));
        assert!(safe_rel_path("file"));
    }

    #[test]
    fn rejects_chunks_not_covering_files() {
        let mut idx = Index::new();
        idx.chunks.push(chunk("sha256:aa", 5, 10));
        idx.files.push(FileEntry {
            path: "f".to_string(),
            mode: 0o644,
            size: 20, // needs 20, chunks cover 10
            symlink: String::new(),
        });
        let blob = idx.marshal().expect("marshal");
        assert!(unmarshal_index(&blob).is_err());
    }

    #[test]
    fn rejects_oversized_raw_chunk() {
        let mut idx = Index::new();
        idx.chunks
            .push(chunk("sha256:aa", 5, MAX_CHUNK_SIZE as i64 + 1));
        let blob = idx.marshal().expect("marshal");
        assert!(unmarshal_index(&blob).is_err());
    }

    #[test]
    fn rejects_non_permission_mode_bits() {
        let mut idx = Index::new();
        idx.files.push(FileEntry {
            path: "f".to_string(),
            mode: 0o4755, // setuid bit set
            size: 0,
            symlink: String::new(),
        });
        let blob = idx.marshal().expect("marshal");
        assert!(unmarshal_index(&blob).is_err());
    }

    #[test]
    fn bounded_decode_rejects_oversize() {
        let big = vec![0u8; 4096];
        let frame = zstd::bulk::compress(&big, 3).expect("compress");
        assert!(zstd_decode_bounded(&frame, 1024).is_err());
        assert!(zstd_decode_bounded(&frame, 4096).is_ok());
    }
}
