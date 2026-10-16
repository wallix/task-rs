//! The classic (non-OCI) remote build cache: a zip archive of a task's
//! generated files, with the cache metadata stored as newline-separated
//! `key:value` pairs in the zip comment.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ocicas::restore::{DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, MAX_LINK_TARGET, MODE_MASK, Tree};
use zip::write::{FileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::error::CacheError;

/// The metadata stored in the zip comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheMeta {
    pub task: String,
    pub sources: String,
    pub generates: String,
}

/// Build the comment string stored in the zip: newline-separated `key:value`.
pub fn cache_comment(task_name: &str, sources_hash: &str, generates_hash: &str) -> String {
    format!("task:{task_name}\nsources:{sources_hash}\ngenerates:{generates_hash}")
}

/// Parse a zip comment into a [`CacheMeta`]. Unknown lines are ignored.
pub fn read_cache_comment(comment: &str) -> CacheMeta {
    let mut m = CacheMeta::default();
    for line in comment.split('\n') {
        if let Some(v) = line.strip_prefix("task:") {
            m.task = v.to_string();
        } else if let Some(v) = line.strip_prefix("sources:") {
            m.sources = v.to_string();
        } else if let Some(v) = line.strip_prefix("generates:") {
            m.generates = v.to_string();
        }
    }
    m
}

/// Add a single file (or symlink) to a zip writer, storing it relative to
/// `base_dir`. Rejects files outside `base_dir`. Directories are skipped.
fn add_file_to_zip<W: Write + std::io::Seek>(
    zw: &mut ZipWriter<W>,
    base_dir: &Path,
    file_path: &Path,
) -> Result<(), CacheError> {
    let meta = fs::symlink_metadata(file_path)?;
    if meta.is_dir() {
        return Ok(());
    }

    let rel = file_path.strip_prefix(base_dir).map_err(|_| {
        CacheError::msg(format!(
            "file {file_path:?} is outside project root {base_dir:?}"
        ))
    })?;
    let name = rel.to_string_lossy().replace('\\', "/");

    let mode = perm_bits(&meta);
    let is_symlink = meta.file_type().is_symlink();

    if is_symlink {
        let target = fs::read_link(file_path)?;
        let opts: FileOptions<'_, ()> = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(mode);
        zw.add_symlink(name, target.to_string_lossy(), opts)?;
    } else {
        let opts: FileOptions<'_, ()> = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(mode);
        zw.start_file(name, opts)?;
        let data = fs::read(file_path)?;
        zw.write_all(&data)?;
    }
    Ok(())
}

/// Create a zip of the given files (relative or absolute, resolved against
/// `dir`) with `meta` in the comment, writing it to `dest`. `files` are the
/// disk paths; they must live under `dir`.
pub fn write_archive(
    dest: &Path,
    dir: &Path,
    files: &[String],
    meta: &CacheMeta,
) -> Result<(), CacheError> {
    let file = fs::File::create(dest)?;
    let mut zw = ZipWriter::new(file);
    for f in files {
        let path = resolve(dir, f);
        add_file_to_zip(&mut zw, dir, &path)?;
    }
    zw.set_comment(cache_comment(&meta.task, &meta.sources, &meta.generates));
    zw.finish()?;
    Ok(())
}

/// What a restore may write. A cache entry may come from a shared remote cache,
/// so it is untrusted: every entry is checked against the policy before any is
/// written, and a refusal leaves the tree untouched. The writes themselves are
/// confined to the root by [`ocicas::restore`], which also documents what is
/// left unbounded.
#[derive(Default)]
pub struct RestorePolicy<'a> {
    /// Checks the cache metadata; an error refuses the entry.
    pub check_meta: Option<&'a MetaCheck<'a>>,
    /// Whether the entry at this root-relative, slash-separated path may be
    /// written. Entries naming a `.git` component are refused regardless.
    pub accept: Option<&'a dyn Fn(&str) -> bool>,
}

/// A check on a cache entry's metadata.
pub type MetaCheck<'a> = dyn Fn(&CacheMeta) -> Result<(), CacheError> + 'a;

