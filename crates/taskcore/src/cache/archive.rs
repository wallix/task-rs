//! The classic (non-OCI) remote build cache: a zip archive of a task's
//! generated files, with the cache metadata stored as newline-separated
//! `key:value` pairs in the zip comment.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cap_primitives::fs::{self as capfs, FollowSymlinks};
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

/// Longest symlink target an entry may carry: `PATH_MAX` on Linux. A link's
/// body is read into memory, so it gets a bound of its own.
const MAX_LINK_TARGET: u64 = 4096;

/// The permission bits an extracted entry may keep: setuid, setgid and sticky
/// are dropped, so a restored file never runs with its owner's privileges.
const MODE_MASK: u32 = 0o777;

/// The modes given an entry that records none, as the writer would record them
/// for a file or directory created under the usual `022` umask.
const DEFAULT_FILE_MODE: u32 = 0o644;
const DEFAULT_DIR_MODE: u32 = 0o755;

/// What a restore may write. The archive may come from a shared remote cache,
/// so it is untrusted: every entry is checked against the policy before any is
/// written, and a refusal leaves the tree untouched.
///
/// Neither the entry count nor the total size is bounded: a legitimate cache
/// can be arbitrarily large, so an archive can fill the disk up to the sizes
/// its entries declare (never past them).
#[derive(Default)]
pub struct RestorePolicy<'a> {
    /// Checks the metadata in the comment; an error refuses the archive.
    pub check_meta: Option<&'a MetaCheck<'a>>,
    /// Whether the entry at this root-relative, slash-separated path may be
    /// written. Entries naming a `.git` component are refused regardless.
    pub accept: Option<&'a dyn Fn(&str) -> bool>,
}

/// A check on an archive's comment metadata.
pub type MetaCheck<'a> = dyn Fn(&CacheMeta) -> Result<(), CacheError> + 'a;

/// The plain components of an entry name, slash- or backslash-separated, with
/// `.` and empty segments dropped. Refuses a name that is absolute, has a `..`
/// component, or a component the platform reads as a prefix or root (`C:` on
/// Windows), so joining the result onto the extraction root cannot leave it.
/// `..` is refused rather than resolved: the kernel resolves it after any
/// symlink before it, which a lexical clean does not see. A `.git` component
/// is refused too, so no restore can plant hooks or rewrite a repository's
/// configuration, as is, on Windows, a name the platform aliases (see
/// [`windows_aliased`]).
fn entry_components(name: &str) -> Result<Vec<&str>, CacheError> {
    let outside = || CacheError::msg(format!("entry {name:?} is outside the extraction root"));
    if name.starts_with(['/', '\\']) {
        return Err(outside());
    }
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(outside()),
            _ => {
                let mut comps = Path::new(part).components();
                if !matches!(
                    (comps.next(), comps.next()),
                    (Some(std::path::Component::Normal(_)), None)
                ) {
                    return Err(outside());
                }
                if part.eq_ignore_ascii_case(".git") {
                    return Err(CacheError::msg(format!(
                        "entry {name:?} is inside a .git directory"
                    )));
                }
                if cfg!(windows) && windows_aliased(part) {
                    return Err(CacheError::msg(format!(
                        "entry {name:?}: {part:?} is not a plain Windows file name"
                    )));
                }
                parts.push(part);
            }
        }
    }
    if parts.is_empty() {
        return Err(CacheError::msg(format!("entry {name:?} names no path")));
    }
    Ok(parts)
}

