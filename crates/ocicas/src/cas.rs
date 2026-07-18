//! Building and restoring a content-addressed file set.
//!
//! [`build`] concatenates the given files into one content stream, cuts it into
//! content-defined chunks, zstd-compresses each, and emits it through a sink —
//! duplicate chunks within the stream are emitted once. [`assemble`] fetches the
//! stream chunks through a source, verifying every chunk's digest and
//! decompressed size before any byte lands on disk.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::chunker::{AVG_CHUNK_SIZE, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};
use crate::error::{Error, Result};
use crate::index::{ChunkRef, FileEntry, Index, ZSTD_LEVEL, safe_rel_path, zstd_decode_bounded};

/// Concatenate the given files (slash-separated paths relative to `base_dir`,
/// duplicates ignored) into the content stream, chunk it, and emit each
/// compressed chunk through `sink`. The returned index lists files sorted by
/// path, so the stream and its serialization are deterministic for a given
/// tree. When `transparent`, chunks are keyed by their uncompressed digest (the
/// vk-registry transparent-zstd scheme); otherwise by the compressed digest.
pub fn build<S>(base_dir: &Path, paths: &[String], mut sink: S, transparent: bool) -> Result<Index>
where
    S: FnMut(&ChunkRef, &[u8]) -> Result<()>,
{
    let mut idx = Index::new();
    idx.transparent = transparent;

    let mut uniq: Vec<String> = paths.to_vec();
    uniq.sort();
    uniq.dedup();

    // Regular files feed the content stream; `reg_entry[i]` is the index of the
    // i-th streamed file in `idx.files`, so its streamed size can be filled in
    // after chunking (the streamed length wins over lstat — the file may still
    // be changing).
    let mut reg_paths: Vec<PathBuf> = Vec::new();
    let mut reg_entry: Vec<usize> = Vec::new();
    for p in &uniq {
        if !safe_rel_path(p) {
            return Err(Error::format(format!("unsafe path {p:?}")));
        }
        let full = base_dir.join(from_slash(p));
        let meta = std::fs::symlink_metadata(&full)?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = std::fs::read_link(&full)?;
            idx.files.push(FileEntry {
                path: p.clone(),
                mode: perm_bits(&meta),
                size: 0,
                symlink: path_to_string(&target),
            });
        } else if ft.is_file() {
            reg_entry.push(idx.files.len());
            reg_paths.push(full);
            idx.files.push(FileEntry {
                path: p.clone(),
                mode: perm_bits(&meta),
                size: 0,
                symlink: String::new(),
            });
        } else {
            return Err(Error::format(format!(
                "{p:?} is neither a regular file nor a symlink"
            )));
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut concat = ConcatFiles::new(reg_paths);
    let chunker =
        fastcdc::v2020::StreamCDC::new(&mut concat, MIN_CHUNK_SIZE, AVG_CHUNK_SIZE, MAX_CHUNK_SIZE);
    for chunk in chunker {
        let chunk = chunk.map_err(|e| Error::format(format!("chunking: {e}")))?;
        let raw = &chunk.data;
        let frame = zstd::bulk::compress(raw, ZSTD_LEVEL)?;
        let refc = if transparent {
            let d = sha256_hex(raw);
            ChunkRef {
                digest: d,
                size: raw.len() as i64,
                raw_size: raw.len() as i64,
            }
        } else {
            let d = sha256_hex(&frame);
            ChunkRef {
                digest: d,
                size: frame.len() as i64,
                raw_size: raw.len() as i64,
            }
        };
        idx.chunks.push(refc.clone());
        if seen.insert(refc.digest.clone()) {
            sink(&refc, &frame)?;
        }
    }

    let sizes = concat.into_sizes();
    for (i, &entry_idx) in reg_entry.iter().enumerate() {
        if let (Some(&sz), Some(entry)) = (sizes.get(i), idx.files.get_mut(entry_idx)) {
            entry.size = sz;
        }
    }
    Ok(idx)
}

/// A `Read` over the concatenation of `paths`, opening one file at a time and
/// recording how many bytes each contributed. Only one file handle is open at
/// once, so a large file set does not exhaust the fd limit.
struct ConcatFiles {
    paths: Vec<PathBuf>,
    sizes: Vec<i64>,
    index: usize,
    cur: Option<File>,
}

impl ConcatFiles {
    fn new(paths: Vec<PathBuf>) -> Self {
        let sizes = vec![0; paths.len()];
        ConcatFiles {
            paths,
            sizes,
            index: 0,
            cur: None,
        }
    }

    fn into_sizes(self) -> Vec<i64> {
        self.sizes
    }
}

impl Read for ConcatFiles {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.cur.is_none() {
                match self.paths.get(self.index) {
                    None => return Ok(0),
                    Some(p) => self.cur = Some(File::open(p)?),
                }
            }
            let Some(f) = self.cur.as_mut() else {
                return Ok(0);
            };
            let n = f.read(buf)?;
            if n == 0 {
                self.cur = None;
                self.index = self.index.saturating_add(1);
                continue;
            }
            if let Some(sz) = self.sizes.get_mut(self.index) {
                *sz = sz.saturating_add(n as i64);
            }
            return Ok(n);
        }
    }
}

