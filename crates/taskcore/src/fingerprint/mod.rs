//! Up-to-date detection for tasks.
//!
//! The [`ChecksumChecker`] compares a deterministic checksum of a task's source
//! files (and serialized rules) against a value stored under `.task/checksum`.
//! Glob expansion of `sources:`/`generates:` patterns is provided by [`globs`],
//! [`cache_globs`], and [`glob`].

mod checksum;
mod glob;
mod offload;
mod pool;
mod sources_checksum;

pub use checksum::{Datum, Snapshot, checksum_files};
pub use glob::{CacheMatcher, cache_globs, glob, globs};
pub use sources_checksum::{ChecksumChecker, PostRun, Recorded, TaskStatus};

/// Counts readings of task sources, per task directory, once [`record`] is
/// called: a hook for tests asserting how many passes a run makes. Off, it
/// costs one atomic load per reading.
#[doc(hidden)]
pub mod passes {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    static ON: AtomicBool = AtomicBool::new(false);
    static COUNTS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

    /// Starts counting.
    pub fn record() {
        ON.store(true, Ordering::Relaxed);
    }

    /// The readings so far of sources of tasks in `dir`.
    pub fn of(dir: &str) -> u64 {
        COUNTS
            .lock()
            .map_or(0, |c| c.get(dir).copied().unwrap_or(0))
    }

    pub(crate) fn note(dir: &str) {
        if ON.load(Ordering::Relaxed)
            && let Ok(mut c) = COUNTS.lock()
        {
            let n = c.entry(dir.to_string()).or_insert(0);
            *n = n.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod testutil {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SWEEP: std::sync::Once = std::sync::Once::new();

    const PREFIX: &str = "taskcore-fingerprint-";

    /// Creates a unique temporary directory and returns its path as a string.
    ///
    /// The directories are not removed when a test ends (callers hold plain
    /// paths); instead the first call in a test process removes those left by
    /// test processes that have exited, so they do not pile up in `TMPDIR`.
    pub fn tmp() -> String {
        SWEEP.call_once(sweep_stale);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "{PREFIX}{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            n
        ));
        fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    /// Removes `tmp()` directories whose test process (the first number in
    /// the name) is gone. Some tests make a directory unreadable, so owner
    /// permissions are restored first. Best effort: errors are ignored.
    fn sweep_stale() {
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix(PREFIX))
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            if pid != std::process::id() && !alive(pid) {
                let path = entry.path();
                make_owner_accessible(&path);
                let _ = fs::remove_dir_all(&path);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn alive(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Without a portable liveness check, keep everything.
    #[cfg(not(target_os = "linux"))]
    fn alive(_pid: u32) -> bool {
        true
    }

    #[cfg(unix)]
    fn make_owner_accessible(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if !meta.is_dir() {
            return;
        }
        let mode = meta.permissions().mode() | 0o700;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_owner_accessible(&entry.path());
            }
        }
    }

    #[cfg(not(unix))]
    fn make_owner_accessible(_path: &Path) {}

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn sweep_removes_unreadable_dirs_of_exited_processes() {
            // A finished child's pid stands in for an exited test process.
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let dead = child.id();
            child.wait().unwrap();
            let stale = std::env::temp_dir().join(format!("{PREFIX}{dead}-0-0"));
            let locked = stale.join("locked");
            fs::create_dir_all(&locked).unwrap();
            fs::write(locked.join("in.txt"), "x").unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let mine = tmp();

            sweep_stale();

            assert!(!stale.exists(), "{} left behind", stale.display());
            assert!(
                Path::new(&mine).exists(),
                "the live process's dir was removed"
            );
        }
    }

    /// Polls `f` once, outside a runtime: fingerprint readings then run inline
    /// and complete at once.
    pub fn now<F: std::future::Future>(f: F) -> F::Output {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(f).as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("a reading waited outside a runtime"),
        }
    }

    /// Joins a directory with a relative path.
    pub fn join(dir: &str, rel: &str) -> String {
        format!("{dir}/{rel}")
    }

    /// Writes `content` to `dir/rel`, creating parent directories.
    pub fn write_file(dir: &str, rel: &str, content: &str) {
        let path = join(dir, rel);
        if let Some(parent) = Path::new(&path).parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    /// Builds a fake `node_modules` tree with a `.yarn-state.yml` marker (a
    /// dotfile not matched by `**/*`) and two package files.
    pub fn setup_node_modules() -> String {
        let dir = tmp();
        write_file(&dir, "node_modules/.yarn-state.yml", "yarn state");
        write_file(&dir, "node_modules/vite/bin/vite.js", "vite binary");
        write_file(&dir, "node_modules/react/index.js", "react module");
        dir
    }
}
