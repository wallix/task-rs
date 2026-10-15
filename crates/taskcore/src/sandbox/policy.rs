//! The access rules a sandboxed task's commands run under.
//!
//! Rules follow Landlock's model, which enforces them: each rule grants a set
//! of access rights to a path and everything beneath it, rights add up along a
//! path, and anything without a rule is denied. A rule on a file grants only
//! the rights that apply to files.
//!
//! Inside the root directory the rules are derived from the files the task may
//! read (its sources, `sandbox.reads`, the Taskfiles):
//!
//! - a directory whose files are all readable, recursively, may be changed
//!   freely: what it holds is readable anyway, and what the commands create in
//!   it are their outputs;
//! - a `writes` directory may be changed freely, whatever it holds;
//! - in any other directory, each readable file gets a read rule, and nothing
//!   can be created.
//!
//! The root itself may be listed, so names under it are visible but contents
//! are not. Outside the root everything is allowed, except in the root's own
//! ancestors: they may only be listed, as Landlock cannot grant creating or
//! removing entries in one without granting it throughout the root too.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

/// Landlock filesystem access rights (`include/uapi/linux/landlock.h`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) mod access {
    pub(crate) const EXECUTE: u64 = 1 << 0;
    pub(crate) const WRITE_FILE: u64 = 1 << 1;
    pub(crate) const READ_FILE: u64 = 1 << 2;
    pub(crate) const READ_DIR: u64 = 1 << 3;
    pub(crate) const REMOVE_DIR: u64 = 1 << 4;
    pub(crate) const REMOVE_FILE: u64 = 1 << 5;
    pub(crate) const MAKE_CHAR: u64 = 1 << 6;
    pub(crate) const MAKE_DIR: u64 = 1 << 7;
    pub(crate) const MAKE_REG: u64 = 1 << 8;
    pub(crate) const MAKE_SOCK: u64 = 1 << 9;
    pub(crate) const MAKE_FIFO: u64 = 1 << 10;
    pub(crate) const MAKE_BLOCK: u64 = 1 << 11;
    pub(crate) const MAKE_SYM: u64 = 1 << 12;
    pub(crate) const REFER: u64 = 1 << 13;
    pub(crate) const TRUNCATE: u64 = 1 << 14;
    pub(crate) const IOCTL_DEV: u64 = 1 << 15;
    /// Every right up to `IOCTL_DEV` (Landlock ABI 5).
    pub(crate) const ALL: u64 = EXECUTE
        | WRITE_FILE
        | READ_FILE
        | READ_DIR
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM
        | REFER
        | TRUNCATE
        | IOCTL_DEV;
    /// The rights that apply to a file rather than a directory.
    pub(crate) const FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;
    /// What a readable file in a restricted directory gets.
    pub(crate) const READ: u64 = READ_FILE | EXECUTE;
}

/// The rules for one sandboxed task.
#[derive(Debug)]
pub struct Policy {
    /// The task the rules belong to, for reporting.
    pub task: String,
    /// The root directory, canonical.
    pub root: PathBuf,
    /// The temporary directory commands get as `TMPDIR`, when the system one
    /// contains the root: files created directly in an ancestor of the root
    /// could not be read back.
    pub tmpdir: Option<PathBuf>,
    /// What the user should know about how the rules came out.
    pub warnings: Vec<String>,
    /// Every rule, root ones and outside ones alike, in the order built.
    pub(crate) rules: Vec<(PathBuf, u64)>,
    /// The rules, by path, for checking an access in tests.
    #[cfg(test)]
    by_path: HashMap<PathBuf, u64>,
    /// The lock held on `tmpdir` while the policy lives, which keeps
    /// [`remove_stale_tmp`] off it.
    _tmp_lock: Option<TmpLock>,
    /// The Landlock ruleset built from `rules`, on first use.
    #[cfg(target_os = "linux")]
    pub(crate) ruleset: std::sync::OnceLock<Result<std::os::fd::OwnedFd, String>>,
}

