//! Building and restoring a content-addressed file set.
//!
//! [`build`] concatenates the given files into one content stream, cuts it into
//! content-defined chunks, zstd-compresses each, and emits it through a sink —
//! duplicate chunks within the stream are emitted once. [`assemble`] fetches the
//! stream chunks through a source, verifying every chunk's digest and
//! decompressed size before any byte lands on disk.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::chunker::{AVG_CHUNK_SIZE, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};
use crate::error::{Error, Result};
use crate::index::{ChunkRef, FileEntry, Index, ZSTD_LEVEL, safe_rel_path, zstd_decode_bounded};
use crate::restore::{self, Tree, Written};

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
///
/// The set is untrusted: every path is checked before anything is written (see
/// [`crate::restore`]), and every entry is written confined to `base_dir`, an
/// existing file or symlink replaced by a fresh one, never written through.
/// It applies no policy on *which* paths may be written: a caller restoring
/// into a project vets [`Index::files`] first (as `task` does through
/// [`crate::Store::pull_index`]).
///
/// Returns what was written, for the caller to commit or undo; a failure
/// partway undoes it itself. With `journal_dir`, a journal there lets
/// [`crate::restore::recover`] clean up a restore cut short.
pub fn assemble<Src>(
    idx: &Index,
    base_dir: &Path,
    journal_dir: Option<&Path>,
    src: Src,
) -> Result<Written>
where
    Src: FnMut(&str) -> Result<Vec<u8>>,
{
    let parts = idx
        .files
        .iter()
        .map(|f| restore::components(&f.path, false))
        .collect::<Result<Vec<_>>>()?;
    restore::check_unique(parts.iter().map(Vec::as_slice))?;
    restore::check_link_parents(
        parts
            .iter()
            .zip(&idx.files)
            .map(|(p, f)| (p.as_slice(), !f.symlink.is_empty())),
    )?;
    if !cfg!(unix)
        && let Some(link) = idx.files.iter().find(|f| !f.symlink.is_empty())
    {
        return Err(Error::format(format!(
            "entry {:?}: symlinks are not restored on this platform",
            link.path
        )));
    }
    let mut tree = Tree::open(base_dir)?;
    if let Some(dir) = journal_dir {
        tree.journal_in(dir)?;
    }
    // A failure partway takes out what was written, so a restore leaves all
    // or nothing behind — unless that undo fails too, which is reported as
    // such rather than as a miss.
    match write_files(idx, &parts, &mut tree, src) {
        Ok(()) => Ok(tree.finish()),
        Err(e) => match tree.into_written().undo() {
            Ok(()) => Err(e),
            Err(undo) => Err(Error::unrecoverable(&e, &undo)),
        },
    }
}

fn write_files<Src>(idx: &Index, parts: &[Vec<String>], tree: &mut Tree, src: Src) -> Result<()>
where
    Src: FnMut(&str) -> Result<Vec<u8>>,
{
    let mut stream = StreamReader::new(idx, src);
    for (entry, parts) in idx.files.iter().zip(parts) {
        if !entry.symlink.is_empty() {
            tree.symlink(parts, entry.symlink.as_bytes())?;
            continue;
        }
        let size = u64::try_from(entry.size)
            .map_err(|_| Error::format(format!("{}: negative size", entry.path)))?;
        let mut body = EntryReader {
            stream: &mut stream,
            remaining: size,
        };
        tree.file(parts, entry.mode, size, &mut body)?;
        if body.remaining > 0 {
            return Err(Error::format(format!("{}: short contents", entry.path)));
        }
    }
    Ok(())
}

/// One file's bytes out of the stream, exactly as many as the index gives it.
struct EntryReader<'s, 'a, Src> {
    stream: &'s mut StreamReader<'a, Src>,
    remaining: u64,
}

