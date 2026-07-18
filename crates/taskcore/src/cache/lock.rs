//! Cache lock backends selected by the `cache.lock` URL scheme.
//!
//! * `file://` — a local exclusive lockfile with retry/backoff. Distinct lock
//!   names never contend; the same name serializes across processes.
//! * `vk://` / `vks://` — a distributed build-once lock backed by a vk-registry
//!   HTTP lock API ([`ocicas::Locker`]).
//! * `redis://` — a distributed lock via Redis `SET NX EX` with a heartbeat
//!   ([`RedisLocker`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::error::CacheError;
use super::redis_lock::{RedisGuard, RedisLocker};
use super::url::CacheUri;

/// Retry interval while waiting on a contended file lock.
const FILE_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Default contention timeout when none is configured.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3600);

/// A lock backend, selected from the `cache.lock` URL.
pub enum CacheLock {
    /// Local exclusive lockfiles under a directory.
    File {
        dir: PathBuf,
        timeout: Option<Duration>,
    },
    /// The vk-registry HTTP lock.
    Vk(ocicas::Locker),
    /// A Redis `SET NX EX` distributed lock.
    Redis(RedisLocker),
}

/// An acquired lock. Dropping or [`Guard::unlock`]-ing releases it. The inner
/// value is taken on explicit unlock so [`Drop`] then does nothing.
pub enum Guard {
    /// A held file lock; the lockfile is removed on release.
    File(Option<PathBuf>),
    /// A held vk lease.
    Vk(Option<ocicas::Lease>),
    /// A held redis lock.
    Redis(Option<RedisGuard>),
}