/// The inputs a [`Policy`] is built from. Paths are absolute.
pub struct PolicyInput<'a> {
    /// The task the rules belong to.
    pub task: &'a str,
    /// The directory whose contents are restricted.
    pub root: &'a Path,
    /// The files the commands may read.
    pub readable: Vec<PathBuf>,
    /// The directories the commands may change freely, created when missing:
    /// ignored, with a warning, when one is the root or an ancestor of it.
    pub writable: Vec<PathBuf>,
    /// The task temp directory (`TASK_TEMP_DIR`), made writable like
    /// `writable`.
    pub temp_dir: PathBuf,
    /// The directories `generates` patterns write in, with the pattern: made
    /// writable like `writable`, unless one would cover the whole root.
    pub generated: Vec<(String, PathBuf)>,
    /// The system temporary directory, which commands use unless it contains
    /// the root or is inside it.
    pub system_tmp: PathBuf,
    /// Where to put the commands' `TMPDIR` otherwise; created, and locked
    /// while the policy lives, when needed. It must not exist yet.
    pub private_tmp: PathBuf,
}

/// How a directory came out of the walk, with the rules for what is beneath
/// it.
enum Walk {
    /// Every file beneath is readable, or it is a `writes` directory: the
    /// directory may be changed freely, with one rule on it (the root aside,
    /// which takes the rules beneath instead).
    Free(Vec<(PathBuf, u64)>),
    /// The rules for what beneath may be read or changed.
    Restricted(Vec<(PathBuf, u64)>),
}

struct Walker {
    readable: HashSet<PathBuf>,
    writable: HashSet<PathBuf>,
    /// The directories that have a readable file or a `writes` directory
    /// beneath them; any other non-empty directory needs no walk.
    interesting: HashSet<PathBuf>,
}

impl Policy {
    /// Builds the rules, creating the missing `writable` directories.
    pub fn build(input: PolicyInput<'_>) -> io::Result<Self> {
        let root = std::fs::canonicalize(input.root).map_err(|e| at(e, "resolving", input.root))?;
        // A temporary directory above the root is one where created files
        // cannot be read back; one inside it, one where they mostly cannot be
        // created.
        let system_tmp = std::fs::canonicalize(input.system_tmp).ok();
        let tmpdir = system_tmp
            .filter(|tmp| root.starts_with(tmp) || tmp.starts_with(&root))
            .map(|_| input.private_tmp.clone());
        let tmp_lock = tmpdir.as_deref().map(private_tmp).transpose()?;
        let mut warnings = Vec::new();
        let mut writable = HashSet::new();
        // What each directory is, and what to do about it, for the warning.
        let declared = input.writable.iter().map(|dir| {
            let fix = "list the directories beneath it that the commands change";
            (dir, "sandbox.writes", fix)
        });
        let temp_dir = (
            &input.temp_dir,
            "the task temp directory (TASK_TEMP_DIR)",
            "set TASK_TEMP_DIR to a directory beneath it",
        );
        let tmp = tmpdir.iter().map(|dir| {
            let fix = "set TASK_TEMP_DIR to a directory beneath it";
            (dir, "the commands' TMPDIR (in TASK_TEMP_DIR)", fix)
        });
        for (dir, what, fix) in declared.chain([temp_dir]).chain(tmp) {
            // One outside the root is created too: directly in an ancestor of
            // the root, the commands could not create it themselves.
            let Some(real) = beneath(&root, dir)? else {
                warnings.push(format!(
                    "{what} {:?} is the root directory or above it and is ignored; {fix}",
                    dir.display()
                ));
                continue;
            };
            writable.insert(real);
        }
        // A file that does not exist (yet) cannot be read: leave it out.
        let mut dirs = HashMap::new();
        let readable: HashSet<PathBuf> = input
            .readable
            .iter()
            .filter_map(|p| canonical_file(p, &mut dirs))
            .collect();

        let mut warned = HashSet::new();
        for (pattern, dir) in &input.generated {
            let Some(dir) = beneath(&root, dir)? else {
                warnings.push(format!(
                    "generates {pattern:?} writes directly in the root directory, which is not \
                     made writable; put the outputs in a subdirectory (e.g. bin/app)"
                ));
                continue;
            };
            if !writable.contains(&dir)
                && readable.iter().any(|r| r.starts_with(&dir))
                && warned.insert(dir.clone())
            {
                warnings.push(format!(
                    "generates {pattern:?} makes {} writable, which holds sources too: \
                     every file in it is readable; give the outputs a directory of their own, \
                     or list it in sandbox.writes",
                    dir.display()
                ));
            }
            writable.insert(dir);
        }

        let mut interesting = HashSet::new();
        for path in readable.iter().chain(writable.iter()) {
            for dir in path.ancestors().skip(1) {
                if !dir.starts_with(&root) || !interesting.insert(dir.to_path_buf()) {
                    break;
                }
            }
        }

        let walker = Walker {
            readable,
            writable,
            interesting,
        };
        let mut rules = outside_rules(&root);
        // The root itself only lists, even with every file in it readable:
        // what it holds gets the rules, so that nothing is created, removed or
        // renamed directly in it.
        let (Walk::Free(inside) | Walk::Restricted(inside)) = walker.walk(&root);
        rules.extend(inside);
        rules.push((root.clone(), access::READ_DIR));
        Ok(Self {
            #[cfg(test)]
            by_path: rules.iter().cloned().collect(),
            task: input.task.to_string(),
            root,
            tmpdir,
            warnings,
            rules,
            _tmp_lock: tmp_lock,
            #[cfg(target_os = "linux")]
            ruleset: std::sync::OnceLock::new(),
        })
    }