impl RestorePolicy<'_> {
    /// Check entry `name`, already split into `parts`, against the policy. A
    /// `.git` component is refused, so no restore can plant hooks or rewrite a
    /// repository's configuration.
    pub fn check_entry(&self, name: &str, parts: &[String]) -> Result<(), CacheError> {
        if parts.iter().any(|p| is_git_name(p)) {
            return Err(CacheError::rejected(format!(
                "entry {name:?} is inside a .git directory"
            )));
        }
        if let Some(accept) = self.accept
            && !accept(&parts.join("/"))
        {
            return Err(CacheError::rejected(format!(
                "entry {name:?} is not one of the files this restore may write"
            )));
        }
        Ok(())
    }
}

/// Whether path component `part` names a `.git` directory: case-insensitively,
/// and on Windows also as its 8.3 short name `GIT~<n>`, as git's own
/// `core.protectNTFS` does.
pub fn is_git_name(part: &str) -> bool {
    if part.eq_ignore_ascii_case(".git") {
        return true;
    }
    cfg!(windows)
        && part.len() > 4
        && part
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("git~"))
        && part
            .get(4..)
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
}

/// The cache files under `dir` a restore would refuse, as inside a `.git`
/// directory, split off `files`: saving them would make the entry unusable.
pub fn split_git_files(dir: &Path, files: &[String]) -> (Vec<String>, Vec<String>) {
    files.iter().cloned().partition(|f| {
        let rel = resolve(dir, f);
        let rel = rel.strip_prefix(dir).unwrap_or(&rel);
        !rel.components()
            .any(|c| is_git_name(&c.as_os_str().to_string_lossy()))
    })
}

/// A restore failure from [`ocicas::restore`], as this module reports its own.
fn restore_err(e: ocicas::Error) -> CacheError {
    match e {
        ocicas::Error::Io(e) => CacheError::Io(e),
        ocicas::Error::Format(m) => CacheError::Msg(m),
        e => CacheError::Ocicas(e),
    }
}

/// An entry name split into the components a restore writes. Backslashes
/// separate too: some Windows writers use them.
fn entry_components(name: &str) -> Result<Vec<String>, CacheError> {
    ocicas::restore::components(name, true).map_err(restore_err)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    Symlink,
    File,
}

/// An entry checked against the policy, ready to be written.
struct Planned {
    index: usize,
    name: String,
    parts: Vec<String>,
    kind: Kind,
    mode: u32,
    size: u64,
}

/// Check every entry of `archive` against `policy` and return them in order,
/// or the first refusal. Nothing is written.
fn plan<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    policy: &RestorePolicy<'_>,
) -> Result<Vec<Planned>, CacheError> {
    let mut planned = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        let kind = if entry.is_dir() {
            Kind::Dir
        } else if entry.is_symlink() {
            Kind::Symlink
        } else {
            Kind::File
        };
        let default = if kind == Kind::Dir {
            DEFAULT_DIR_MODE
        } else {
            DEFAULT_FILE_MODE
        };
        let mode = entry.unix_mode().map_or(default, |m| m & MODE_MASK);
        let size = entry.size();
        drop(entry);
        let parts = entry_components(&name)?;
        policy.check_entry(&name, &parts)?;
        planned.push(Planned {
            index,
            name,
            parts,
            kind,
            mode,
            size,
        });
    }
    ocicas::restore::check_link_parents(
        planned
            .iter()
            .map(|p| (p.parts.as_slice(), p.kind == Kind::Symlink)),
    )
    .map_err(restore_err)?;
    Ok(planned)
}

/// Read a symlink entry's target, refusing one longer than [`MAX_LINK_TARGET`].
fn link_target(name: &str, entry: impl Read) -> Result<Vec<u8>, CacheError> {
    let mut target = Vec::new();
    entry.take(MAX_LINK_TARGET + 1).read_to_end(&mut target)?;
    if target.len() as u64 > MAX_LINK_TARGET {
        return Err(CacheError::msg(format!(
            "entry {name:?}: symlink target is longer than {MAX_LINK_TARGET} bytes"
        )));
    }
    Ok(target)
}