impl<Src> Read for EntryReader<'_, '_, Src>
where
    Src: FnMut(&str) -> Result<Vec<u8>>,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = self
            .remaining
            .min(buf.len() as u64)
            .min(MAX_CHUNK_SIZE as u64) as usize;
        if want == 0 {
            return Ok(0);
        }
        let data = self.stream.read(want).map_err(|e| match e {
            Error::Io(e) => e,
            Error::Format(m) | Error::Network(m) | Error::Unrecoverable(m) => {
                std::io::Error::other(m)
            }
            e => std::io::Error::other(e),
        })?;
        let n = data.len();
        if let Some(dst) = buf.get_mut(..n) {
            dst.copy_from_slice(&data);
        }
        self.remaining = self.remaining.saturating_sub(n as u64);
        Ok(n)
    }
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

/// Return the bare hex of a `sha256:<hex>` digest — the layout helper for
/// content-addressed stores (`.../sha256/<hex>`).
pub fn digest_hex(digest: &str) -> Option<&str> {
    // Require lowercase hex: this names a local store file, so a forged
    // digest containing `/` or `..` must not become a path.
    digest.strip_prefix("sha256:").filter(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
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
        assemble(&idx, &dst, None, mem_source(&store)).unwrap();

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
        assemble(&idx, &dst, None, mem_source(&store)).unwrap();
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
        assert!(assemble(&idx, &dst, None, mem_source(&store)).is_err());
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
        assemble(&idx, &dst, None, mem_source(&store)).unwrap();
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            Path::new("target.txt")
        );
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
    }

    /// A one-file index of `body` at `path` with `mode`, plus the chunks.
    fn one_file(path: &str, mode: u32, body: &[u8]) -> (Index, HashMap<String, Vec<u8>>) {
        let src = tmpdir("src");
        write_file(&src, "f", body);
        let (mut idx, store) = mem_build(&src, &["f".to_string()], false);
        idx.files[0].path = path.to_string();
        idx.files[0].mode = mode;
        (idx, store)
    }

    fn symlink_entry(path: &str, target: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            mode: 0o777,
            size: 0,
            symlink: target.to_string(),
        }
    }

    #[test]
    fn assemble_refuses_paths_that_leave_the_root() {
        for path in [
            "../x",
            "a/../../x",
            "/abs",
            "a\\..\\..\\x",
            "\\abs",
            "x\0",
            "",
        ] {
            let (idx, store) = one_file(path, 0o644, b"x");
            let root = tmpdir("root");
            let dst = root.join("dst");
            std::fs::create_dir(&dst).unwrap();
            assert!(
                assemble(&idx, &dst, None, mem_source(&store)).is_err(),
                "{path:?}"
            );
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "{path:?}");
            assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 0, "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn assemble_refuses_entries_under_its_own_symlink_before_writing() {
        let outside = tmpdir("outside");
        let (mut idx, store) = one_file("a/secret", 0o644, b"PWN");
        idx.files
            .insert(0, symlink_entry("a", outside.to_str().unwrap()));
        let dst = tmpdir("dst");
        let err = assemble(&idx, &dst, None, mem_source(&store)).unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");
        assert!(!outside.join("secret").exists());
        assert!(std::fs::symlink_metadata(dst.join("a")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn assemble_does_not_follow_symlinks_on_disk() {
        let outside = tmpdir("outside");
        std::fs::write(outside.join("secret"), b"S").unwrap();
        let dst = tmpdir("dst");
        std::os::unix::fs::symlink(&outside, dst.join("p")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dst.join("leaf")).unwrap();
        let (idx, store) = one_file("p/secret", 0o644, b"PWN");
        assert!(assemble(&idx, &dst, None, mem_source(&store)).is_err());
        let (idx, store) = one_file("leaf", 0o644, b"PWN");
        assemble(&idx, &dst, None, mem_source(&store)).unwrap();
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"S");
        assert_eq!(std::fs::read(dst.join("leaf")).unwrap(), b"PWN");
    }

    #[cfg(unix)]
    #[test]
    fn assemble_does_not_write_or_chmod_a_hard_linked_file() {
        use std::os::unix::fs::PermissionsExt;
        let outside = tmpdir("outside");
        let secret = outside.join("secret");
        std::fs::write(&secret, b"S").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let dst = tmpdir("dst");
        std::fs::hard_link(&secret, dst.join("h")).unwrap();
        let (idx, store) = one_file("h", 0o777, b"PWN");
        assemble(&idx, &dst, None, mem_source(&store)).unwrap();
        assert_eq!(std::fs::read(&secret).unwrap(), b"S");
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o600);
        assert_eq!(std::fs::read(dst.join("h")).unwrap(), b"PWN");
    }

    #[cfg(unix)]
    #[test]
    fn assemble_refuses_a_destination_named_twice() {
        let outside = tmpdir("outside");
        let (mut idx, store) = one_file("x", 0o644, b"PWN");
        let target = outside.join("f");
        idx.files
            .insert(0, symlink_entry("x", target.to_str().unwrap()));
        let dst = tmpdir("dst");
        let err = assemble(&idx, &dst, None, mem_source(&store)).unwrap_err();
        assert!(err.to_string().contains("appears twice"), "{err}");
        assert!(!target.exists());
        assert!(std::fs::symlink_metadata(dst.join("x")).is_err());
    }

    #[test]
    fn assemble_refuses_an_oversized_symlink_target() {
        let idx = Index {
            files: vec![symlink_entry("l", &"t/".repeat(4096))],
            ..Index::new()
        };
        let dst = tmpdir("dst");
        assert!(assemble(&idx, &dst, None, |_: &str| Ok(Vec::new())).is_err());
    }

    #[test]
    fn digest_hex_takes_only_lowercase_hex() {
        let good = format!("sha256:{}", "a".repeat(64));
        assert!(digest_hex(&good).is_some());
        let forged = format!("sha256:{:/<64}", "../../../../tmp/x");
        assert!(digest_hex(&forged).is_none());
        assert!(digest_hex(&format!("sha256:{}", "A".repeat(64))).is_none());
    }

    /// A restore failing partway whose undo fails too (here: the directory of
    /// what it wrote turns read-only) is `Unrecoverable`, not a plain failure.
    #[cfg(unix)]
    #[test]
    fn a_partial_restore_that_cannot_be_undone_is_unrecoverable() {
        use std::os::unix::fs::PermissionsExt;
        let src = tmpdir("src");
        write_file(&src, "f0", b"first");
        write_file(&src, "f1", b"second");
        let (mut idx, store) = mem_build(&src, &["f0".to_string(), "f1".to_string()], false);
        idx.files[0].path = "sub/a".to_string();
        idx.files[1].path = "b".to_string();
        // One more chunk the second file needs, which fails to fetch.
        let fake = format!("sha256:{}", "f".repeat(64));
        idx.chunks.push(ChunkRef {
            digest: fake.clone(),
            size: 1,
            raw_size: 1,
        });
        idx.files[1].size += 1;
        let dst = tmpdir("dst");
        let base = dst.clone();
        let err = assemble(&idx, &dst, None, |d: &str| {
            if d == fake {
                std::fs::set_permissions(base.join("sub"), std::fs::Permissions::from_mode(0o555))
                    .unwrap();
                return Err(Error::format("injected fetch failure"));
            }
            mem_source(&store)(d)
        })
        .unwrap_err();
        std::fs::set_permissions(dst.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(err, Error::Unrecoverable(_)), "{err}");
        assert!(err.to_string().contains("injected fetch failure"), "{err}");

        // The same failure with an undo that works is an ordinary error.
        let dst = tmpdir("dst2");
        let err = assemble(&idx, &dst, None, |d: &str| {
            if d == fake {
                return Err(Error::format("injected fetch failure"));
            }
            mem_source(&store)(d)
        })
        .unwrap_err();
        assert!(!matches!(err, Error::Unrecoverable(_)), "{err}");
        assert!(!dst.join("sub").exists());
    }
}