/// Whether Windows would read path component `part` as something other than
/// the file it names: a device (`CON`, `NUL`, `COM1`, … with any extension), an
/// alternate data stream (`:`), or a name it silently trims (trailing dot or
/// space) and so aliases with another. Mirrors Go's `filepath.IsLocal`.
fn windows_aliased(part: &str) -> bool {
    if part.contains(':') || part.ends_with(['.', ' ']) {
        return true;
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut chars = stem.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let digit: String = chars.collect();
    (prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
        && matches!(
            digit.as_str(),
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"
        )
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
        let parts: Vec<String> = entry_components(&name)?
            .into_iter()
            .map(str::to_string)
            .collect();
        if let Some(accept) = policy.accept
            && !accept(&parts.join("/"))
        {
            return Err(CacheError::rejected(format!(
                "entry {name:?} is not one of the files this restore may write"
            )));
        }
        planned.push(Planned {
            index,
            name,
            parts,
            kind,
            mode,
            size,
        });
    }
    // An entry below a symlink entry would be written through the link.
    let links: std::collections::BTreeSet<&[String]> = planned
        .iter()
        .filter(|p| p.kind == Kind::Symlink)
        .map(|p| p.parts.as_slice())
        .collect();
    for p in &planned {
        for depth in 1..p.parts.len() {
            if let Some(prefix) = p.parts.get(..depth)
                && let Some(part) = prefix.last()
                && links.contains(prefix)
            {
                return Err(CacheError::msg(format!(
                    "entry {:?}: parent {part:?} is a symlink",
                    p.name
                )));
            }
        }
    }
    Ok(planned)
}

/// Open the directory `parents` names below `root`, one component at a time
/// and never through a symlink, creating missing ones when `create` is set.
/// Every later operation is relative to the returned handle, so a directory
/// swapped for a link after this walk cannot redirect it.
fn open_parent(root: &fs::File, parents: &[String], create: bool) -> Result<fs::File, CacheError> {
    let mut dir = root.try_clone()?;
    for part in parents {
        let part_path = Path::new(part);
        match capfs::stat(&dir, part_path, FollowSymlinks::No) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(CacheError::msg(format!("parent {part:?} is a symlink")));
            }
            Ok(_) => {}
            Err(e) if create && e.kind() == std::io::ErrorKind::NotFound => {
                capfs::create_dir(&dir, part_path, &capfs::DirOptions::new())?;
            }
            Err(e) => return Err(e.into()),
        }
        dir = capfs::open_dir_nofollow(&dir, part_path)?;
    }
    Ok(dir)
}

/// Open `leaf` in `dir` for reading, failing rather than following it if it
/// is a symlink; with `directory`, failing too if it is not a directory.
#[cfg(unix)]
fn open_nofollow(dir: &fs::File, leaf: &Path, directory: bool) -> std::io::Result<fs::File> {
    use capfs::OpenOptionsExt;
    let flags = libc::O_NOFOLLOW | if directory { libc::O_DIRECTORY } else { 0 };
    capfs::open(
        dir,
        leaf,
        capfs::OpenOptions::new().read(true).custom_flags(flags),
    )
}

/// Without `O_NOFOLLOW`, the open stays confined to `dir` by the handle-relative
/// resolution; the callers check the type through the handle.
#[cfg(not(unix))]
fn open_nofollow(dir: &fs::File, leaf: &Path, _directory: bool) -> std::io::Result<fs::File> {
    capfs::open(dir, leaf, capfs::OpenOptions::new().read(true))
}