/// Restore the file set under `base_dir`, fetching the stream chunks through
/// `src`. Every chunk is digest-verified and its decompressed size checked, so a
/// corrupt or substituted blob fails the restore instead of landing on disk.
/// Existing files are overwritten.
pub fn assemble<Src>(idx: &Index, base_dir: &Path, src: Src) -> Result<()>
where
    Src: FnMut(&str) -> Result<Vec<u8>>,
{
    let mut stream = StreamReader::new(idx, src);
    for entry in &idx.files {
        if !safe_rel_path(&entry.path) {
            return Err(Error::format(format!(
                "unsafe path {:?} in index",
                entry.path
            )));
        }
        // A symlink entry can point outside the root; a later entry written
        // under it would escape. Reject any write whose parent chain crosses a
        // symlink.
        check_no_symlink_parents(base_dir, &entry.path)?;
        let full = base_dir.join(from_slash(&entry.path));
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !entry.symlink.is_empty() {
            remove_path(&full)?;
            make_symlink(&entry.symlink, &full)?;
            continue;
        }
        assemble_file(entry, &full, &mut stream)?;
    }
    Ok(())
}

/// Walks the decompressed stream chunk by chunk; consumers read exactly the
/// file sizes of the index, in index order.
struct StreamReader<'a, Src> {
    idx: &'a Index,
    src: Src,
    next: usize,
    raw: Vec<u8>,
}

impl<'a, Src> StreamReader<'a, Src>
where
    Src: FnMut(&str) -> Result<Vec<u8>>,
{
    fn new(idx: &'a Index, src: Src) -> Self {
        StreamReader {
            idx,
            src,
            next: 0,
            raw: Vec::new(),
        }
    }

    fn read(&mut self, n: usize) -> Result<Vec<u8>> {
        let idx = self.idx;
        while self.raw.len() < n {
            let Some(refc) = idx.chunks.get(self.next) else {
                return Err(Error::format("stream exhausted (index inconsistent)"));
            };
            self.next = self.next.saturating_add(1);
            let data = (self.src)(&refc.digest)?;
            // The digest covers whatever `src` returns: the raw bytes under the
            // transparent scheme (the registry serves canonical), the zstd frame
            // otherwise.
            let got = sha256_hex(&data);
            if got != refc.digest {
                return Err(Error::format(format!(
                    "chunk digest mismatch: want {}, got {got}",
                    refc.digest
                )));
            }
            let raw = if idx.transparent {
                data
            } else {
                zstd_decode_bounded(&data, refc.raw_size as usize)?
            };
            if raw.len() as i64 != refc.raw_size {
                return Err(Error::format(format!(
                    "chunk {}: raw size {}, index says {}",
                    refc.digest,
                    raw.len(),
                    refc.raw_size
                )));
            }
            self.raw.extend_from_slice(&raw);
        }
        Ok(self.raw.drain(..n).collect())
    }
}

