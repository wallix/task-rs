//! Writing an untrusted file set under a directory without leaving it.
//!
//! A restored set comes from a shared cache, so its names, links and modes are
//! attacker-controlled. [`components`] refuses every name that could resolve
//! outside the root, and [`check_link_parents`] every entry the set would
//! write through one of its own symlinks. [`Tree`] then performs each
//! operation relative to a directory handle reached from a handle on the root
//! one component at a time, never through a symlink, so neither a link on disk
//! nor one swapped in concurrently can redirect it. Files are always created
//! as fresh inodes (an existing file, perhaps hard-linked elsewhere, is
//! unlinked first, never truncated or chmodded), and modes lose setuid,
//! setgid and sticky.
//!
//! Neither the entry count nor the total size is bounded here: a legitimate
//! set can be arbitrarily large, so a restore can fill the disk up to the
//! sizes the set declares, never past them.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use cap_primitives::fs::{self as capfs, FollowSymlinks};

use crate::error::{Error, Result};

/// Longest symlink target an entry may carry: `PATH_MAX` on Linux.
pub const MAX_LINK_TARGET: u64 = 4096;

/// Longest entry name, and longest component of one, [`components`] accepts.
pub const MAX_NAME: usize = 4096;
pub const MAX_COMPONENT: usize = 255;

/// The permission bits a restored entry may keep: setuid, setgid and sticky
/// are dropped, so a restored file never runs with its owner's privileges.
pub const MODE_MASK: u32 = 0o777;

/// The modes given an entry that records none, as a writer would record them
/// for a file or directory created under the usual `022` umask.
pub const DEFAULT_FILE_MODE: u32 = 0o644;
pub const DEFAULT_DIR_MODE: u32 = 0o755;

/// The plain components of entry name `name`, with `.` and empty segments
/// dropped. `/` separates; so does `\` when `backslash_separates`, and
/// otherwise a `\` is refused, as Windows would read it as a separator.
///
/// Refuses a name that is absolute, has a `..` component, a NUL, or a
/// component the platform reads as a prefix or root (`C:` on Windows), so
/// joining the result onto the root cannot leave it. `..` is refused rather
/// than resolved: the kernel resolves it after any symlink before it, which a
/// lexical clean does not see. On Windows a name the platform aliases is
/// refused too (see [`windows_aliased`]).
pub fn components(name: &str, backslash_separates: bool) -> Result<Vec<String>> {
    // `PATH_MAX` and `NAME_MAX` on Linux: nothing longer can be restored, and
    // a policy matches names against patterns, so they get a bound.
    if name.len() > MAX_NAME {
        return Err(Error::format(format!(
            "entry name is longer than {MAX_NAME} bytes"
        )));
    }
    let outside = || Error::format(format!("entry {name:?} is outside the extraction root"));
    if name.starts_with(['/', '\\']) || name.contains('\0') {
        return Err(outside());
    }
    if !backslash_separates && name.contains('\\') {
        return Err(Error::format(format!(
            "entry {name:?} contains a backslash"
        )));
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
                if part.len() > MAX_COMPONENT {
                    return Err(Error::format(format!(
                        "entry {name:?}: a component is longer than {MAX_COMPONENT} bytes"
                    )));
                }
                if cfg!(windows) && windows_aliased(part) {
                    return Err(Error::format(format!(
                        "entry {name:?}: {part:?} is not a plain Windows file name"
                    )));
                }
                parts.push(part.to_string());
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::format(format!("entry {name:?} names no path")));
    }
    Ok(parts)
}

