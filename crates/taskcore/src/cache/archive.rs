//! The classic (non-OCI) remote build cache: a zip archive of a task's
//! generated files, with the cache metadata stored as newline-separated
//! `key:value` pairs in the zip comment.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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

/// Reject a path any of whose existing parent directories is a symlink:
/// writing under it would follow the link out of `base_dir`. `slash_path` is
/// relative and slash-separated; the leaf is not checked.
fn check_no_symlink_parents(base_dir: &Path, slash_path: &str) -> Result<(), CacheError> {
    let parts: Vec<&str> = slash_path.split('/').collect();
    let Some(dirs) = parts.get(..parts.len().saturating_sub(1)) else {
        return Ok(());
    };
    let mut dir = base_dir.to_path_buf();
    for part in dirs {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(fi) => {
                if fi.file_type().is_symlink() {
                    return Err(CacheError::msg(format!(
                        "entry {slash_path:?}: parent {part:?} is a symlink"
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Extract a single zip entry to `base_dir`, rejecting names that escape the
/// root or whose parent chain crosses a symlink from an earlier entry.
fn extract_zip_entry(
    base_dir: &Path,
    name: &str,
    is_dir: bool,
    is_symlink: bool,
    mode: Option<u32>,
    data: &[u8],
) -> Result<(), CacheError> {
    let slash = name.replace('\\', "/");
    let clean = clean_slash(&slash);
    if clean == ".." || clean.starts_with("../") || Path::new(name).is_absolute() {
        return Err(CacheError::msg(format!(
            "entry {name:?} is outside the extraction root"
        )));
    }
    check_no_symlink_parents(base_dir, &clean)?;

    let path = base_dir.join(&slash);

    if is_dir {
        fs::create_dir_all(&path)?;
        if let Some(m) = mode {
            set_mode(&path, m);
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Skip a rewrite when the on-disk entry is already identical.
    if let Ok(existing) = fs::symlink_metadata(&path) {
        let existing_is_symlink = existing.file_type().is_symlink();
        if existing_is_symlink == is_symlink {
            if is_symlink {
                if let Ok(link) = fs::read_link(&path)
                    && link.as_os_str().as_encoded_bytes() == data
                {
                    return Ok(());
                }
            } else if let Ok(cur) = fs::read(&path)
                && cur == data
            {
                if let Some(m) = mode {
                    set_mode(&path, m);
                }
                return Ok(());
            }
        }
        let _ = fs::remove_file(&path);
    }

    if is_symlink {
        let target = String::from_utf8_lossy(data);
        make_symlink(target.as_ref(), &path)?;
        return Ok(());
    }

    let mut out = fs::File::create(&path)?;
    out.write_all(data)?;
    drop(out);
    if let Some(m) = mode {
        set_mode(&path, m);
    }
    Ok(())
}

/// Extract every entry of the archive at `zip_path` into `base_dir`. Reads the
/// comment first and returns the parsed [`CacheMeta`]. A per-entry extraction
/// failure aborts and is returned.
pub fn extract_archive(zip_path: &Path, base_dir: &Path) -> Result<CacheMeta, CacheError> {
    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;
    let meta = read_cache_comment(std::str::from_utf8(archive.comment()).unwrap_or(""));

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        let is_dir = entry.is_dir();
        let is_symlink = entry.is_symlink();
        let mode = entry.unix_mode();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        drop(entry);
        extract_zip_entry(base_dir, &name, is_dir, is_symlink, mode, &data)?;
    }
    Ok(meta)
}

/// Whether the archive at `zip_path` contains exactly the given files with
/// identical content. Used to skip re-exporting an unchanged cache.
pub fn archive_matches(base_dir: &Path, zip_path: &Path, files: &[String]) -> bool {
    let Ok(file) = fs::File::open(zip_path) else {
        return false;
    };
    let Ok(mut archive) = ZipArchive::new(file) else {
        return false;
    };

    let mut seen: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for f in files {
        seen.insert(resolve(base_dir, f), false);
    }

    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            return false;
        };
        let fpath = base_dir.join(entry.name());
        if !seen.contains_key(&fpath) {
            return false; // extraneous file in zip
        }
        let is_symlink = entry.is_symlink();

        let disk_data = if is_symlink {
            match fs::read_link(&fpath) {
                Ok(link) => link.as_os_str().as_encoded_bytes().to_vec(),
                Err(_) => return false,
            }
        } else {
            match fs::read(&fpath) {
                Ok(d) => d,
                Err(_) => return false,
            }
        };

        let mut zip_data = Vec::new();
        if entry.read_to_end(&mut zip_data).is_err() {
            return false;
        }
        if disk_data != zip_data {
            return false;
        }
        seen.insert(fpath, true);
    }

    for (f, found) in &seen {
        if !found {
            if fs::metadata(f).map(|m| m.is_dir()).unwrap_or(false) {
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

/// Lexically clean a slash-separated relative path, collapsing `.` and
/// resolving `..` without touching the filesystem.
fn clean_slash(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&last) if last != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    if out.is_empty() {
        ".".to_string()
    } else {
        out.join("/")
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

/// Apply unix permission bits to `path`, best-effort. A no-op on non-unix
/// platforms, which have no mode.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Create a symlink at `link` pointing to `target`.
#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
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
        let got = extract_archive(&zip_path, &dst).unwrap();
        assert_eq!(got, meta);
        assert_eq!(fs::read(dst.join("out.txt")).unwrap(), b"hello");
    }

    #[test]
    fn extract_rejects_traversal() {
        let dst = tempdir();
        let err = extract_zip_entry(&dst, "../escape", false, false, None, b"x");
        assert!(err.is_err());
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
