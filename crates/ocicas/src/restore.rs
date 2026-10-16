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

/// Longest entry name, and longest component of one, [`components`] accepts,
/// in the platform's units ([`name_len`]): `PATH_MAX` and `NAME_MAX` bytes on
/// unix; on Windows 255 UTF-16 units a component and its long-path limit.
#[cfg(not(windows))]
pub const MAX_NAME: usize = 4096;
#[cfg(windows)]
pub const MAX_NAME: usize = 32767;
pub const MAX_COMPONENT: usize = 255;

/// The length of `s` in the units the platform limits names in: bytes on
/// unix, UTF-16 code units on Windows.
pub fn name_len(s: &str) -> usize {
    if cfg!(windows) {
        s.encode_utf16().count()
    } else {
        s.len()
    }
}

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
    // Nothing longer can be restored, and a policy matches names against
    // patterns, so they get a bound.
    if name_len(name) > MAX_NAME {
        return Err(Error::format(format!(
            "entry name is longer than the platform allows ({MAX_NAME})"
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
                if name_len(part) > MAX_COMPONENT {
                    return Err(Error::format(format!(
                        "entry {name:?}: a component is longer than the platform allows ({MAX_COMPONENT})"
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

/// Refuse a set naming one destination twice: checks look at one entry per
/// name, while a restore would write each in turn. Names that differ only in
/// case count as one where the filesystem usually ignores case (Windows,
/// macOS).
pub fn check_unique<'a>(entries: impl Iterator<Item = &'a [String]>) -> Result<()> {
    let fold = cfg!(windows) || cfg!(target_os = "macos");
    let mut seen = BTreeSet::new();
    for parts in entries {
        let key: Vec<String> = if fold {
            parts.iter().map(|p| p.to_lowercase()).collect()
        } else {
            parts.to_vec()
        };
        if !seen.insert(key) {
            return Err(Error::format(format!(
                "entry {:?} appears twice",
                parts.join("/")
            )));
        }
    }
    Ok(())
}

/// What [`find_below`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// An entry `hit` accepted, at these components below the root.
    Hit(Vec<String>),
    /// More than the walk's budget of entries: nothing can be said.
    TooMany,
    Nothing,
}

/// Walk the directory `dir` (components below `root`) without following links,
/// visiting at most `budget` entries, for one whose name `hit` accepts.
pub fn find_below(
    root: &File,
    dir: &[String],
    budget: usize,
    hit: impl Fn(&str) -> bool,
) -> Result<Found> {
    let mut todo = vec![dir.to_vec()];
    let mut seen = 0usize;
    while let Some(parts) = todo.pop() {
        let handle = match open_parent(root, &parts, false, &mut Vec::new()) {
            Ok(h) => h,
            Err(Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(Error::Format(_)) => continue, // a link: not descended
            Err(e) => return Err(e),
        };
        for entry in capfs::read_dir(&handle, Path::new("."))? {
            let entry = entry?;
            seen = seen.saturating_add(1);
            if seen > budget {
                return Ok(Found::TooMany);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut child = parts.clone();
            child.push(name.clone());
            if hit(&name) {
                return Ok(Found::Hit(child));
            }
            if entry.file_type()?.is_dir() {
                todo.push(child);
            }
        }
    }
    Ok(Found::Nothing)
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
    /// What this restore wrote, for [`Written::undo`].
    written: Written,
}

/// The device and inode of a file, telling the file a restore created from one
/// put at the same name since. `None` off unix, where every name counts as
/// the restore's own.
type Id = Option<(u64, u64)>;

#[cfg(unix)]
fn id_of(md: &std::fs::Metadata) -> Id {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn id_of(_md: &std::fs::Metadata) -> Id {
    None
}

/// The identity of whatever `leaf` names in `dir`, without following a link.
#[cfg(unix)]
fn leaf_id(dir: &File, leaf: &Path) -> Result<Option<Id>> {
    use capfs::MetadataExt;
    match capfs::stat(dir, leaf, FollowSymlinks::No) {
        Ok(m) => Ok(Some(Some((m.dev(), m.ino())))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
fn leaf_id(dir: &File, leaf: &Path) -> Result<Option<Id>> {
    Ok(leaf_type(dir, leaf)?.map(|_| None))
}

/// A name the restore created, and what it created there.
#[derive(Debug)]
pub(crate) struct Owned {
    parts: Vec<String>,
    id: Id,
}

/// An original the restore moved aside to make room for an entry: `parts`,
/// now named `aside` in the same directory.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Aside {
    parts: Vec<String>,
    aside: String,
    /// The original's identity, so only it is dropped on commit.
    #[serde(skip)]
    id: Id,
}

/// A record, under the temp dir, of the originals a restore moved aside, so
/// that a restore cut short (a crash, a kill) can be cleaned up by the next:
/// see [`recover`]. Held locked while the restore runs.
#[derive(Debug)]
struct Journal {
    file: File,
    path: PathBuf,
}

impl Journal {
    fn create(dir: &Path, base: &Path) -> Result<Self> {
        use std::io::Write as _;
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.journal", aside_name()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.lock()?;
        writeln!(file, "{}", serde_json::to_string(&base.to_string_lossy())?)?;
        Ok(Self { file, path })
    }

    fn record(&mut self, aside: &Aside) -> Result<()> {
        use std::io::Write as _;
        writeln!(self.file, "{}", serde_json::to_string(aside)?)?;
        Ok(self.file.sync_data()?)
    }

    fn close(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Clean up after restores into `journal_dir`'s journals that were cut short:
/// put each original a restore moved aside back where its name is free, or
/// drop it where the name has been filled since. A journal another process
/// holds is a restore still running, and is left alone. Returns what it did,
/// for the caller to report.
pub fn recover(journal_dir: &Path) -> Vec<String> {
    let mut done = Vec::new();
    let Ok(entries) = std::fs::read_dir(journal_dir) else {
        return done;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "journal") {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        if file.try_lock().is_err() {
            continue; // in use
        }
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(&file));
        let base: Option<String> = lines
            .next()
            .and_then(|l| l.ok())
            .and_then(|l| serde_json::from_str(&l).ok());
        if let Some(base) = base
            && let Ok(root) =
                capfs::open_ambient_dir(Path::new(&base), cap_primitives::ambient_authority())
        {
            for aside in lines
                .map_while(|l| l.ok())
                .filter_map(|l| serde_json::from_str::<Aside>(&l).ok())
            {
                let name = aside.parts.join("/");
                let r = in_parent(&root, &aside.parts, |parent, leaf| {
                    let original = Path::new(&aside.aside);
                    if leaf_type(parent, original)?.is_none() {
                        return Ok(());
                    }
                    if leaf_type(parent, leaf)?.is_none() {
                        no_replace_move(parent, original, leaf)?;
                        done.push(format!("put back {name:?} after an interrupted restore"));
                    } else {
                        capfs::remove_file(parent, original)?;
                        done.push(format!(
                            "dropped the original of {name:?} left by an interrupted restore"
                        ));
                    }
                    Ok(())
                });
                if let Err(e) = r {
                    done.push(format!("could not clean up {name:?}: {e}"));
                }
            }
        }
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
    done
}

/// Whether `name` is one a restore gives an original it moves aside.
pub fn is_aside_name(name: &str) -> bool {
    name.starts_with(ASIDE_PREFIX)
}

const ASIDE_PREFIX: &str = ".task-restore-";

/// Move `from` to `to` in `dir`, failing rather than replacing anything at
/// `to`: hard-link, then unlink (a filesystem without hard links falls back
/// to checking `to` is free, then renaming).
fn no_replace_move(dir: &File, from: &Path, to: &Path) -> Result<()> {
    match capfs::hard_link(dir, from, dir, to) {
        Ok(()) => Ok(capfs::remove_file(dir, from)?),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::format(format!("{} is taken", to.display())))
        }
        Err(_) => {
            if leaf_type(dir, to)?.is_some() {
                return Err(Error::format(format!("{} is taken", to.display())));
            }
            Ok(capfs::rename(dir, from, dir, to)?)
        }
    }
}

/// Changes made by a restore. Call [`Written::undo`] after a restore or
/// verification failure, or [`Written::commit`] after successful verification.
/// Until one is called, replaced originals remain renamed beside their entries.
#[derive(Debug, Default)]
pub struct Written {
    base: PathBuf,
    /// Files and links created, in order.
    leaves: Vec<Owned>,
    /// Directories created, in order.
    dirs: Vec<Owned>,
    /// Originals moved aside, in order.
    asides: Vec<Aside>,
    /// Directory entries and their modes, applied on [`Written::commit`]:
    /// before, a restored mode could stop [`Written::undo`].
    dir_modes: Vec<(Vec<String>, u32)>,
    journal: Option<Journal>,
}

impl Written {
    /// Whether the restore changed nothing.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty() && self.dirs.is_empty() && self.asides.is_empty()
    }

    /// Take the restore back: remove what it created (only if each name still
    /// holds what it created there, and directories only once empty), then
    /// put back the originals it moved aside. Through the same confined walk;
    /// best-effort past the first failure, which is returned.
    pub fn undo(self) -> Result<()> {
        let root = capfs::open_ambient_dir(&self.base, cap_primitives::ambient_authority())?;
        let mut first: Option<Error> = None;
        let mut note = |r: Result<()>| {
            if let Err(e) = r
                && first.is_none()
            {
                first = Some(e);
            }
        };
        for owned in self.leaves.iter().rev() {
            note(remove_owned(&root, owned, false));
        }
        for owned in self.dirs.iter().rev() {
            note(remove_owned(&root, owned, true));
        }
        for aside in self.asides.iter().rev() {
            note(put_back(&root, aside));
        }
        // Every original is back (or reported): the journal is done with.
        if first.is_none()
            && let Some(j) = self.journal
        {
            j.close();
        }
        first.map_or(Ok(()), Err)
    }

    /// Keep the restore: apply the restored directories' modes, innermost
    /// first, and drop the originals it moved aside (each only if its name
    /// still holds that original).
    pub fn commit(self) -> Result<()> {
        let root = capfs::open_ambient_dir(&self.base, cap_primitives::ambient_authority())?;
        let mut first = None;
        let mut note = |r: Result<()>| {
            if let Err(e) = r
                && first.is_none()
            {
                first = Some(e);
            }
        };
        for (parts, mode) in self.dir_modes.iter().rev() {
            note(in_parent(&root, parts, |parent, leaf| {
                let dir = open_nofollow(parent, leaf, true)?;
                Ok(set_mode(&dir, *mode)?)
            }));
        }
        for aside in &self.asides {
            note(in_parent(&root, &aside.parts, |dir, _| {
                let name = Path::new(&aside.aside);
                match leaf_id(dir, name)? {
                    Some(id) if id == aside.id => Ok(capfs::remove_file(dir, name)?),
                    _ => Ok(()),
                }
            }));
        }
        if let Some(j) = self.journal {
            j.close();
        }
        first.map_or(Ok(()), Err)
    }
}

/// Run `f` with the handle on the directory holding `parts` and its leaf,
/// reached from `root` without following links. A missing parent is success:
/// there is nothing left to act on.
fn in_parent(
    root: &File,
    parts: &[String],
    f: impl FnOnce(&File, &Path) -> Result<()>,
) -> Result<()> {
    let Some((leaf, parents)) = parts.split_last() else {
        return Ok(());
    };
    match open_parent(root, parents, false, &mut Vec::new()) {
        Ok(dir) => f(&dir, Path::new(leaf)),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove what the restore created at `owned`, if the name still holds it: a
/// file or link, or with `dir` a directory once empty.
fn remove_owned(root: &File, owned: &Owned, dir: bool) -> Result<()> {
    in_parent(root, &owned.parts, |parent, leaf| {
        match leaf_id(parent, leaf)? {
            Some(id) if id == owned.id => {}
            // Gone, or replaced since: not the restore's to remove.
            _ => return Ok(()),
        }
        let removed = if dir {
            capfs::remove_dir(parent, leaf)
        } else {
            capfs::remove_file(parent, leaf)
        };
        match removed {
            // A directory something else has filled since stays.
            Err(e) if dir && e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
            r => Ok(r?),
        }
    })
}

/// Move an original back to its name, never replacing what holds it now.
fn put_back(root: &File, aside: &Aside) -> Result<()> {
    in_parent(root, &aside.parts, |parent, leaf| {
        no_replace_move(parent, Path::new(&aside.aside), leaf).map_err(|_| {
            Error::format(format!(
                "{:?} is taken: its original stays as {:?}",
                aside.parts.join("/"),
                aside.aside
            ))
        })
    })
}

/// A name for an original moved aside: unpredictable, so nothing can be
/// planted at it, and short, so it fits whatever the original's length.
fn aside_name() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let random = std::hash::BuildHasher::hash_one(
        &std::collections::hash_map::RandomState::new(),
        (
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now(),
        ),
    );
    format!("{ASIDE_PREFIX}{random:016x}")
}

/// An entry of a set about to be restored, as [`resolve_link`] sees it.
#[derive(Debug, Clone)]
pub enum Pending {
    Dir,
    File,
    Link(Vec<u8>),
}

/// Where [`resolve_link`] found a link to lead.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// To this path below the root (which may not exist yet).
    Inside(Vec<String>),
    /// Out of the root.
    Outside,
}

/// Most links one resolution follows: Linux's `MAXSYMLINKS`.
const MAX_LINK_HOPS: usize = 40;

/// Resolve a link at `link` (components below `root`) pointing at `target`
/// the way the kernel would once the set is restored: component by component
/// from the link's directory, through the set's own `pending` entries and,
/// for names the set leaves alone, through what is on disk below `root`,
/// following links (at most 40). Returns where it leads. An absolute target,
/// the link's own or one met on the way, leads inside only if it names a
/// path under `base`.
pub fn resolve_link(
    root: &File,
    base: &Path,
    pending: &std::collections::BTreeMap<Vec<String>, Pending>,
    link: &[String],
    target: &[u8],
) -> Result<Resolution> {
    let mut cur: Vec<String> = link
        .split_last()
        .map(|(_, d)| d.to_vec())
        .unwrap_or_default();
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    if !expand(base, &mut cur, &mut queue, target) {
        return Ok(Resolution::Outside);
    }
    let mut hops = 0usize;
    // Whether `cur` is a directory known to be real: the link's own, or one
    // the set or the disk holds. A `..` from anything else (a name nothing
    // holds yet, or a file) depends on what is created there later, perhaps a
    // link, so it counts as leaving.
    let mut known = true;
    while let Some(comp) = queue.pop_front() {
        match comp.as_str() {
            "" | "." => continue,
            ".." => {
                if !known || cur.pop().is_none() {
                    return Ok(Resolution::Outside);
                }
                continue;
            }
            _ => {}
        }
        let mut cand = cur.clone();
        cand.push(comp);
        let node = match pending.get(&cand) {
            Some(p) => Some(p.clone()),
            None if pending
                .range(cand.clone()..)
                .next()
                .is_some_and(|(k, _)| k.len() > cand.len() && k.starts_with(&cand)) =>
            {
                // A directory the restore creates for an entry below it.
                Some(Pending::Dir)
            }
            None => on_disk(root, &cand)?,
        };

        match node {
            Some(Pending::Link(t)) => {
                hops = hops.saturating_add(1);
                if hops > MAX_LINK_HOPS {
                    return Err(Error::format(format!(
                        "link {:?}: more than {MAX_LINK_HOPS} links to follow",
                        link.join("/")
                    )));
                }
                let was = cur.len();
                if !expand(base, &mut cur, &mut queue, &t) {
                    return Ok(Resolution::Outside);
                }
                // An absolute target restarts from the root, a real directory.
                if cur.len() != was {
                    known = true;
                }
            }
            other => {
                known = matches!(other, Some(Pending::Dir));
                cur = cand;
            }
        }
    }
    Ok(Resolution::Inside(cur))
}

/// Queue link target `target`'s components ahead of the rest, moving `cur` to
/// the root for an absolute one. False if it is absolute outside `base`, or
/// not text.
fn expand(
    base: &Path,
    cur: &mut Vec<String>,
    queue: &mut std::collections::VecDeque<String>,
    target: &[u8],
) -> bool {
    let Ok(target) = std::str::from_utf8(target) else {
        return false;
    };
    let path = Path::new(target);
    let rest: Vec<String> = if path.has_root() || path.is_absolute() {
        let Ok(rel) = path.strip_prefix(base) else {
            return false;
        };
        cur.clear();
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    } else {
        target.split('/').map(str::to_string).collect()
    };
    for comp in rest.into_iter().rev() {
        queue.push_front(comp);
    }
    true
}

/// What is at `parts` below `root` on disk: `None` if nothing (or a parent is
/// missing), a link with its target, or a file or directory.
fn on_disk(root: &File, parts: &[String]) -> Result<Option<Pending>> {
    let Some((leaf, parents)) = parts.split_last() else {
        return Ok(Some(Pending::Dir));
    };
    let dir = match open_parent(root, parents, false, &mut Vec::new()) {
        Ok(d) => d,
        Err(Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let leaf = Path::new(leaf);
    Ok(match leaf_type(&dir, leaf)? {
        None => None,
        Some(t) if t.is_symlink() => Some(Pending::Link(
            capfs::read_link_contents(&dir, leaf)?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        )),
        Some(t) if t.is_dir() => Some(Pending::Dir),
        Some(_) => Some(Pending::File),
    })
}

/// Whether directory `parts` below `root` holds an entry named `name`, reached
/// without following links.
pub fn has_child(root: &File, parts: &[String], name: &str) -> bool {
    let mut path = parts.to_vec();
    path.push(name.to_string());
    on_disk(root, &path).is_ok_and(|n| n.is_some())
}

impl Tree {
    /// Open the tree rooted at `base`, which must exist.
    pub fn open(base: &Path) -> Result<Self> {
        Ok(Self {
            root: capfs::open_ambient_dir(base, cap_primitives::ambient_authority())?,
            base: base.to_path_buf(),
            cached: None,
            dirs: Vec::new(),
            written: Written {
                base: base.to_path_buf(),
                ..Written::default()
            },
        })
    }

    /// Keep a journal of the originals this restore moves aside in
    /// `journal_dir`, for [`recover`] should it be cut short.
    pub fn journal_in(&mut self, journal_dir: &Path) -> Result<()> {
        self.written.journal = Some(Journal::create(journal_dir, &self.base)?);
        Ok(())
    }

    /// Stop restoring and hand back what was written so far, for
    /// [`Written::undo`] after a failure.
    pub fn into_written(mut self) -> Written {
        self.written.dir_modes = std::mem::take(&mut self.dirs);
        self.written
    }

    /// Move the file or link `leaf` names in `parent` aside, so the entry
    /// written next gets a fresh inode and the original can be put back. A
    /// directory there is an error: the entry cannot replace it.
    fn move_aside(&mut self, parent: &File, leaf: &Path, parts: &[String]) -> Result<()> {
        match leaf_type(parent, leaf)? {
            None => Ok(()),
            Some(t) if t.is_dir() => Err(Error::format(format!(
                "entry {:?}: a directory is in the way",
                parts.join("/")
            ))),
            Some(_) => {
                let aside = Aside {
                    parts: parts.to_vec(),
                    aside: aside_name(),
                    id: leaf_id(parent, leaf)?.flatten(),
                };
                // Journaled first: a crash after the move still finds it.
                if let Some(j) = &mut self.written.journal {
                    j.record(&aside)?;
                }
                no_replace_move(parent, leaf, Path::new(&aside.aside))?;
                self.written.asides.push(aside);
                Ok(())
            }
        }
    }

    /// The handle on the directory holding entry `parts`, walked from the root
    /// without following links, creating missing directories when `create`.
    fn parent(&mut self, parts: &[String], create: bool) -> Result<(File, PathBuf)> {
        let Some((leaf, parents)) = parts.split_last() else {
            return Err(Error::format("entry names no path"));
        };
        let dir = match self.cached.take() {
            Some((names, dir)) if names.as_slice() == parents => dir,
            _ => open_parent(&self.root, parents, create, &mut self.written.dirs)
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
        if !leaf_type(&parent, &leaf)?.is_some_and(|t| t.is_dir()) {
            self.move_aside(&parent, &leaf, parts)?;
            capfs::create_dir(&parent, &leaf, &capfs::DirOptions::new())?;
            self.written.dirs.push(Owned {
                parts: parts.to_vec(),
                id: leaf_id(&parent, &leaf)?.flatten(),
            });
        }
        self.dirs.push((parts.to_vec(), mode & MODE_MASK));
        Ok(())
    }

    /// Restore symlink entry `parts` pointing at `target` (raw bytes), leaving
    /// an identical link alone and replacing a file or link.
    pub fn symlink(&mut self, parts: &[String], target: &[u8]) -> Result<()> {
        if !cfg!(unix) {
            return Err(Error::format(format!(
                "entry {:?}: symlinks are not restored on this platform",
                parts.join("/")
            )));
        }
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
        self.move_aside(&parent, &leaf, parts)?;
        make_symlink(target, &parent, &leaf, &self.base, parts)?;
        self.written.leaves.push(Owned {
            parts: parts.to_vec(),
            id: leaf_id(&parent, &leaf)?.flatten(),
        });
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
        self.move_aside(&parent, &leaf, parts)?;
        // `create_new` fails on anything at `leaf`, a symlink included, so the
        // file is never opened through a link swapped in meanwhile.
        let mut out = capfs::open(
            &parent,
            &leaf,
            capfs::OpenOptions::new().write(true).create_new(true),
        )?;
        self.written.leaves.push(Owned {
            parts: parts.to_vec(),
            id: id_of(&out.metadata()?),
        });
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

    /// Returns what the restore wrote. Directory modes wait for
    /// [`Written::commit`].
    pub fn finish(self) -> Written {
        self.into_written()
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
/// [`msg`], for the crate's other modules.
pub(crate) fn message(e: &Error) -> String {
    msg(e)
}

fn msg(e: &Error) -> String {
    match e {
        Error::Format(m) | Error::Network(m) | Error::Unrecoverable(m) => m.clone(),
        Error::Io(e) => e.to_string(),
        Error::Json(e) => e.to_string(),
    }
}

/// Open the directory `parents` names below `root`, one component at a time
/// and never through a symlink, creating missing ones when `create` is set.
pub(crate) fn open_parent(
    root: &File,
    parents: &[String],
    create: bool,
    created: &mut Vec<Owned>,
) -> Result<File> {
    let mut dir = root.try_clone()?;
    for (depth, part) in parents.iter().enumerate() {
        let part_path = Path::new(part);
        match capfs::stat(&dir, part_path, FollowSymlinks::No) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error::format(format!("parent {part:?} is a symlink")));
            }
            Ok(_) => {}
            Err(e) if create && e.kind() == std::io::ErrorKind::NotFound => {
                capfs::create_dir(&dir, part_path, &capfs::DirOptions::new())?;
                created.push(Owned {
                    parts: parents.get(..=depth).unwrap_or_default().to_vec(),
                    id: leaf_id(&dir, part_path)?.flatten(),
                });
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
        // 128 `é`: 256 bytes, too long a component for unix, fine for Windows.
        let accented = "\u{e9}".repeat(128);
        assert_eq!(components(&accented, false).is_ok(), cfg!(windows));
        assert_eq!(
            components(&"ab/".repeat(1366), false).is_err(),
            !cfg!(windows)
        );
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

    #[cfg(unix)]
    fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ocicas-restore-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn p(s: &str) -> Vec<String> {
        components(s, false).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn resolve_link_follows_links_on_disk_and_in_the_set() {
        use std::collections::BTreeMap;
        use std::os::unix::fs::symlink;
        let base = tmp("resolve");
        let outside = tmp("outside");
        std::fs::create_dir_all(base.join("packages/lib")).unwrap();
        symlink(&outside, base.join("bridge")).unwrap();
        symlink("packages", base.join("pk")).unwrap();
        let root = capfs::open_ambient_dir(&base, cap_primitives::ambient_authority()).unwrap();
        let mut pending = BTreeMap::new();
        pending.insert(
            p("nm/.pnpm/lib"),
            Pending::Link(b"../../packages/lib".to_vec()),
        );
        let r = |link: &str, target: &str, pending: &BTreeMap<Vec<String>, Pending>| {
            resolve_link(&root, &base, pending, &p(link), target.as_bytes()).unwrap()
        };
        let inside = |s: &str| Resolution::Inside(if s.is_empty() { vec![] } else { p(s) });
        // pnpm-style links stay inside, through a link of the set itself.
        assert_eq!(
            r("nm/@ws/lib", "../../packages/lib", &pending),
            inside("packages/lib")
        );
        assert_eq!(
            r("nm/@ws/lib", "../.pnpm/lib", &pending),
            inside("packages/lib")
        );
        assert_eq!(r("a", "pk/lib", &pending), inside("packages/lib"));
        assert_eq!(r("a", "pk/..", &pending), inside(""));
        // Through an existing link out of the project.
        assert_eq!(r("out/link", "../bridge", &pending), Resolution::Outside);
        assert_eq!(r("out/link", "../bridge/x", &pending), Resolution::Outside);
        assert_eq!(r("a", "../x", &pending), Resolution::Outside);
        // `..` from a name nothing holds yet depends on what is created there.
        assert_eq!(r("a", "missing/../x", &pending), Resolution::Outside);
        assert_eq!(r("a", "/etc", &pending), Resolution::Outside);
        // Absolute, but naming a path in the project.
        let abs = base.join("packages").to_string_lossy().into_owned();
        assert_eq!(r("a", &abs, &pending), inside("packages"));
        // A loop is an error, not a hang.
        let mut looped = BTreeMap::new();
        looped.insert(p("x"), Pending::Link(b"y".to_vec()));
        looped.insert(p("y"), Pending::Link(b"x".to_vec()));
        assert!(resolve_link(&root, &base, &looped, &p("z"), b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn undo_puts_back_the_originals_it_replaced() {
        let base = tmp("undo");
        std::fs::create_dir_all(base.join("nm")).unwrap();
        std::fs::write(base.join("nm/keep.txt"), b"mine").unwrap();
        std::os::unix::fs::symlink("elsewhere", base.join("nm/link")).unwrap();
        let mut tree = Tree::open(&base).unwrap();
        tree.file(&p("nm/keep.txt"), 0o644, 6, &b"cached"[..])
            .unwrap();
        tree.file(&p("nm/link"), 0o644, 1, &b"x"[..]).unwrap();
        tree.file(&p("new/deep/f"), 0o644, 1, &b"x"[..]).unwrap();
        let written = tree.finish();
        assert_eq!(std::fs::read(base.join("nm/keep.txt")).unwrap(), b"cached");
        written.undo().unwrap();
        assert_eq!(std::fs::read(base.join("nm/keep.txt")).unwrap(), b"mine");
        assert_eq!(
            std::fs::read_link(base.join("nm/link")).unwrap(),
            Path::new("elsewhere")
        );
        assert!(!base.join("new").exists());
        // Nothing left over beside them.
        let names: Vec<_> = std::fs::read_dir(base.join("nm"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(names.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn undo_leaves_what_replaced_its_files_since() {
        let base = tmp("undo-race");
        let mut tree = Tree::open(&base).unwrap();
        tree.file(&p("out/f"), 0o644, 1, &b"x"[..]).unwrap();
        let written = tree.finish();
        // Something else puts a file of its own at the name (written first,
        // so it cannot reuse the restored file's inode).
        std::fs::write(base.join("out/g"), b"theirs").unwrap();
        std::fs::rename(base.join("out/g"), base.join("out/f")).unwrap();
        written.undo().unwrap();
        assert_eq!(std::fs::read(base.join("out/f")).unwrap(), b"theirs");
    }

    #[cfg(unix)]
    #[test]
    fn directory_modes_wait_for_commit_so_undo_can_remove() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp("modes");
        std::fs::create_dir(base.join("old")).unwrap();
        std::fs::set_permissions(base.join("old"), std::fs::Permissions::from_mode(0o750)).unwrap();
        let mut tree = Tree::open(&base).unwrap();
        tree.dir(&p("locked"), 0o000).unwrap();
        tree.file(&p("locked/f"), 0o644, 1, &b"x"[..]).unwrap();
        tree.dir(&p("old"), 0o555).unwrap();
        let mode = |s: &str| {
            std::fs::metadata(base.join(s))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        let written = tree.finish();
        assert_eq!(mode("old"), 0o750);
        written.undo().unwrap();
        assert!(!base.join("locked").exists());
        assert_eq!(mode("old"), 0o750);

        let mut tree = Tree::open(&base).unwrap();
        tree.dir(&p("ro"), 0o555).unwrap();
        tree.file(&p("ro/f"), 0o644, 1, &b"x"[..]).unwrap();
        tree.finish().commit().unwrap();
        assert_eq!(mode("ro"), 0o555);
        std::fs::set_permissions(base.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn put_back_never_replaces_a_file_created_meanwhile() {
        let base = tmp("noreplace");
        std::fs::write(base.join("f"), b"mine").unwrap();
        let mut tree = Tree::open(&base).unwrap();
        tree.file(&p("f"), 0o644, 1, &b"x"[..]).unwrap();
        let written = tree.finish();
        // Something else replaces the restored file with its own.
        std::fs::write(base.join("g"), b"theirs").unwrap();
        std::fs::rename(base.join("g"), base.join("f")).unwrap();
        assert!(written.undo().is_err());
        assert_eq!(std::fs::read(base.join("f")).unwrap(), b"theirs");
    }

    #[cfg(unix)]
    #[test]
    fn recover_puts_back_what_an_interrupted_restore_moved_aside() {
        let base = tmp("crash");
        let journal = tmp("journal");
        std::fs::write(base.join("free"), b"mine").unwrap();
        std::fs::write(base.join("filled"), b"mine").unwrap();
        let mut tree = Tree::open(&base).unwrap();
        tree.journal_in(&journal).unwrap();
        tree.file(&p("free"), 0o644, 1, &b"x"[..]).unwrap();
        tree.file(&p("filled"), 0o644, 1, &b"x"[..]).unwrap();
        // The process dies here: no commit, no undo; the journal's lock goes.
        let written = tree.finish();
        let Written { journal: held, .. } = written;
        drop(held.map(|j| j.file));
        std::fs::remove_file(base.join("free")).unwrap();
        let done = recover(&journal);
        assert_eq!(done.len(), 2, "{done:?}");
        assert_eq!(std::fs::read(base.join("free")).unwrap(), b"mine");
        assert_eq!(std::fs::read(base.join("filled")).unwrap(), b"x");
        let names: Vec<String> = std::fs::read_dir(&base)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| is_aside_name(n)), "{names:?}");
        assert_eq!(std::fs::read_dir(&journal).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_running_restores_journal_is_left_alone() {
        let base = tmp("live");
        let journal = tmp("live-journal");
        let mut tree = Tree::open(&base).unwrap();
        tree.journal_in(&journal).unwrap();
        assert!(recover(&journal).is_empty());
        assert_eq!(std::fs::read_dir(&journal).unwrap().count(), 1);
        tree.finish().commit().unwrap();
        assert_eq!(std::fs::read_dir(&journal).unwrap().count(), 0);
    }

    #[test]
    fn unique_destinations() {
        let a = p_any("a/b");
        let b = p_any("a/c");
        assert!(check_unique([a.as_slice(), b.as_slice()].into_iter()).is_ok());
        assert!(check_unique([a.as_slice(), a.as_slice()].into_iter()).is_err());
        let upper = p_any("A/B");
        let folded = check_unique([a.as_slice(), upper.as_slice()].into_iter()).is_err();
        assert_eq!(folded, cfg!(windows) || cfg!(target_os = "macos"));
    }

    fn p_any(s: &str) -> Vec<String> {
        components(s, false).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn find_below_walks_without_following_links() {
        let base = tmp("walk");
        std::fs::create_dir_all(base.join("vendor/repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("clean/a/b")).unwrap();
        std::os::unix::fs::symlink(base.join("vendor"), base.join("clean/link")).unwrap();
        let root = capfs::open_ambient_dir(&base, cap_primitives::ambient_authority()).unwrap();
        let git = |n: &str| n == ".git";
        assert_eq!(
            find_below(&root, &p("vendor"), 100, git).unwrap(),
            Found::Hit(p("vendor/repo/.git"))
        );
        assert_eq!(
            find_below(&root, &p("clean"), 100, git).unwrap(),
            Found::Nothing
        );
        assert_eq!(
            find_below(&root, &p("clean"), 1, git).unwrap(),
            Found::TooMany
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_drops_the_originals() {
        let base = tmp("commit");
        std::fs::write(base.join("f"), b"mine").unwrap();
        let mut tree = Tree::open(&base).unwrap();
        tree.file(&p("f"), 0o644, 1, &b"x"[..]).unwrap();
        tree.finish().commit().unwrap();
        let names: Vec<_> = std::fs::read_dir(&base).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
        assert_eq!(std::fs::read(base.join("f")).unwrap(), b"x");
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