fn assemble_file<Src>(
    entry: &FileEntry,
    full: &Path,
    stream: &mut StreamReader<'_, Src>,
) -> Result<()>
where
    Src: FnMut(&str) -> Result<Vec<u8>>,
{
    // O_TRUNC on a stale symlink would write through it into the link target:
    // replace anything that is not a regular file.
    if let Ok(meta) = std::fs::symlink_metadata(full)
        && !meta.file_type().is_file()
    {
        remove_path(full)?;
    }
    let mut f = open_write(full, entry.mode)?;
    // The file may pre-exist with other permissions: enforce the index's.
    set_perm(&f, entry.mode)?;

    let mut remaining = entry.size;
    while remaining > 0 {
        let want = remaining.min(MAX_CHUNK_SIZE as i64);
        let data = stream.read(want as usize)?;
        f.write_all(&data)?;
        remaining = remaining.saturating_sub(data.len() as i64);
    }
    Ok(())
}

/// Reject a path any of whose existing parent directories is a symlink: writing
/// under it would follow the link out of `base_dir`. The leaf is not checked —
/// it is the entry being (re)created.
fn check_no_symlink_parents(base_dir: &Path, slash_path: &str) -> Result<()> {
    let parts: Vec<&str> = slash_path.split('/').collect();
    let n = parts.len();
    let mut dir = base_dir.to_path_buf();
    for part in parts.iter().take(n.saturating_sub(1)) {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(Error::format(format!(
                        "{slash_path:?}: parent {part:?} is a symlink"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Return the bare hex of a `sha256:<hex>` digest — the layout helper for
/// content-addressed stores (`.../sha256/<hex>`).
pub fn digest_hex(digest: &str) -> Option<&str> {
    digest.strip_prefix("sha256:").filter(|hex| hex.len() == 64)
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(71);
    s.push_str("sha256:");
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(windows)]
fn from_slash(p: &str) -> PathBuf {
    PathBuf::from(p.replace('/', "\\"))
}

#[cfg(not(windows))]
fn from_slash(p: &str) -> PathBuf {
    PathBuf::from(p)
}

fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn perm_bits(m: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn perm_bits(m: &Metadata) -> u32 {
    if m.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

fn open_write(path: &Path, mode: u32) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let _ = mode;
    Ok(opts.open(path)?)
}

#[cfg(unix)]
fn set_perm(f: &File, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_perm(_f: &File, _mode: u32) -> Result<()> {
    Ok(())
}

fn remove_path(p: &Path) -> Result<()> {
    match std::fs::symlink_metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            if meta.file_type().is_dir() {
                std::fs::remove_dir_all(p)?;
            } else {
                std::fs::remove_file(p)?;
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn make_symlink(target: &str, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ocicas-test-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(base: &Path, rel: &str, data: &[u8]) {
        let full = base.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, data).unwrap();
    }

    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut s = seed;
        while out.len() < len {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            out.extend_from_slice(&z.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn mem_build(
        base: &Path,
        paths: &[String],
        transparent: bool,
    ) -> (Index, HashMap<String, Vec<u8>>) {
        let mut store: HashMap<String, Vec<u8>> = HashMap::new();
        let idx = build(
            base,
            paths,
            |r, frame| {
                store.insert(r.digest.clone(), frame.to_vec());
                Ok(())
            },
            transparent,
        )
        .unwrap();
        (idx, store)
    }

    fn mem_source(store: &HashMap<String, Vec<u8>>) -> impl FnMut(&str) -> Result<Vec<u8>> + '_ {
        move |d: &str| {
            store
                .get(d)
                .cloned()
                .ok_or_else(|| Error::format(format!("missing chunk {d}")))
        }
    }

    #[test]
    fn round_trips_a_small_tree() {
        let src = tmpdir("src");
        write_file(&src, "a.txt", b"hello world");
        write_file(&src, "sub/b.bin", &pseudo_random(700_000, 7));
        let paths = vec!["sub/b.bin".to_string(), "a.txt".to_string()];

        let (idx, store) = mem_build(&src, &paths, false);
        // Files sorted by path in the index.
        assert_eq!(
            idx.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "sub/b.bin"]
        );

        let dst = tmpdir("dst");
        assemble(&idx, &dst, mem_source(&store)).unwrap();

        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello world");
        assert_eq!(
            std::fs::read(src.join("sub/b.bin")).unwrap(),
            std::fs::read(dst.join("sub/b.bin")).unwrap()
        );
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
    }

    #[test]
    fn build_is_deterministic() {
        // The same tree must produce the same chunk digests, which is what lets
        // two pushes of the same content dedup against each other registry-side.
        let src = tmpdir("src");
        write_file(&src, "x", &pseudo_random(2_000_000, 11));
        write_file(&src, "y", b"small");
        let paths = vec!["x".to_string(), "y".to_string()];

        let (idx1, _) = mem_build(&src, &paths, false);
        let (idx2, _) = mem_build(&src, &paths, false);
        let d1: Vec<&str> = idx1.chunks.iter().map(|c| c.digest.as_str()).collect();
        let d2: Vec<&str> = idx2.chunks.iter().map(|c| c.digest.as_str()).collect();
        assert_eq!(d1, d2);
        assert!(
            idx1.chunks.len() > 1,
            "a 2 MB stream should cut into multiple chunks"
        );
        std::fs::remove_dir_all(&src).ok();
    }

    #[test]
    fn transparent_scheme_round_trips() {
        let src = tmpdir("src");
        write_file(&src, "f", &pseudo_random(500_000, 3));
        let paths = vec!["f".to_string()];

        // Emulate the transparent registry: it stores the uploaded zstd frame
        // but serves canonical (raw) bytes back, so decompress on fetch. The
        // digest is over the raw bytes.
        let mut store: HashMap<String, Vec<u8>> = HashMap::new();
        let idx = build(
            &src,
            &paths,
            |r, frame| {
                let raw = zstd::bulk::decompress(frame, MAX_CHUNK_SIZE).unwrap();
                store.insert(r.digest.clone(), raw);
                Ok(())
            },
            true,
        )
        .unwrap();
        assert!(idx.transparent);

        let dst = tmpdir("dst");
        assemble(&idx, &dst, mem_source(&store)).unwrap();
        assert_eq!(
            std::fs::read(src.join("f")).unwrap(),
            std::fs::read(dst.join("f")).unwrap()
        );
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
    }

    #[test]
    fn detects_corrupted_chunk() {
        let src = tmpdir("src");
        write_file(&src, "f", &pseudo_random(400_000, 5));
        let paths = vec!["f".to_string()];
        let (idx, mut store) = mem_build(&src, &paths, false);
        // Corrupt one stored blob.
        if let Some(first) = idx.chunks.first() {
            store.insert(first.digest.clone(), b"garbage".to_vec());
        }
        let dst = tmpdir("dst");
        assert!(assemble(&idx, &dst, mem_source(&store)).is_err());
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
    }

    #[cfg(unix)]
    #[test]
    fn round_trips_a_symlink() {
        let src = tmpdir("src");
        write_file(&src, "target.txt", b"data");
        std::os::unix::fs::symlink("target.txt", src.join("link")).unwrap();
        let paths = vec!["target.txt".to_string(), "link".to_string()];
        let (idx, store) = mem_build(&src, &paths, false);
        let dst = tmpdir("dst");
        assemble(&idx, &dst, mem_source(&store)).unwrap();
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            Path::new("target.txt")
        );
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
    }
}