/// Write one planned entry into `tree`. A file is left alone only when it is
/// already identical, mode included, and linked nowhere else; otherwise it is
/// written as a fresh inode.
fn write_entry<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    p: &Planned,
    tree: &mut Tree,
) -> Result<(), CacheError> {
    match p.kind {
        Kind::Dir => tree.dir(&p.parts, p.mode).map_err(restore_err),
        Kind::Symlink => {
            let target = link_target(&p.name, archive.by_index(p.index)?)?;
            tree.symlink(&p.parts, &target).map_err(restore_err)
        }
        Kind::File => {
            if tree
                .unchanged(&p.parts, p.mode, p.size, archive.by_index(p.index)?)
                .map_err(restore_err)?
            {
                return Ok(());
            }
            tree.file(&p.parts, p.mode, p.size, archive.by_index(p.index)?)
                .map_err(restore_err)
        }
    }
}

/// Extract every entry of the archive at `zip_path` into `base_dir`, as
/// `policy` allows, and return the parsed comment [`CacheMeta`]. The comment
/// and every entry name are checked before anything is written; a failure
/// while writing aborts and is returned.
///
/// Names that escape the root, or whose parent chain crosses a symlink, are
/// refused, and every write is confined to `base_dir` (see
/// [`ocicas::restore`]). Permission bits are applied without setuid, setgid or
/// sticky; directories get theirs last, so a read-only one does not stop its
/// contents from being restored.
pub fn extract_archive(
    zip_path: &Path,
    base_dir: &Path,
    policy: &RestorePolicy<'_>,
) -> Result<CacheMeta, CacheError> {
    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;
    let meta = read_cache_comment(std::str::from_utf8(archive.comment()).unwrap_or(""));
    if let Some(check) = policy.check_meta {
        check(&meta)?;
    }
    let planned = plan(&mut archive, policy)?;

    let mut tree = Tree::open(base_dir).map_err(restore_err)?;
    for p in &planned {
        write_entry(&mut archive, p, &mut tree)?;
    }
    tree.finish().map_err(restore_err)?;
    Ok(meta)
}

/// Whether the archive at `zip_path` contains exactly the given files with
/// identical content. Used to skip re-exporting an unchanged cache. Reads go
/// through the same link-free directory walk as extraction, so neither an
/// entry name nor a symlink on disk can make it read outside `base_dir`.
pub fn archive_matches(base_dir: &Path, zip_path: &Path, files: &[String]) -> bool {
    let Ok(file) = fs::File::open(zip_path) else {
        return false;
    };
    let Ok(mut archive) = ZipArchive::new(file) else {
        return false;
    };
    let Ok(mut tree) = Tree::open(base_dir) else {
        return false;
    };

    let mut seen: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for f in files {
        seen.insert(resolve(base_dir, f), false);
    }

    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            return false;
        };
        let name = entry.name().to_string();
        let (is_symlink, size) = (entry.is_symlink(), entry.size());
        // Only a name extraction would accept can match a file under base_dir.
        let Ok(parts) = entry_components(&name) else {
            return false;
        };
        let fpath: PathBuf = std::iter::once(base_dir)
            .chain(parts.iter().map(Path::new))
            .collect();
        if !seen.contains_key(&fpath) {
            return false; // extraneous file in zip
        }
        if !tree.matches(&parts, is_symlink, size, entry) {
            return false;
        }
        seen.insert(fpath, true);
    }

    for (f, found) in &seen {
        if !found {
            // A listed directory has no entry of its own; check it through the
            // same confined walk.
            let parts: Option<Vec<String>> = f.strip_prefix(base_dir).ok().map(|rel| {
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect()
            });
            if parts.is_some_and(|parts| tree.is_dir(&parts)) {
                continue;
            }
            return false;
        }
    }
    true
}

/// Resolve a possibly-relative cache file path against the working directory.
fn resolve(dir: &Path, f: &str) -> PathBuf {
    let p = Path::new(f);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}