/// Whether Windows would read path component `part` as something other than
/// the file it names: a device (`CON`, `NUL`, `COM1`, … with any extension), an
/// alternate data stream (`:`), or a name it silently trims (trailing dot or
/// space) and so aliases with another. Mirrors Go's `filepath.IsLocal`.
pub fn windows_aliased(part: &str) -> bool {
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

/// Refuse a set in which an entry lies below one of the set's symlinks: it
/// would be written through the link. `entries` yields each entry's
/// components and whether it is a symlink.
pub fn check_link_parents<'a>(
    entries: impl Iterator<Item = (&'a [String], bool)> + Clone,
) -> Result<()> {
    let links: BTreeSet<&[String]> = entries
        .clone()
        .filter(|(_, link)| *link)
        .map(|(parts, _)| parts)
        .collect();
    for (parts, _) in entries {
        for depth in 1..parts.len() {
            if let Some(prefix) = parts.get(..depth)
                && let Some(part) = prefix.last()
                && links.contains(prefix)
            {
                return Err(Error::format(format!(
                    "entry {:?}: parent {part:?} is a symlink",
                    parts.join("/")
                )));
            }
        }
    }
    Ok(())
}

/// A directory tree being restored or compared, every operation confined to
/// it (see the module docs).
pub struct Tree {
    root: File,
    base: PathBuf,
    /// The parent handle of the last entry, reused while entries share it.
    cached: Option<(Vec<String>, File)>,
    /// Directory entries and their modes, applied by [`Tree::finish`].
    dirs: Vec<(Vec<String>, u32)>,
}

impl Tree {
    /// Open the tree rooted at `base`, which must exist.
    pub fn open(base: &Path) -> Result<Self> {
        Ok(Self {
            root: capfs::open_ambient_dir(base, cap_primitives::ambient_authority())?,
            base: base.to_path_buf(),
            cached: None,
            dirs: Vec::new(),
        })
    }

    /// The handle on the directory holding entry `parts`, walked from the root
    /// without following links, creating missing directories when `create`.
    fn parent(&mut self, parts: &[String], create: bool) -> Result<(File, PathBuf)> {
        let Some((leaf, parents)) = parts.split_last() else {
            return Err(Error::format("entry names no path"));
        };
        let dir = match self.cached.take() {
            Some((names, dir)) if names.as_slice() == parents => dir,
            _ => open_parent(&self.root, parents, create)
                .map_err(|e| Error::format(format!("entry {:?}: {}", parts.join("/"), msg(&e))))?,
        };
        let held = dir.try_clone()?;
        self.cached = Some((parents.to_vec(), held));
        Ok((dir, PathBuf::from(leaf)))
    }

    /// Restore directory entry `parts`. Its `mode` is applied by
    /// [`Tree::finish`], once everything below it is written.
    pub fn dir(&mut self, parts: &[String], mode: u32) -> Result<()> {
        let (parent, leaf) = self.parent(parts, true)?;
        match leaf_type(&parent, &leaf)? {
            Some(t) if t.is_dir() => {}
            Some(_) => {
                capfs::remove_file(&parent, &leaf)?;
                capfs::create_dir(&parent, &leaf, &capfs::DirOptions::new())?;
            }
            None => capfs::create_dir(&parent, &leaf, &capfs::DirOptions::new())?,
        }
        self.dirs.push((parts.to_vec(), mode & MODE_MASK));
        Ok(())
    }

    /// Restore symlink entry `parts` pointing at `target` (raw bytes), leaving
    /// an identical link alone and replacing a file or link.
    pub fn symlink(&mut self, parts: &[String], target: &[u8]) -> Result<()> {
        if target.len() as u64 > MAX_LINK_TARGET {
            return Err(Error::format(format!(
                "entry {:?}: symlink target is longer than {MAX_LINK_TARGET} bytes",
                parts.join("/")
            )));
        }
        let (parent, leaf) = self.parent(parts, true)?;
        if leaf_type(&parent, &leaf)?.is_some_and(|t| t.is_symlink())
            && capfs::read_link_contents(&parent, &leaf)
                .is_ok_and(|l| l.as_os_str().as_encoded_bytes() == target)
        {
            return Ok(());
        }
        clear_leaf(&parent, &leaf, parts)?;
        make_symlink(target, &parent, &leaf, &self.base, parts)?;
        Ok(())
    }

    /// Whether regular-file entry `parts` can stay as it is on disk: its only
    /// link, already `mode`, `size` bytes long and identical to `data`. Never
    /// on a platform without link counts and modes.
    pub fn unchanged(
        &mut self,
        parts: &[String],
        mode: u32,
        size: u64,
        data: impl Read,
    ) -> Result<bool> {
        let (parent, leaf) = self.parent(parts, true)?;
        if !leaf_type(&parent, &leaf)?.is_some_and(|t| t.is_file()) {
            return Ok(false);
        }
        unchanged(&parent, &leaf, mode & MODE_MASK, size, data)
    }

    /// Restore regular-file entry `parts` as a fresh inode holding `data`,
    /// which must not run past `size` bytes, with permission bits `mode`.
    pub fn file(&mut self, parts: &[String], mode: u32, size: u64, data: impl Read) -> Result<()> {
        let (parent, leaf) = self.parent(parts, true)?;
        clear_leaf(&parent, &leaf, parts)?;
        // `create_new` fails on anything at `leaf`, a symlink included, so the
        // file is never opened through a link swapped in meanwhile.
        let mut out = capfs::open(
            &parent,
            &leaf,
            capfs::OpenOptions::new().write(true).create_new(true),
        )?;
        let written = std::io::copy(&mut data.take(size.saturating_add(1)), &mut out)?;
        if written > size {
            return Err(Error::format(format!(
                "entry {:?} is larger than its declared {size} bytes",
                parts.join("/")
            )));
        }
        set_mode(&out, mode & MODE_MASK)?;
        Ok(())
    }

    /// Apply the directory entries' modes, innermost first, so a directory
    /// losing its write bit comes after its children were written.
    pub fn finish(mut self) -> Result<()> {
        let dirs = std::mem::take(&mut self.dirs);
        for (parts, mode) in dirs.iter().rev() {
            let (parent, leaf) = self.parent(parts, false)?;
            let dir = open_nofollow(&parent, &leaf, true)?;
            set_mode(&dir, *mode)?;
        }
        Ok(())
    }

    /// Whether entry `parts` on disk is what the set holds: a symlink to the
    /// target `data` reads to, or a regular file of `size` bytes reading the
    /// same as `data`. Any failure to reach it confined reads as a mismatch.
    pub fn matches(
        &mut self,
        parts: &[String],
        is_symlink: bool,
        size: u64,
        data: impl Read,
    ) -> bool {
        let Ok((parent, leaf)) = self.parent(parts, false) else {
            return false;
        };
        matches_disk(&parent, &leaf, is_symlink, size, data).unwrap_or(false)
    }

    /// Whether entry `parts` is a directory, reached without following links.
    pub fn is_dir(&mut self, parts: &[String]) -> bool {
        let Ok((parent, leaf)) = self.parent(parts, false) else {
            return false;
        };
        leaf_type(&parent, &leaf).is_ok_and(|t| t.is_some_and(|t| t.is_dir()))
    }
}

/// The text of a restore error without the crate prefix [`Error`]'s `Display`
/// adds, for nesting in another message.
fn msg(e: &Error) -> String {
    match e {
        Error::Format(m) | Error::Network(m) => m.clone(),
        Error::Io(e) => e.to_string(),
        Error::Json(e) => e.to_string(),
    }
}

/// Open the directory `parents` names below `root`, one component at a time
/// and never through a symlink, creating missing ones when `create` is set.
pub(crate) fn open_parent(root: &File, parents: &[String], create: bool) -> Result<File> {
    let mut dir = root.try_clone()?;
    for part in parents {
        let part_path = Path::new(part);
        match capfs::stat(&dir, part_path, FollowSymlinks::No) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error::format(format!("parent {part:?} is a symlink")));
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
pub(crate) fn open_nofollow(dir: &File, leaf: &Path, directory: bool) -> std::io::Result<File> {
    use capfs::OpenOptionsExt;
    let flags = libc::O_NOFOLLOW | if directory { libc::O_DIRECTORY } else { 0 };
    capfs::open(
        dir,
        leaf,
        capfs::OpenOptions::new().read(true).custom_flags(flags),
    )
}

/// The same through cap's own no-follow option (the `cap-fs-ext` extension's
/// hook); the callers check the type through the handle.
#[cfg(not(unix))]
pub(crate) fn open_nofollow(dir: &File, leaf: &Path, _directory: bool) -> std::io::Result<File> {
    capfs::open(
        dir,
        leaf,
        capfs::OpenOptions::new()
            .read(true)
            ._cap_fs_ext_follow(FollowSymlinks::No),
    )
}

/// The type of whatever `leaf` names in `dir`, without following a link.
pub(crate) fn leaf_type(dir: &File, leaf: &Path) -> Result<Option<capfs::FileType>> {
    match capfs::stat(dir, leaf, FollowSymlinks::No) {
        Ok(m) => Ok(Some(m.file_type())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove the file or symlink `leaf` names in `dir`, so the entry written next
/// gets a fresh inode. A directory there is an error: the entry cannot
/// replace it.
fn clear_leaf(dir: &File, leaf: &Path, parts: &[String]) -> Result<()> {
    match leaf_type(dir, leaf)? {
        None => Ok(()),
        Some(t) if t.is_dir() => Err(Error::format(format!(
            "entry {:?}: a directory is in the way",
            parts.join("/")
        ))),
        Some(_) => Ok(capfs::remove_file(dir, leaf)?),
    }
}

/// Whether `disk` holds exactly what `data` reads to, through fixed buffers.
fn same_stream(mut disk: impl Read, mut data: impl Read) -> Result<bool> {
    let mut want = [0u8; 8192];
    let mut have = [0u8; 8192];
    loop {
        let n = data.read(&mut want)?;
        if n == 0 {
            // The entry ended: the file matches only if it did too.
            return Ok(disk.read(&mut have[..1])? == 0);
        }
        let got = have
            .get_mut(..n)
            .ok_or_else(|| Error::format("short read"))?;
        if disk.read_exact(got).is_err() || got != want.get(..n).unwrap_or_default() {
            return Ok(false);
        }
    }
}

#[cfg(unix)]
fn unchanged(dir: &File, leaf: &Path, mode: u32, size: u64, data: impl Read) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let file = open_nofollow(dir, leaf, false)?;
    let md = file.metadata()?;
    if !md.is_file() || md.nlink() != 1 || md.len() != size || md.mode() & 0o7777 != mode {
        return Ok(false);
    }
    same_stream(std::io::BufReader::new(file), data)
}

#[cfg(not(unix))]
fn unchanged(_dir: &File, _leaf: &Path, _mode: u32, _size: u64, _data: impl Read) -> Result<bool> {
    Ok(false)
}

fn matches_disk(
    dir: &File,
    leaf: &Path,
    is_symlink: bool,
    size: u64,
    data: impl Read,
) -> Result<bool> {
    let Some(kind) = leaf_type(dir, leaf)? else {
        return Ok(false);
    };
    if is_symlink {
        if !kind.is_symlink() {
            return Ok(false);
        }
        let mut target = Vec::new();
        data.take(MAX_LINK_TARGET + 1).read_to_end(&mut target)?;
        let link = capfs::read_link_contents(dir, leaf)?;
        return Ok(link.as_os_str().as_encoded_bytes() == target);
    }
    if !kind.is_file() {
        return Ok(false);
    }
    let file = open_nofollow(dir, leaf, false)?;
    let md = file.metadata()?;
    if !md.is_file() || md.len() != size || !single_link(&md) {
        return Ok(false);
    }
    same_stream(std::io::BufReader::new(file), data)
}

/// Whether the file `md` describes has no other name, so reading it reads
/// nothing a hard link shares from outside. Unknown, so false, off unix.
#[cfg(unix)]
fn single_link(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    md.nlink() == 1
}

#[cfg(not(unix))]
fn single_link(_md: &std::fs::Metadata) -> bool {
    false
}

/// Apply unix permission bits through an open handle, which no symlink can
/// redirect. A no-op on non-unix platforms, which have no mode.
#[cfg(unix)]
fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_file: &File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Create `leaf` in `dir` as a symlink to `target`, its raw bytes.
#[cfg(unix)]
fn make_symlink(
    target: &[u8],
    dir: &File,
    leaf: &Path,
    _base: &Path,
    _parts: &[String],
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    capfs::symlink_contents(std::ffi::OsStr::from_bytes(target), dir, leaf)
}

/// Refused off unix: Windows has no handle-relative way to create a link (cap
/// itself rebuilds a path from the handle), so a directory swapped in after
/// the walk could redirect it. A set holding a symlink misses there instead.
#[cfg(not(unix))]
fn make_symlink(
    _target: &[u8],
    _dir: &File,
    _leaf: &Path,
    _base: &Path,
    parts: &[String],
) -> std::io::Result<()> {
    Err(std::io::Error::other(format!(
        "entry {:?}: symlinks are not restored on this platform",
        parts.join("/")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_refuses_names_that_leave_the_root() {
        for name in [
            "", ".", "./", "..", "../x", "a/../b", "a/..", "/abs", "\\abs", "x\0", "..\0/x",
        ] {
            assert!(components(name, true).is_err(), "{name:?}");
            assert!(components(name, false).is_err(), "{name:?}");
        }
        assert!(components("a\\..\\x", true).is_err());
        assert!(components("a\\b", false).is_err());
        assert_eq!(components("a\\b", true).unwrap(), ["a", "b"]);
        assert_eq!(components("./a//b/", true).unwrap(), ["a", "b"]);
        assert!(components(&"a".repeat(256), false).is_err());
        assert!(components(&"a".repeat(255), false).is_ok());
        assert!(components(&"ab/".repeat(1366), false).is_err());
        assert!(components(&"ab/".repeat(1365), false).is_ok());
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

    #[test]
    fn link_parents_are_refused_in_either_order() {
        let p = |s: &str| components(s, false).unwrap();
        let (a, ab) = (p("a"), p("a/b"));
        for set in [[(&a, true), (&ab, false)], [(&ab, false), (&a, true)]] {
            let it = set.iter().map(|(parts, link)| (parts.as_slice(), *link));
            let err = check_link_parents(it).unwrap_err();
            assert!(err.to_string().contains("is a symlink"), "{err}");
        }
        let it = [(&a, false), (&ab, false)]
            .into_iter()
            .map(|(parts, link)| (parts.as_slice(), link));
        assert!(check_link_parents(it).is_ok());
    }
}