    /// A policy of just `rules`, for testing how they are enforced.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn for_rules(root: PathBuf, rules: Vec<(PathBuf, u64)>) -> Self {
        Self {
            by_path: rules.iter().cloned().collect(),
            task: "t".to_string(),
            root,
            tmpdir: None,
            warnings: Vec::new(),
            rules,
            _tmp_lock: None,
            ruleset: std::sync::OnceLock::new(),
        }
    }

    /// How many rules the policy has, for reporting.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// The rights `path` (absolute, without `.`/`..` or symlinks) is granted,
    /// as Landlock has it: those of the rules on it and on its ancestors. So
    /// outside the root, an entry made in an ancestor of the root since the
    /// rules were built may only be listed, as the ancestor itself.
    #[cfg(test)]
    pub(crate) fn granted(&self, path: &Path) -> u64 {
        path.ancestors()
            .filter_map(|dir| self.by_path.get(dir))
            .fold(0, |granted, rights| granted | rights)
    }

    /// Reports whether `path` is granted every right in `need`.
    #[cfg(test)]
    pub(crate) fn allows(&self, path: &Path, need: u64) -> bool {
        self.granted(path) & need == need
    }

    /// Reports whether an entry may be linked or moved from directory `from`
    /// into directory `to`, as Landlock's `REFER` rule has it: across
    /// directories, both must grant `REFER` and the entry must gain no right
    /// it lacked — a file only its file rights, a directory every right.
    #[cfg(test)]
    pub(crate) fn refers(&self, from: &Path, to: &Path, dir: bool) -> bool {
        if from == to {
            return true;
        }
        let (had, gets) = (self.granted(from), self.granted(to));
        let kept = if dir { access::ALL } else { access::FILE };
        had & gets & access::REFER != 0 && gets & kept & !had == 0
    }
}