/// Read the unix permission bits of `meta`. On non-unix platforms there is no
/// mode, so approximate from the readonly flag — enough to store a sane value
/// in the zip's `unix_permissions` for a unix consumer.
#[cfg(unix)]
fn perm_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn perm_bits(meta: &fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn comment_of(bytes: &[u8]) -> String {
        let archive = ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        std::str::from_utf8(archive.comment()).unwrap().to_string()
    }

    fn build_with_comment(comment: &str) -> Vec<u8> {
        let mut zw = ZipWriter::new(Cursor::new(Vec::new()));
        if !comment.is_empty() {
            zw.set_comment(comment.to_string());
        }
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn set_cache_comment_round_trip() {
        let c = cache_comment("myapp:build", "abc123", "def456");
        let bytes = build_with_comment(&c);
        let meta = read_cache_comment(&comment_of(&bytes));
        assert_eq!(meta.task, "myapp:build");
        assert_eq!(meta.sources, "abc123");
        assert_eq!(meta.generates, "def456");
    }

    #[test]
    fn read_cache_comment_empty() {
        let bytes = build_with_comment("");
        let meta = read_cache_comment(&comment_of(&bytes));
        assert_eq!(meta, CacheMeta::default());
    }

    #[test]
    fn read_cache_comment_partial_fields() {
        let meta = read_cache_comment("generates:abc123");
        assert_eq!(meta.task, "");
        assert_eq!(meta.sources, "");
        assert_eq!(meta.generates, "abc123");
    }

    #[test]
    fn read_cache_comment_unknown_fields() {
        let meta = read_cache_comment("task:foo\nfuture_field:bar\ngenerates:abc");
        assert_eq!(meta.task, "foo");
        assert_eq!(meta.generates, "abc");
    }

    #[test]
    fn set_cache_comment_with_colons_in_task_name() {
        let c = cache_comment("bastionadm:node_modules", "src1", "gen1");
        let bytes = build_with_comment(&c);
        let meta = read_cache_comment(&comment_of(&bytes));
        assert_eq!(meta.task, "bastionadm:node_modules");
    }

    #[test]
    fn write_extract_and_match_round_trip() {
        let src = tempdir();
        fs::write(src.join("out.txt"), b"hello").unwrap();
        let zip_path = src.join("cache.zip");
        let meta = CacheMeta {
            task: "build".into(),
            sources: "s".into(),
            generates: "g".into(),
        };
        write_archive(&zip_path, &src, &["out.txt".to_string()], &meta).unwrap();

        assert!(archive_matches(&src, &zip_path, &["out.txt".to_string()]));

        let dst = tempdir();
        let got = extract_archive(&zip_path, &dst, &RestorePolicy::default()).unwrap();
        assert_eq!(got, meta);
        assert_eq!(fs::read(dst.join("out.txt")).unwrap(), b"hello");
    }

    type Zw = ZipWriter<Cursor<Vec<u8>>>;

    fn zip_bytes(build: impl FnOnce(&mut Zw)) -> Vec<u8> {
        let mut zw = ZipWriter::new(Cursor::new(Vec::new()));
        build(&mut zw);
        zw.finish().unwrap().into_inner()
    }

    fn add_file(zw: &mut Zw, name: &str, data: &[u8]) {
        zw.start_file(name, SimpleFileOptions::default()).unwrap();
        zw.write_all(data).unwrap();
    }

    fn add_link(zw: &mut Zw, name: &str, target: &str) {
        zw.add_symlink(name, target, SimpleFileOptions::default())
            .unwrap();
    }

    /// Extract `bytes` into `dst` from an archive file kept outside it.
    fn extract_bytes(bytes: &[u8], dst: &Path) -> Result<CacheMeta, CacheError> {
        extract_with(bytes, dst, &RestorePolicy::default())
    }

    fn extract_with(
        bytes: &[u8],
        dst: &Path,
        policy: &RestorePolicy<'_>,
    ) -> Result<CacheMeta, CacheError> {
        let zip_path = tempdir().join("cache.zip");
        fs::write(&zip_path, bytes).unwrap();
        extract_archive(&zip_path, dst, policy)
    }

    fn matches_bytes(bytes: &[u8], dst: &Path, files: &[&str]) -> bool {
        let zip_path = tempdir().join("cache.zip");
        fs::write(&zip_path, bytes).unwrap();
        let files: Vec<String> = files.iter().map(|f| (*f).to_string()).collect();
        archive_matches(dst, &zip_path, &files)
    }

    /// Replace every occurrence of `from` (an entry name, in both its headers)
    /// with `to`, of the same length.
    fn patch_name(bytes: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        let mut i = 0;
        while i + from.len() <= bytes.len() {
            if &bytes[i..i + from.len()] == from {
                bytes[i..i + from.len()].copy_from_slice(to);
            }
            i += 1;
        }
    }

    /// Offsets of the central directory headers, in entry order.
    fn central_headers(bytes: &[u8]) -> Vec<usize> {
        bytes
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == b"PK\x01\x02")
            .map(|(i, _)| i)
            .collect()
    }

    fn patch_u32(bytes: &mut [u8], at: usize, f: impl FnOnce(u32) -> u32) {
        let field: &mut [u8; 4] = (&mut bytes[at..at + 4]).try_into().unwrap();
        *field = f(u32::from_le_bytes(*field)).to_le_bytes();
    }

    /// Rewrite the full unix mode (type and permission bits) of entry `i`.
    #[cfg(unix)]
    fn patch_unix_mode(bytes: &mut [u8], i: usize, mode: u32) {
        let cdh = central_headers(bytes)[i];
        patch_u32(bytes, cdh + 38, |attrs| (mode << 16) | (attrs & 0xffff));
    }

    /// Rewrite the uncompressed size entry `i` declares, in both headers.
    fn patch_declared_size(bytes: &mut [u8], i: usize, size: u32) {
        let cdh = central_headers(bytes)[i];
        let mut local = 0;
        patch_u32(bytes, cdh + 42, |off| {
            local = off as usize;
            off
        });
        patch_u32(bytes, cdh + 24, |_| size);
        patch_u32(bytes, local + 22, |_| size);
    }

    #[test]
    fn extract_rejects_parent_components() {
        for name in ["../escape", "a/../../escape", "a/../b", "a\\..\\..\\escape"] {
            let root = tempdir();
            let dst = root.join("dst");
            fs::create_dir(&dst).unwrap();
            let bytes = zip_bytes(|zw| add_file(zw, name, b"x"));
            assert!(extract_bytes(&bytes, &dst).is_err(), "{name:?} accepted");
            assert!(!root.join("escape").exists(), "{name:?} escaped");
        }
    }

    #[test]
    fn extract_rejects_absolute_names() {
        let outside = tempdir();
        let abs = outside.join("abs").to_string_lossy().into_owned();
        // A backslash-separated absolute name is a relative file name to the
        // unix path API, but turns absolute once separators are normalised.
        for name in [abs.clone(), abs.replace('/', "\\")] {
            let dst = tempdir();
            let bytes = zip_bytes(|zw| add_file(zw, &name, b"x"));
            assert!(extract_bytes(&bytes, &dst).is_err(), "{name:?} accepted");
            assert!(!outside.join("abs").exists(), "{name:?} escaped");
        }
    }

    #[cfg(windows)]
    #[test]
    fn extract_rejects_windows_prefixes() {
        for name in ["C:escape", "C:/escape", "//server/share/escape"] {
            let dst = tempdir();
            let bytes = zip_bytes(|zw| add_file(zw, name, b"x"));
            assert!(extract_bytes(&bytes, &dst).is_err(), "{name:?} accepted");
        }
    }

    #[cfg(unix)]
    #[test]
    fn extract_rejects_parent_component_after_symlink() {
        // `a/..` is `outside` to the kernel, which resolves `a` first, though
        // a lexical clean reads it as the extraction root.
        let outside = tempdir();
        fs::create_dir(outside.join("sub")).unwrap();
        let dst = tempdir();
        let bytes = zip_bytes(|zw| {
            add_link(zw, "a", outside.join("sub").to_str().unwrap());
            add_file(zw, "a/../escape", b"x");
        });
        assert!(extract_bytes(&bytes, &dst).is_err());
        assert!(!outside.join("escape").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extract_rejects_entry_under_symlinked_parent() {
        let outside = tempdir();
        let dst = tempdir();
        let bytes = zip_bytes(|zw| {
            add_link(zw, "a", outside.to_str().unwrap());
            add_file(zw, "a/passwd", b"x");
        });
        let err = extract_bytes(&bytes, &dst).unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");
        assert!(!outside.join("passwd").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extract_keeps_symlink_targets_outside_the_root() {
        // A link may point anywhere (a virtualenv's `bin/python`); what
        // extraction must not do is write through one, tested above.
        let dst = tempdir();
        let bytes = zip_bytes(|zw| add_link(zw, "python", "/usr/bin/python3"));
        extract_bytes(&bytes, &dst).unwrap();
        assert_eq!(
            fs::read_link(dst.join("python")).unwrap(),
            Path::new("/usr/bin/python3")
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_directory_entry_does_not_chmod_through_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let outside = tempdir();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        let dst = tempdir();
        let bytes = zip_bytes(|zw| {
            add_link(zw, "d", outside.to_str().unwrap());
            zw.add_directory("d/", SimpleFileOptions::default().unix_permissions(0o777))
                .unwrap();
        });
        extract_bytes(&bytes, &dst).unwrap();
        let mode = fs::metadata(&outside).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o700);
        assert!(fs::symlink_metadata(dst.join("d")).unwrap().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn extract_strips_setuid_setgid_and_sticky_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dst = tempdir();
        let mut bytes = zip_bytes(|zw| {
            add_file(zw, "bin", b"#!/bin/sh\n");
            zw.add_directory("shared/", SimpleFileOptions::default())
                .unwrap();
        });
        patch_unix_mode(&mut bytes, 0, 0o106_755);
        patch_unix_mode(&mut bytes, 1, 0o041_777);
        extract_bytes(&bytes, &dst).unwrap();
        let mode = |p: &str| fs::metadata(dst.join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("bin"), 0o755);
        assert_eq!(mode("shared"), 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn extract_replaces_existing_symlink_instead_of_writing_through() {
        let outside = tempdir();
        fs::write(outside.join("file"), b"same").unwrap();
        let dst = tempdir();
        std::os::unix::fs::symlink(outside.join("file"), dst.join("out")).unwrap();
        // Same bytes as the link's target, so a by-path comparison that
        // followed the link would also skip the rewrite.
        let bytes = zip_bytes(|zw| add_file(zw, "out", b"same"));
        extract_bytes(&bytes, &dst).unwrap();
        let fi = fs::symlink_metadata(dst.join("out")).unwrap();
        assert!(fi.is_file(), "the link was kept");

        fs::remove_file(dst.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.join("file"), dst.join("out")).unwrap();
        let bytes = zip_bytes(|zw| add_file(zw, "out", b"new"));
        extract_bytes(&bytes, &dst).unwrap();
        assert_eq!(fs::read(outside.join("file")).unwrap(), b"same");
        assert_eq!(fs::read(dst.join("out")).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn extract_duplicate_entry_does_not_follow_earlier_symlink() {
        let outside = tempdir();
        let dst = tempdir();
        let mut bytes = zip_bytes(|zw| {
            add_link(zw, "dupA", outside.join("f").to_str().unwrap());
            add_file(zw, "dupB", b"payload");
        });
        // ZipWriter refuses a duplicate name; rename the second entry in place.
        for i in 0..bytes.len() - 3 {
            if &bytes[i..i + 4] == b"dupB" {
                bytes[i + 3] = b'A';
            }
        }
        extract_bytes(&bytes, &dst).unwrap();
        assert!(!outside.join("f").exists());
        assert_eq!(fs::read(dst.join("dupA")).unwrap(), b"payload");
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn extract_does_not_chmod_or_write_a_hard_linked_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Same bytes and different bytes: neither may touch the shared inode.
        for data in [&b"S"[..], b"PWN"] {
            let outside = tempdir();
            let secret = outside.join("secret");
            fs::write(&secret, b"S").unwrap();
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
            let dst = tempdir();
            fs::hard_link(&secret, dst.join("h")).unwrap();
            let bytes = zip_bytes(|zw| {
                zw.start_file("h", SimpleFileOptions::default().unix_permissions(0o777))
                    .unwrap();
                zw.write_all(data).unwrap();
            });
            extract_bytes(&bytes, &dst).unwrap();
            assert_eq!(mode_of(&secret), 0o600);
            assert_eq!(fs::read(&secret).unwrap(), b"S");
            assert_eq!(fs::read(dst.join("h")).unwrap(), data);
            assert_eq!(mode_of(&dst.join("h")), 0o777);
            assert_eq!(fs::metadata(&secret).unwrap().nlink(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn extract_keeps_an_identical_file_untouched() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dst = tempdir();
        fs::write(dst.join("f"), b"same").unwrap();
        fs::set_permissions(dst.join("f"), fs::Permissions::from_mode(0o640)).unwrap();
        let ino = fs::metadata(dst.join("f")).unwrap().ino();
        let bytes = zip_bytes(|zw| {
            zw.start_file("f", SimpleFileOptions::default().unix_permissions(0o640))
                .unwrap();
            zw.write_all(b"same").unwrap();
        });
        extract_bytes(&bytes, &dst).unwrap();
        assert_eq!(fs::metadata(dst.join("f")).unwrap().ino(), ino);
    }

    #[cfg(unix)]
    #[test]
    fn extract_without_a_mode_drops_existing_privileged_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dst = tempdir();
        fs::write(dst.join("f"), b"x").unwrap();
        fs::set_permissions(dst.join("f"), fs::Permissions::from_mode(0o4755)).unwrap();
        let mut bytes = zip_bytes(|zw| add_file(zw, "f", b"x"));
        // No external attributes at all: the entry carries no mode.
        let cdh = central_headers(&bytes)[0];
        patch_u32(&mut bytes, cdh + 38, |_| 0);
        extract_bytes(&bytes, &dst).unwrap();
        assert_eq!(mode_of(&dst.join("f")), DEFAULT_FILE_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn extract_restores_into_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dst = tempdir();
        let bytes = zip_bytes(|zw| {
            zw.add_directory("d/", SimpleFileOptions::default().unix_permissions(0o555))
                .unwrap();
            add_file(zw, "d/x", b"x");
        });
        extract_bytes(&bytes, &dst).unwrap();
        assert_eq!(fs::read(dst.join("d/x")).unwrap(), b"x");
        assert_eq!(mode_of(&dst.join("d")), 0o555);
        fs::set_permissions(dst.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn extract_refuses_entries_the_policy_does_not_accept_before_writing() {
        let dst = tempdir();
        let bytes = zip_bytes(|zw| {
            add_file(zw, "out/a", b"a");
            add_file(zw, "Taskfile.yml", b"pwn");
        });
        let accept = |rel: &str| rel.starts_with("out/");
        let policy = RestorePolicy {
            check_meta: None,
            accept: Some(&accept),
        };
        let err = extract_with(&bytes, &dst, &policy).unwrap_err();
        assert!(matches!(err, CacheError::Rejected(_)), "{err}");
        assert!(!dst.join("out").exists());
        assert!(!dst.join("Taskfile.yml").exists());
    }

    #[test]
    fn extract_checks_the_metadata_before_writing() {
        let dst = tempdir();
        let bytes = zip_bytes(|zw| {
            add_file(zw, "out", b"a");
            zw.set_comment(cache_comment("other", "s", "g"));
        });
        let check = |m: &CacheMeta| {
            if m.task == "build" {
                Ok(())
            } else {
                Err(CacheError::rejected("task name mismatch"))
            }
        };
        let policy = RestorePolicy {
            check_meta: Some(&check),
            accept: None,
        };
        assert!(extract_with(&bytes, &dst, &policy).is_err());
        assert!(!dst.join("out").exists());
    }

    #[test]
    fn git_names() {
        for part in [".git", ".GIT", ".Git"] {
            assert!(is_git_name(part), "{part}");
        }
        assert_eq!(is_git_name("GIT~1"), cfg!(windows));
        assert_eq!(is_git_name("git~12"), cfg!(windows));
        for part in ["git", ".gitignore", "git~", "GIT~x", ".git2"] {
            assert!(!is_git_name(part), "{part}");
        }
        let dir = Path::new("/p");
        let (keep, git) = split_git_files(
            dir,
            &["out/a".into(), "/p/sub/.git/HEAD".into(), ".git/x".into()],
        );
        assert_eq!(keep, ["out/a"]);
        assert_eq!(git, ["/p/sub/.git/HEAD", ".git/x"]);
    }

    #[test]
    fn extract_refuses_git_entries() {
        for name in [".git/hooks/pre-commit", "sub/.GIT/config"] {
            let dst = tempdir();
            let bytes = zip_bytes(|zw| add_file(zw, name, b"x"));
            assert!(extract_bytes(&bytes, &dst).is_err(), "{name:?} accepted");
        }
    }

    #[cfg(windows)]
    #[test]
    fn extract_rejects_windows_aliased_names() {
        for name in ["NUL", "out/con.txt", "out/x.", "out/x ", "out/a:stream"] {
            let dst = tempdir();
            let bytes = zip_bytes(|zw| add_file(zw, name, b"x"));
            assert!(extract_bytes(&bytes, &dst).is_err(), "{name:?} accepted");
        }
    }

    #[test]
    fn extract_decodes_a_legacy_cp437_name() {
        // An old writer's name: not UTF-8, no UTF-8 flag, so CP437 (0x82 = é).
        let dst = tempdir();
        let mut bytes = zip_bytes(|zw| add_file(zw, "cafX", b"x"));
        patch_name(&mut bytes, b"cafX", b"caf\x82");
        extract_bytes(&bytes, &dst).unwrap();
        assert_eq!(fs::read(dst.join("caf\u{e9}")).unwrap(), b"x");
    }

    #[cfg(unix)]
    #[test]
    fn archive_matches_does_not_read_through_symlinks() {
        let outside = tempdir();
        fs::write(outside.join("secret"), b"S").unwrap();
        let dst = tempdir();
        // A leaf link: the entry is a file, the disk holds a link to one.
        std::os::unix::fs::symlink(outside.join("secret"), dst.join("out")).unwrap();
        let bytes = zip_bytes(|zw| add_file(zw, "out", b"S"));
        assert!(!matches_bytes(&bytes, &dst, &["out"]));
        // A parent link.
        std::os::unix::fs::symlink(&outside, dst.join("p")).unwrap();
        let bytes = zip_bytes(|zw| add_file(zw, "p/secret", b"S"));
        assert!(!matches_bytes(&bytes, &dst, &["p/secret"]));
    }

    #[test]
    fn archive_matches_compares_sizes_and_contents() {
        let dst = tempdir();
        fs::write(dst.join("a"), b"abc").unwrap();
        let same = zip_bytes(|zw| add_file(zw, "a", b"abc"));
        assert!(matches_bytes(&same, &dst, &["a"]));
        let longer = zip_bytes(|zw| add_file(zw, "a", b"abcd"));
        assert!(!matches_bytes(&longer, &dst, &["a"]));
        let other = zip_bytes(|zw| add_file(zw, "a", b"abd"));
        assert!(!matches_bytes(&other, &dst, &["a"]));
    }

    #[cfg(unix)]
    #[test]
    fn archive_matches_does_not_read_hard_linked_files() {
        let outside = tempdir();
        fs::write(outside.join("secret"), b"S").unwrap();
        let dst = tempdir();
        fs::hard_link(outside.join("secret"), dst.join("h")).unwrap();
        let bytes = zip_bytes(|zw| add_file(zw, "h", b"S"));
        assert!(!matches_bytes(&bytes, &dst, &["h"]));
    }

    #[test]
    fn extract_rejects_oversized_symlink_target() {
        let dst = tempdir();
        let target = "t/".repeat(4096);
        let bytes = zip_bytes(|zw| add_link(zw, "l", &target));
        let err = extract_bytes(&bytes, &dst).unwrap_err();
        assert!(err.to_string().contains("symlink target"), "{err}");
    }

    #[test]
    fn extract_rejects_entry_larger_than_declared() {
        let dst = tempdir();
        let mut bytes = zip_bytes(|zw| {
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zw.start_file("bomb", opts).unwrap();
            zw.write_all(&[0u8; 1 << 20]).unwrap();
        });
        patch_declared_size(&mut bytes, 0, 100);
        assert!(extract_bytes(&bytes, &dst).is_err());
        assert!(fs::metadata(dst.join("bomb")).map_or(0, |m| m.len()) <= 101);
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "task-cache-test-{}-{}",
            std::process::id(),
            fastid()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn fastid() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }
}