/// The type of whatever `leaf` names in `dir`, without following a link.
fn leaf_type(dir: &fs::File, leaf: &Path) -> Result<Option<capfs::FileType>, CacheError> {
    match capfs::stat(dir, leaf, FollowSymlinks::No) {
        Ok(m) => Ok(Some(m.file_type())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove the file or symlink `leaf` names in `dir`, so the entry written next
/// gets a fresh inode rather than an existing one (perhaps hard-linked
/// elsewhere). A directory there is an error: the entry cannot replace it.
fn clear_leaf(dir: &fs::File, leaf: &Path, name: &str) -> Result<(), CacheError> {
    match leaf_type(dir, leaf)? {
        None => Ok(()),
        Some(t) if t.is_dir() => Err(CacheError::msg(format!(
            "entry {name:?}: a directory is in the way"
        ))),
        Some(_) => Ok(capfs::remove_file(dir, leaf)?),
    }
}

/// Whether `disk` holds exactly what `entry` decompresses to. Streams both
/// through fixed buffers.
fn same_stream(mut disk: impl Read, mut entry: impl Read) -> Result<bool, CacheError> {
    let mut want = [0u8; 8192];
    let mut have = [0u8; 8192];
    loop {
        let n = entry.read(&mut want)?;
        if n == 0 {
            // The entry ended: the file matches only if it did too.
            return Ok(disk.read(&mut have[..1])? == 0);
        }
        let got = have
            .get_mut(..n)
            .ok_or_else(|| CacheError::msg("short read"))?;
        if disk.read_exact(got).is_err() || got != want.get(..n).unwrap_or_default() {
            return Ok(false);
        }
    }
}

/// Whether the regular file `leaf` in `dir` can stay as it is: its only link,
/// already `mode`, `size` bytes long and identical to `entry`. Never on a
/// platform without link counts and modes; the entry is then rewritten.
#[cfg(unix)]
fn unchanged(
    dir: &fs::File,
    leaf: &Path,
    mode: u32,
    size: u64,
    entry: impl Read,
) -> Result<bool, CacheError> {
    use std::os::unix::fs::MetadataExt;
    let file = open_nofollow(dir, leaf, false)?;
    let md = file.metadata()?;
    if !md.is_file() || md.nlink() != 1 || md.len() != size || md.mode() & 0o7777 != mode {
        return Ok(false);
    }
    same_stream(std::io::BufReader::new(file), entry)
}

#[cfg(not(unix))]
fn unchanged(
    _dir: &fs::File,
    _leaf: &Path,
    _mode: u32,
    _size: u64,
    _entry: impl Read,
) -> Result<bool, CacheError> {
    Ok(false)
}

/// Copy `entry` into `out`, refusing one that inflates past the `size` its
/// header declared.
fn copy_entry(
    name: &str,
    size: u64,
    entry: impl Read,
    out: &mut fs::File,
) -> Result<(), CacheError> {
    let written = std::io::copy(&mut entry.take(size.saturating_add(1)), out)?;
    if written > size {
        return Err(CacheError::msg(format!(
            "entry {name:?} is larger than its declared {size} bytes"
        )));
    }
    Ok(())
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

/// Write one planned entry under the directory handle `parent`. An existing
/// file or symlink at the destination is replaced by a fresh one, never
/// written or chmodded through; a file is left alone only when it is already
/// identical, mode included, and linked nowhere else.
fn write_entry<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    p: &Planned,
    parent: &fs::File,
    leaf: &Path,
    base_dir: &Path,
) -> Result<(), CacheError> {
    match p.kind {
        Kind::Dir => {
            match leaf_type(parent, leaf)? {
                Some(t) if t.is_dir() => {}
                Some(_) => {
                    capfs::remove_file(parent, leaf)?;
                    capfs::create_dir(parent, leaf, &capfs::DirOptions::new())?;
                }
                None => capfs::create_dir(parent, leaf, &capfs::DirOptions::new())?,
            }
            Ok(())
        }
        Kind::Symlink => {
            let target = link_target(&p.name, archive.by_index(p.index)?)?;
            if leaf_type(parent, leaf)?.is_some_and(|t| t.is_symlink())
                && capfs::read_link_contents(parent, leaf)
                    .is_ok_and(|l| l.as_os_str().as_encoded_bytes() == target)
            {
                return Ok(());
            }
            clear_leaf(parent, leaf, &p.name)?;
            make_symlink(&target, parent, leaf, base_dir, &p.parts)?;
            Ok(())
        }
        Kind::File => {
            if leaf_type(parent, leaf)?.is_some_and(|t| t.is_file())
                && unchanged(parent, leaf, p.mode, p.size, archive.by_index(p.index)?)?
            {
                return Ok(());
            }
            clear_leaf(parent, leaf, &p.name)?;
            // `create_new` fails on anything at `leaf`, a symlink included, so
            // the file is never opened through a link swapped in meanwhile.
            let mut out = capfs::open(
                parent,
                leaf,
                capfs::OpenOptions::new().write(true).create_new(true),
            )?;
            copy_entry(&p.name, p.size, archive.by_index(p.index)?, &mut out)?;
            set_file_mode(&out, p.mode)?;
            Ok(())
        }
    }
}

/// Extract every entry of the archive at `zip_path` into `base_dir`, as
/// `policy` allows, and return the parsed comment [`CacheMeta`]. The comment
/// and every entry name are checked before anything is written; a failure
/// while writing aborts and is returned.
///
/// Names that escape the root, or whose parent chain crosses a symlink, are
/// refused. Every operation is relative to a directory handle opened without
/// following links from a handle on `base_dir`, so a concurrent swap cannot
/// redirect it either. Permission bits are applied, without setuid, setgid or
/// sticky, through the entry's own handle; directories get theirs last, so a
/// read-only one does not stop its contents from being restored.
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

    let root = capfs::open_ambient_dir(base_dir, cap_primitives::ambient_authority())?;
    let mut cached: Option<(Vec<String>, fs::File)> = None;
    let mut dirs = Vec::new();
    for p in &planned {
        let Some((leaf, parents)) = p.parts.split_last() else {
            continue;
        };
        let parent = match cached.take() {
            Some((names, dir)) if names.as_slice() == parents => dir,
            _ => open_parent(&root, parents, true)
                .map_err(|e| CacheError::msg(format!("entry {:?}: {e}", p.name)))?,
        };
        write_entry(&mut archive, p, &parent, Path::new(leaf), base_dir)?;
        if p.kind == Kind::Dir {
            dirs.push(p);
        }
        cached = Some((parents.to_vec(), parent));
    }
    // Innermost first, so a directory losing its write bit comes after its
    // children have been written.
    for p in dirs.iter().rev() {
        let Some((leaf, parents)) = p.parts.split_last() else {
            continue;
        };
        let parent = open_parent(&root, parents, false)?;
        let dir = open_nofollow(&parent, Path::new(leaf), true)?;
        set_file_mode(&dir, p.mode)?;
    }
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
    let Ok(root) = capfs::open_ambient_dir(base_dir, cap_primitives::ambient_authority()) else {
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
        let Some((leaf, parents)) = parts.split_last() else {
            return false;
        };
        let parents: Vec<String> = parents.iter().map(|s| (*s).to_string()).collect();
        let Ok(dir) = open_parent(&root, &parents, false) else {
            return false;
        };
        if !entry_matches_disk(&dir, Path::new(leaf), is_symlink, size, entry).unwrap_or(false) {
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

/// Whether `leaf` in `dir` is what `entry` holds: a symlink with its target,
/// or a regular file of its size and content.
fn entry_matches_disk(
    dir: &fs::File,
    leaf: &Path,
    is_symlink: bool,
    size: u64,
    entry: impl Read,
) -> Result<bool, CacheError> {
    let Some(kind) = leaf_type(dir, leaf)? else {
        return Ok(false);
    };
    if is_symlink {
        if !kind.is_symlink() {
            return Ok(false);
        }
        let target = link_target("", entry)?;
        let link = capfs::read_link_contents(dir, leaf)?;
        return Ok(link.as_os_str().as_encoded_bytes() == target);
    }
    if !kind.is_file() {
        return Ok(false);
    }
    let file = open_nofollow(dir, leaf, false)?;
    let md = file.metadata()?;
    if !md.is_file() || md.len() != size {
        return Ok(false);
    }
    same_stream(std::io::BufReader::new(file), entry)
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

/// Apply unix permission bits through an open handle, which no symlink can
/// redirect. A no-op on non-unix platforms, which have no mode.
#[cfg(unix)]
fn set_file_mode(file: &fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &fs::File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Create `leaf` in `dir` as a symlink to `target`, its raw bytes.
#[cfg(unix)]
fn make_symlink(
    target: &[u8],
    dir: &fs::File,
    leaf: &Path,
    _base_dir: &Path,
    _parts: &[String],
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    capfs::symlink_contents(std::ffi::OsStr::from_bytes(target), dir, leaf)
}

/// Create the symlink by path: the handle-relative call refuses an absolute
/// target, which a cached link may legitimately have. The parents were just
/// walked without following links; only a concurrent swap could redirect it.
#[cfg(windows)]
fn make_symlink(
    target: &[u8],
    _dir: &fs::File,
    _leaf: &Path,
    base_dir: &Path,
    parts: &[String],
) -> std::io::Result<()> {
    let path: PathBuf = std::iter::once(base_dir.to_path_buf())
        .chain(parts.iter().map(PathBuf::from))
        .collect();
    std::os::windows::fs::symlink_file(String::from_utf8_lossy(target).as_ref(), path)
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
    fn extract_refuses_git_entries() {
        for name in [".git/hooks/pre-commit", "sub/.GIT/config"] {
            let dst = tempdir();
            let bytes = zip_bytes(|zw| add_file(zw, name, b"x"));
            assert!(extract_bytes(&bytes, &dst).is_err(), "{name:?} accepted");
        }
    }

    #[test]
    fn windows_aliased_names() {
        for part in [
            "CON",
            "con.txt",
            "Nul",
            "aux.tar.gz",
            "COM1",
            "lpt9.log",
            "COM\u{b9}",
            "CONIN$",
            "a.",
            "a ",
            "file:stream",
            "c:",
        ] {
            assert!(windows_aliased(part), "{part:?}");
        }
        for part in ["CONSOLE", "COM10", "com", "LPT0", "a.b", "nul_", ".hidden"] {
            assert!(!windows_aliased(part), "{part:?}");
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
