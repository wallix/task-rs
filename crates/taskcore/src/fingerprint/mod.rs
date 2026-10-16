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

    /// Creates a unique temporary directory and returns its path as a string.
    pub fn tmp() -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "taskcore-fingerprint-{}-{}-{}",
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