impl Walker {
    fn walk(&self, dir: &Path) -> Walk {
        if self.writable.contains(dir) {
            return Walk::Free(Vec::new());
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Walk::Restricted(Vec::new());
        };
        if !self.interesting.contains(dir) {
            // Nothing beneath is readable: free only when it is empty.
            let mut entries = entries;
            return if entries.next().is_none() {
                Walk::Free(Vec::new())
            } else {
                Walk::Restricted(Vec::new())
            };
        }
        let mut free = true;
        let mut rules = Vec::new();
        for entry in entries {
            // An entry that cannot be read may be a file that is not readable.
            let Ok(entry) = entry else {
                free = false;
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                free = false;
                continue;
            };
            let path = entry.path();
            // A symlink grants nothing: what it points to is checked on its own
            // path.
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                match self.walk(&path) {
                    Walk::Free(_) => rules.push((path, access::ALL)),
                    Walk::Restricted(inner) => {
                        free = false;
                        rules.extend(inner);
                    }
                }
            } else if self.readable.contains(&path) {
                rules.push((path, access::READ));
            } else {
                free = false;
            }
        }
        if free {
            Walk::Free(rules)
        } else {
            Walk::Restricted(rules)
        }
    }
}

/// Resolves symlinks in absolute `path`, returning `None` if it is missing.
/// Many files share directories: caching their resolution in `dirs` leaves
/// one `lstat` per file that is not itself a symlink.
fn canonical_file(path: &Path, dirs: &mut HashMap<PathBuf, Option<PathBuf>>) -> Option<PathBuf> {
    // No file name: the path ends in `..`, or is `/`.
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return std::fs::canonicalize(path).ok();
    };
    let dir = dirs
        .entry(parent.to_path_buf())
        .or_insert_with(|| std::fs::canonicalize(parent).ok())
        .clone()?;
    let file = dir.join(name);
    match std::fs::symlink_metadata(&file) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(&file).ok(),
        Ok(_) => Some(file),
        Err(_) => None,
    }
}

/// A run's temporary directory, locked while the run lives: a lock file beside
/// it (`<dir>.lock`), out of reach of what the commands do to `TMPDIR`.
/// Dropping it removes both.
#[derive(Debug)]
struct TmpLock {
    dir: PathBuf,
    /// Holds the lock while open.
    _file: std::fs::File,
}

impl Drop for TmpLock {
    fn drop(&mut self) {
        remove_tmp(&self.dir);
        live().remove(&self.dir);
    }
}

/// Removes the run's temporary directory `dir` and its lock file, once its
/// commands are done; the lock itself stays held until the policy is dropped,
/// which may outlive the run.
pub fn remove_tmp(dir: &Path) {
    // The directory first: one is never left without its lock file but by a
    // crash, and `remove_stale_tmp` handles that. Best effort: what is left
    // over is only temporary files, and either may be gone already.
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_file(suffixed(dir, ".lock"));
}

/// The temporary directories of this process's live runs, which
/// [`remove_stale_tmp`] leaves alone even where a lock gives no protection
/// within a process (`flock` emulated with POSIX locks, on NFS).
fn live() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    static LIVE: std::sync::OnceLock<std::sync::Mutex<HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `path` with `suffix` appended to its file name.
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Creates the run's temporary directory `dir`, which must not exist yet, and
/// its lock, held until the result is dropped. The lock file exists, locked,
/// before the directory: it is locked under a setup name and then renamed into
/// place, so that a lock file found unlocked never belongs to a live run, and
/// a directory found without one was left by a crash.
fn private_tmp(dir: &Path) -> io::Result<TmpLock> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| at(e, "creating", parent))?;
    }
    let lock = suffixed(dir, ".lock");
    let mut attempt = 0u8;
    let file = loop {
        let setup = suffixed(dir, &format!(".lock.new{attempt}"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&setup)
            .map_err(|e| at(e, "creating", &setup))?;
        file.lock().map_err(|e| at(e, "locking", &setup))?;
        match std::fs::rename(&setup, &lock) {
            Ok(()) => break file,
            // A concurrent `remove_stale_tmp` took it, still unlocked, for a
            // leftover: try again under another name.
            Err(e) if e.kind() == io::ErrorKind::NotFound && attempt < 3 => {
                attempt = attempt.saturating_add(1);
            }
            Err(e) => return Err(at(e, "renaming", &setup)),
        }
    };
    live().insert(dir.to_path_buf());
    let tmp = TmpLock {
        dir: dir.to_path_buf(),
        _file: file,
    };
    std::fs::create_dir(dir).map_err(|e| at(e, "creating", dir))?;
    Ok(tmp)
}