impl Guard {
    /// Release the lock explicitly. Dropping also releases (the vk lease then
    /// expires server-side, since its release is async and cannot run in Drop).
    pub async fn unlock(mut self) -> Result<(), CacheError> {
        match &mut self {
            Guard::File(path) => {
                if let Some(p) = path.take() {
                    let _ = std::fs::remove_file(&p);
                }
                Ok(())
            }
            Guard::Vk(lease) => match lease.take() {
                Some(l) => l.unlock().await.map_err(CacheError::from),
                None => Ok(()),
            },
            // Dropping the guard releases the lock synchronously (owner-checked
            // DEL) and stops its heartbeat; see `RedisGuard`'s `Drop`.
            Guard::Redis(guard) => {
                drop(guard.take());
                Ok(())
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Guard::File(Some(path)) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl CacheLock {
    /// Build a lock backend from the resolved `cache.lock` URL string, with an
    /// optional contention timeout (`None` = default 1h). Returns `Ok(None)`
    /// when the string is empty (no lock configured).
    pub fn from_url(raw: &str, timeout: Option<Duration>) -> Result<Option<CacheLock>, CacheError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let u = CacheUri::parse(raw)
            .ok_or_else(|| CacheError::url(format!("cache lock url {raw:?}: not a URL")))?;
        match u.scheme.as_str() {
            "file" => Ok(Some(CacheLock::File {
                dir: PathBuf::from(&u.path),
                timeout,
            })),
            "vk" | "vks" => {
                let scheme = if u.scheme == "vks" { "https" } else { "http" };
                let base = format!("{scheme}://{}", u.host);
                let prefix = u.path.trim_matches('/').to_string();
                let locker = ocicas::Locker::new(base, prefix)?.with_timeout(timeout);
                Ok(Some(CacheLock::Vk(locker)))
            }
            "redis" => Ok(Some(CacheLock::Redis(RedisLocker::new(
                &u.host,
                &u.username,
                u.password.clone(),
                &u.path,
                timeout,
            )))),
            other => Err(CacheError::unsupported(format!(
                "unsupported lock scheme {other:?}"
            ))),
        }
    }

    /// Acquire the lock for `name`, blocking until acquired or the timeout
    /// expires. `on_contention` fires once, the first time the lock is held.
    pub async fn lock<F: FnOnce()>(
        &self,
        name: &str,
        on_contention: F,
    ) -> Result<Guard, CacheError> {
        match self {
            CacheLock::File { dir, timeout } => {
                file_lock(dir, name, timeout.unwrap_or(DEFAULT_TIMEOUT), on_contention).await
            }
            CacheLock::Vk(locker) => locker
                .lock(name, on_contention)
                .await
                .map(|l| Guard::Vk(Some(l)))
                .map_err(CacheError::from),
            CacheLock::Redis(locker) => locker
                .lock(name, on_contention)
                .await
                .map(|g| Guard::Redis(Some(g))),
        }
    }
}

/// Acquire an exclusive lockfile at `dir/<safe name>.lock`, retrying until the
/// timeout. The file is created atomically (`create_new`), so only one holder
/// wins; a stale file left by a dead holder is evicted when its recorded PID is
/// no longer alive.
async fn file_lock<F: FnOnce()>(
    dir: &Path,
    name: &str,
    timeout: Duration,
    on_contention: F,
) -> Result<Guard, CacheError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.lock", safe_name(name)));
    let deadline = Instant::now().checked_add(timeout);
    let mut notified: Option<F> = Some(on_contention);

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                write_holder(&file, name);
                return Ok(Guard::File(Some(path)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(cb) = notified.take() {
                    cb();
                }
                evict_stale_lock(&path);
                if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                    return Err(CacheError::msg(format!(
                        "lock: timeout after {timeout:?} acquiring {name:?}"
                    )));
                }
                tokio::time::sleep(FILE_RETRY_INTERVAL).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn write_holder(mut file: &std::fs::File, name: &str) {
    use std::io::Write as _;
    let _ = write!(file, "pid={}\nlock={name}\n", std::process::id());
}

/// Remove a lockfile whose recorded holder PID is no longer alive.
fn evict_stale_lock(path: &Path) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return;
    };
    let pid = read_holder_pid(&data);
    if pid > 0 && !process_alive(pid) {
        let _ = std::fs::remove_file(path);
    }
}

fn read_holder_pid(info: &str) -> i32 {
    for line in info.split('\n') {
        if let Some(v) = line.strip_prefix("pid=")
            && let Ok(pid) = v.trim().parse::<i32>()
        {
            return pid;
        }
    }
    0
}

/// Whether a process with the given PID exists (POSIX `kill(pid, 0)`).
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only permission/existence checks, no delivery.
    unsafe { libc_kill(pid, 0) == 0 }
}

// Minimal FFI to `kill(2)` — avoids pulling in the `libc` crate for one call.
#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Whether a process with the given PID exists (Windows `OpenProcess`).
#[cfg(windows)]
fn process_alive(pid: i32) -> bool {
    // PROCESS_QUERY_LIMITED_INFORMATION: enough to probe existence without
    // requiring elevated rights against the target process.
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    // SAFETY: OpenProcess only opens a query handle; any handle we obtain is
    // closed immediately. A null handle means the PID is not a live process.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

// Minimal FFI to the Win32 process calls — avoids pulling in `windows-sys` for
// two calls; kernel32 is already linked by std.
#[cfg(windows)]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

/// Characters outside the safe set `[a-zA-Z0-9_.-]` are replaced by `_`; when
/// the name was altered (or contains `..`) a short hash is appended for
/// uniqueness, matching the reference file-lock naming.
fn safe_name(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.len() > 200 {
        safe.truncate(200);
    }
    if safe != name || name.contains("..") {
        safe = format!("{safe}_{}", short_hash(name, 8));
    }
    if safe.is_empty() || safe == "_" {
        safe = short_hash(name, 16);
    }
    safe
}

fn short_hash(s: &str, n: usize) -> String {
    let hex = format!("{:016x}", twox_hash::XxHash3_64::oneshot(s.as_bytes()));
    hex.get(..n).unwrap_or(&hex).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("task-lock-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn mutual_exclusion_same_name() {
        let dir = tempdir();
        let lock = CacheLock::File {
            dir: dir.clone(),
            timeout: Some(Duration::from_millis(500)),
        };
        let g1 = lock.lock("t", || {}).await.unwrap();

        // A second acquire of the same name must time out while the first is held.
        let err = lock.lock("t", || {}).await;
        assert!(err.is_err());
        let msg = err.err().unwrap().to_string();
        assert!(msg.contains("timeout"), "{msg}");

        g1.unlock().await.unwrap();
        // After release the lock is acquirable again.
        let g2 = lock.lock("t", || {}).await.unwrap();
        g2.unlock().await.unwrap();
    }

    #[tokio::test]
    async fn different_names_do_not_block() {
        let dir = tempdir();
        let lock = CacheLock::File {
            dir,
            timeout: Some(Duration::from_secs(2)),
        };
        let a = lock.lock("task-a", || {}).await.unwrap();
        let b = lock.lock("task-b", || {}).await.unwrap();
        a.unlock().await.unwrap();
        b.unlock().await.unwrap();
    }

    #[tokio::test]
    async fn on_contention_fires_only_when_held() {
        let dir = tempdir();
        let lock = CacheLock::File {
            dir,
            timeout: Some(Duration::from_millis(300)),
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let g = lock
            .lock("uncontended", move || {
                c.store(true, std::sync::atomic::Ordering::SeqCst)
            })
            .await
            .unwrap();
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        g.unlock().await.unwrap();
    }

    #[tokio::test]
    async fn released_after_holder_dropped() {
        let dir = tempdir();
        let lock = CacheLock::File {
            dir,
            timeout: Some(Duration::from_secs(2)),
        };
        {
            let _g = lock.lock("drop-test", || {}).await.unwrap();
        } // dropped here → released
        let g = lock.lock("drop-test", || {}).await.unwrap();
        g.unlock().await.unwrap();
    }

    #[test]
    fn stale_lock_evicted() {
        let dir = tempdir();
        let path = dir.join("stale.lock");
        let dead_pid = (1i32 << 22) - 1; // almost certainly not running
        std::fs::write(&path, format!("pid={dead_pid}\nlock=stale\n")).unwrap();
        evict_stale_lock(&path);
        assert!(!path.exists());
    }

    #[test]
    fn read_holder_pid_parsing() {
        assert_eq!(read_holder_pid("pid=42\nlock=test"), 42);
        assert_eq!(read_holder_pid("no-pid-here"), 0);
        assert_eq!(read_holder_pid(""), 0);
        assert_eq!(read_holder_pid("pid=notanumber"), 0);
    }

    #[test]
    fn process_alive_self_and_dead() {
        assert!(process_alive(std::process::id() as i32));
        assert!(!process_alive((1i32 << 22) - 1));
    }

    #[test]
    fn safe_name_sanitizes_and_hashes() {
        assert_eq!(safe_name("simple.name-1"), "simple.name-1");
        let unsafe_name = safe_name("a/b:c");
        assert!(unsafe_name.starts_with("a_b_c_"));
        assert!(!safe_name("..").is_empty());
    }

    #[test]
    fn redis_scheme_builds_locker() {
        assert!(matches!(
            CacheLock::from_url("redis://localhost:6379/locks", None),
            Ok(Some(CacheLock::Redis(_)))
        ));
    }

    #[test]
    fn file_and_empty_from_url() {
        // The vk backend eagerly builds a reqwest client, which needs a process
        // crypto provider installed by the binary — not available under `cargo
        // test` — so vk dispatch is exercised via URL parsing in the url module.
        assert!(matches!(
            CacheLock::from_url("file:///tmp/locks", None),
            Ok(Some(CacheLock::File { .. }))
        ));
        assert!(matches!(CacheLock::from_url("", None), Ok(None)));
        assert_eq!(
            CacheUri::parse("vks://reg:5000/task").map(|u| u.scheme),
            Some("vks".to_string())
        );
    }
}