/// Removes what runs that are gone left under `tmp`: a directory whose lock
/// file nobody holds, or that has none (a crash between the two), and a lock
/// file left setting up. Safe to run concurrently, from any process sharing
/// `tmp`: a live run holds its lock from before its directory exists until
/// after it is removed.
pub fn remove_stale_tmp(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(dir) = name.strip_suffix(".lock") {
            let dir = tmp.join(dir);
            if live().contains(&dir) {
                continue;
            }
            if let Ok(file) = std::fs::File::open(&path)
                && file.try_lock().is_ok()
            {
                // Best effort, the lock held meanwhile: a later run tries
                // again. The directory first, as a live run removes them.
                let _ = std::fs::remove_dir_all(&dir);
                let _ = std::fs::remove_file(&path);
            }
        } else if name.contains(".lock.new") {
            if let Ok(file) = std::fs::File::open(&path)
                && file.try_lock().is_ok()
            {
                // Best effort, as above; its run retries if it was setting up.
                let _ = std::fs::remove_file(&path);
            }
        } else if entry.file_type().is_ok_and(|k| k.is_dir())
            && !suffixed(&path, ".lock").exists()
            && !live().contains(&path)
        {
            // Best effort, as above.
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// `dir` canonical, created when missing, or `None` when it is `root` or an
/// ancestor of it. Checked before creating it and again after: a symlink on the
/// way may lead back to the root.
fn beneath(root: &Path, dir: &Path) -> io::Result<Option<PathBuf>> {
    if root.starts_with(resolved(dir)) {
        return Ok(None);
    }
    std::fs::create_dir_all(dir).map_err(|e| at(e, "creating", dir))?;
    let real = std::fs::canonicalize(dir).map_err(|e| at(e, "resolving", dir))?;
    Ok((!root.starts_with(&real)).then_some(real))
}

/// Adds the operation and path missing from the OS error.
fn at(e: io::Error, doing: &str, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{doing} {}: {e}", path.display()))
}

/// `path` with the symlinks of its longest existing ancestor resolved, and the
/// missing rest appended.
fn resolved(path: &Path) -> PathBuf {
    for dir in path.ancestors() {
        if let Ok(real) = std::fs::canonicalize(dir) {
            let rest = path.strip_prefix(dir).unwrap_or(Path::new(""));
            return real.join(rest);
        }
    }
    path.to_path_buf()
}

/// Grants everything beside each ancestor of `root` (the siblings on the way
/// down), which is everything outside it, and listing the ancestors themselves.
/// Symlinks are skipped: one could point back into the root.
fn outside_rules(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut rules = Vec::new();
    let mut below = root;
    for dir in root.ancestors().skip(1) {
        // Inherited by the root too, which may be listed anyway.
        rules.push((dir.to_path_buf(), access::READ_DIR));
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path == below {
                    continue;
                }
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => rules.push((path, access::ALL)),
                    Ok(kind) if !kind.is_symlink() => rules.push((path, access::FILE)),
                    _ => {}
                }
            }
        }
        below = dir;
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::access::*;
    use super::*;

    fn tree(files: &[&str]) -> (tempfile_dir::Dir, PathBuf) {
        let dir = tempfile_dir::Dir::new();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        (dir, root)
    }

    fn build(root: &Path, readable: &[&str], writable: &[&str]) -> Policy {
        build_with(root, readable, writable, &[])
    }

    fn build_with(root: &Path, readable: &[&str], writable: &[&str], generated: &[&str]) -> Policy {
        Policy::build(PolicyInput {
            task: "t",
            root,
            readable: readable.iter().map(|p| root.join(p)).collect(),
            writable: writable.iter().map(|p| root.join(p)).collect(),
            temp_dir: root.join(".task"),
            generated: generated
                .iter()
                .map(|p| (p.to_string(), root.join(p)))
                .collect(),
            system_tmp: std::env::temp_dir(),
            private_tmp: root.join(".task/tmp"),
        })
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_generated_directory_leading_back_to_the_root_is_not_writable() {
        let (_d, root) = tree(&["src/a.c", "secret"]);
        std::os::unix::fs::symlink(&root, root.join("root-link")).unwrap();
        let p = build_with(&root, &["src/a.c"], &[], &["root-link", "root-link/app/.."]);
        assert!(!p.allows(&root.join("secret"), READ_FILE));
        assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("put the outputs in a subdirectory"));
    }

    #[test]
    fn a_generated_file_at_the_root_is_reported() {
        let (_d, root) = tree(&["src/a.c", "README"]);
        // `generates: [app]` writes in the root itself.
        let p = build_with(&root, &["src/a.c"], &[], &[""]);
        assert!(!p.allows(&root, MAKE_REG));
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("subdirectory"));
    }

    #[test]
    fn writes_at_or_above_the_root_are_ignored_and_reported() {
        let (_d, root) = tree(&["src/a.c", "secret"]);
        let parent = root.parent().unwrap().to_path_buf();
        let p = Policy::build(PolicyInput {
            task: "t",
            root: &root,
            readable: vec![root.join("src/a.c")],
            writable: vec![root.clone(), parent.clone(), root.join("src/..")],
            temp_dir: root.join(".task"),
            generated: Vec::new(),
            system_tmp: std::env::temp_dir(),
            private_tmp: root.join(".task/tmp"),
        })
        .unwrap();
        assert!(!p.allows(&root.join("secret"), READ_FILE));
        assert!(!p.allows(&parent, MAKE_REG));
        assert_eq!(p.warnings.len(), 3, "{:?}", p.warnings);
        assert!(p.warnings.iter().all(|w| w.contains("is ignored")));
        assert!(p.warnings.iter().all(|w| w.starts_with("sandbox.writes")));
    }

    #[test]
    fn a_temp_dir_at_the_root_is_reported_as_such() {
        let (_d, root) = tree(&["src/a.c", "secret"]);
        let p = Policy::build(PolicyInput {
            task: "t",
            root: &root,
            readable: vec![root.join("src/a.c")],
            writable: Vec::new(),
            temp_dir: root.clone(),
            generated: Vec::new(),
            system_tmp: std::env::temp_dir(),
            private_tmp: root.join("tmp/run"),
        })
        .unwrap();
        assert!(!p.allows(&root.join("secret"), READ_FILE));
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(
            p.warnings[0].starts_with("the task temp directory (TASK_TEMP_DIR)"),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_setup_failure_names_the_path() {
        let (_d, root) = tree(&["src/a.c", "file"]);
        let err = Policy::build(PolicyInput {
            task: "t",
            root: &root,
            readable: Vec::new(),
            writable: vec![root.join("file/out")],
            temp_dir: root.join(".task"),
            generated: Vec::new(),
            system_tmp: std::env::temp_dir(),
            private_tmp: root.join(".task/tmp/run"),
        })
        .unwrap_err();
        let want = format!("creating {}", root.join("file/out").display());
        assert!(err.to_string().starts_with(&want), "{err}");
    }

    #[test]
    fn all_is_every_right_up_to_abi_5() {
        assert_eq!(ALL, (1 << 16) - 1);
    }

    #[test]
    fn the_root_itself_is_never_writable() {
        let (_d, root) = tree(&["a", "sub/b"]);
        let p = build(&root, &["a", "sub/b"], &[]);
        assert!(!p.allows(&root, MAKE_REG));
        assert!(!p.allows(&root, REMOVE_FILE));
        assert!(p.allows(&root.join("a"), READ_FILE));
        assert!(!p.allows(&root.join("a"), WRITE_FILE));
        assert!(p.allows(&root.join("sub/new"), MAKE_REG | WRITE_FILE));
    }

    #[test]
    fn only_what_dead_runs_left_in_the_temporary_directory_is_removed() {
        let (_d, root) = tree(&["a"]);
        let tmp = root.join(".task/tmp");
        let live = build_tmp(&root, &tmp.join("live"));
        assert!(tmp.join("live").is_dir() && tmp.join("live.lock").is_file());
        // A run killed after locking, before its directory, and before
        // locking.
        std::fs::create_dir_all(tmp.join("stale")).unwrap();
        std::fs::write(tmp.join("stale.lock"), "").unwrap();
        std::fs::create_dir_all(tmp.join("orphan")).unwrap();
        std::fs::write(tmp.join("setup.lock.new0"), "").unwrap();
        remove_stale_tmp(&tmp);
        let left: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let mut left = left;
        left.sort();
        assert_eq!(left, ["live", "live.lock"]);
        drop(live);
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    }

    #[test]
    fn a_system_temporary_directory_inside_the_root_is_replaced() {
        let (_d, root) = tree(&["a", "tmp/x"]);
        let p = Policy::build(PolicyInput {
            task: "t",
            root: &root,
            readable: Vec::new(),
            writable: Vec::new(),
            temp_dir: root.join(".task"),
            generated: Vec::new(),
            system_tmp: root.join("tmp"),
            private_tmp: root.join(".task/tmp/run"),
        })
        .unwrap();
        assert_eq!(p.tmpdir, Some(root.join(".task/tmp/run")));
        let elsewhere = Policy::build(PolicyInput {
            task: "t",
            root: &root,
            readable: Vec::new(),
            writable: Vec::new(),
            temp_dir: root.join(".task"),
            generated: Vec::new(),
            system_tmp: PathBuf::from("/"),
            private_tmp: root.join(".task/tmp/other"),
        })
        .unwrap();
        assert_eq!(elsewhere.tmpdir, Some(root.join(".task/tmp/other")));
        let apart = PathBuf::from(format!("{}-tmp", root.display()));
        std::fs::create_dir_all(&apart).unwrap();
        let unrelated = Policy::build(PolicyInput {
            task: "t",
            root: &root,
            readable: Vec::new(),
            writable: Vec::new(),
            temp_dir: root.join(".task"),
            generated: Vec::new(),
            system_tmp: apart.clone(),
            private_tmp: root.join(".task/tmp/none"),
        })
        .unwrap();
        assert_eq!(unrelated.tmpdir, None);
        std::fs::remove_dir_all(&apart).unwrap();
    }

    /// A policy whose temporary directory is `private_tmp`.
    fn build_tmp(root: &Path, private_tmp: &Path) -> Policy {
        Policy::build(PolicyInput {
            task: "t",
            root,
            readable: Vec::new(),
            writable: Vec::new(),
            temp_dir: root.join(".task"),
            generated: Vec::new(),
            system_tmp: std::env::temp_dir(),
            private_tmp: private_tmp.to_path_buf(),
        })
        .unwrap()
    }

    #[test]
    fn ancestors_of_the_root_may_only_be_listed() {
        let (_d, root) = tree(&["a"]);
        let p = build(&root, &[], &[]);
        for dir in root.ancestors().skip(1) {
            assert_eq!(p.granted(dir), READ_DIR, "{}", dir.display());
            assert!(p.rules.contains(&(dir.to_path_buf(), READ_DIR)));
        }
        assert!(!p.allows(root.parent().unwrap(), MAKE_REG));
        assert!(!p.refers(root.parent().unwrap(), &root, false));
    }

    #[test]
    fn a_generated_directory_holding_sources_is_reported() {
        let (_d, root) = tree(&["src/a.c", "src/config.h"]);
        let p = build_with(&root, &["src/a.c"], &[], &["src", "out"]);
        assert!(p.allows(&root.join("src/config.h"), READ_FILE));
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("holds sources too"));
    }

    #[test]
    fn reads_only_readable_files_in_a_mixed_directory() {
        let (_d, root) = tree(&["src/a.c", "src/b.c", "README"]);
        let p = build(&root, &["src/a.c"], &[]);
        assert!(p.allows(&root.join("src/a.c"), READ_FILE));
        assert!(!p.allows(&root.join("src/a.c"), WRITE_FILE));
        assert!(!p.allows(&root.join("src/b.c"), READ_FILE));
        assert!(!p.allows(&root.join("README"), READ_FILE));
        assert!(!p.allows(&root.join("src"), MAKE_REG));
        assert!(p.allows(&root.join("src"), READ_DIR));
    }

    #[test]
    fn links_and_moves_must_not_widen_access() {
        let (_d, root) = tree(&["src/a.c", "secret/key"]);
        let p = build(&root, &["src/a.c"], &["out", "tmp"]);
        assert!(!p.refers(&root.join("secret"), &root.join("out"), false));
        assert!(p.refers(&root.join("out"), &root.join("tmp"), false));
        assert!(p.refers(&root.join("secret"), &root.join("secret"), false));
        assert!(!p.refers(&root.join("secret"), Path::new("/elsewhere"), false));
    }

    #[test]
    fn a_directory_of_readable_files_is_free() {
        let (_d, root) = tree(&["pkg/debian/rules", "pkg/debian/control", "other"]);
        let p = build(&root, &["pkg/debian/rules", "pkg/debian/control"], &[]);
        assert!(p.allows(&root.join("pkg/debian/tmp/new"), MAKE_REG | WRITE_FILE));
        assert!(p.allows(&root.join("pkg"), MAKE_DIR));
        assert!(!p.allows(&root.join("other"), READ_FILE));
        assert!(!p.allows(&root, MAKE_REG));
    }

    #[test]
    fn writes_directories_are_free_and_created() {
        let (_d, root) = tree(&["src/a.c", "src/target/old.o", "build/keep"]);
        let p = build(&root, &["src/a.c"], &["src/target", "build/out"]);
        assert!(p.allows(&root.join("src/target/old.o"), READ_FILE | WRITE_FILE));
        assert!(root.join("build/out").is_dir());
        assert!(p.allows(&root.join("build/out/x"), MAKE_REG));
        assert!(!p.allows(&root.join("build"), MAKE_REG));
        assert!(!p.allows(&root.join("build/keep"), READ_FILE));
    }

    #[test]
    fn empty_directories_are_free_and_outside_is_allowed() {
        let (_d, root) = tree(&["Taskfile.yml", "a"]);
        std::fs::create_dir(root.join("empty")).unwrap();
        let sibling = PathBuf::from(format!("{}-sibling", root.display()));
        std::fs::create_dir_all(&sibling).unwrap();
        let p = build(&root, &["Taskfile.yml"], &[]);
        assert!(p.allows(&root.join("empty/x"), MAKE_REG));
        assert!(p.allows(&sibling.join("x"), WRITE_FILE | MAKE_REG));
        // Made in an ancestor since: no rule, as under Landlock.
        let late = PathBuf::from(format!("{}-late", root.display()));
        assert_eq!(p.granted(&late), READ_DIR);
        std::fs::remove_dir_all(&sibling).unwrap();
        let outside = p.rules.iter().any(|(path, _)| !path.starts_with(&root));
        assert!(outside, "expected rules for the root's siblings");
    }

    #[test]
    fn a_root_under_the_temporary_directory_gets_its_own() {
        let (_d, root) = tree(&["a"]);
        let p = build(&root, &[], &[]);
        assert_eq!(p.tmpdir, Some(root.join(".task/tmp")));
        assert!(p.allows(&root.join(".task/tmp/cc1.s"), MAKE_REG | READ_FILE));
    }

    /// A self-removing temporary directory, kept local to these tests.
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                static N: AtomicUsize = AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("task-sandbox-policy-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
