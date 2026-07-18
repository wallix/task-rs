//! The remote build cache: download/extract a task's generated files on a hit,
//! and export them on a miss. Two transports are supported, selected by the
//! `cache.url` scheme:
//!
//! * `file://` — a zip archive of the generated files, cache metadata in the
//!   zip comment ([`archive`]).
//! * `oci://` — an ocicas artifact of content-defined chunks deduplicated
//!   registry-side ([`oci`]).
//!
//! The `cache.lock` scheme selects a distributed build-once lock ([`lock`]).
//!
//! The functions here take explicit parameters (working dir, fingerprint temp
//! dir, logger, task) rather than an executor, mirroring the reference
//! implementation's executor cache methods.

pub mod archive;
pub mod error;
pub mod lock;
pub mod oci;
pub mod redis_lock;
pub mod url;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::ast::Task;
use crate::fingerprint::ChecksumChecker;
use crate::logger::{Color, Logger};

pub use archive::CacheMeta;
pub use error::CacheError;
pub use lock::{CacheLock, Guard};
pub use url::CacheUri;

/// A parsed `cache.url`, dispatched by scheme.
pub enum CacheUrl {
    /// A `file://` zip archive at the given path.
    Zip { path: String },
    /// An `oci://` artifact: repository, tag, and store options.
    Oci {
        repo: String,
        tag: String,
        opts: ocicas::RemoteOptions,
    },
    /// A `redis://` entry — not supported in this build.
    Redis,
}

impl CacheUrl {
    /// Parse the resolved `cache.url` string. Returns `Ok(None)` when empty.
    /// Template variables are resolved during task compilation, so the string
    /// is ready to parse directly.
    pub fn parse(raw: &str) -> Result<Option<CacheUrl>, CacheError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let u = CacheUri::parse(raw)
            .ok_or_else(|| CacheError::url(format!("cache url {raw:?}: not a URL")))?;
        match u.scheme.as_str() {
            "file" => Ok(Some(CacheUrl::Zip {
                path: u.path.clone(),
            })),
            "oci" => {
                let (repo, tag, opts) = oci::parse_oci_cache_url(&u)?;
                Ok(Some(CacheUrl::Oci { repo, tag, opts }))
            }
            "redis" => Ok(Some(CacheUrl::Redis)),
            other => Err(CacheError::unsupported(format!(
                "unsupported cache scheme {other:?}"
            ))),
        }
    }
}

/// Hosts already reported as unreachable, so the warning prints once per host
/// per run rather than once per cached task (a build has many).
fn unreachable_warned() -> &'static Mutex<HashSet<String>> {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The registry host of an `host[:port]/repo[:tag]` reference — the leading
/// segment before the first `/`, used to key the once-per-host warning.
fn cache_host(repo: &str) -> &str {
    repo.split('/').next().unwrap_or(repo)
}

/// Emit a single visible warning the first time `repo`'s host is found
/// unreachable. Cache errors are otherwise logged at verbose level so a normal
/// miss stays quiet; a connectivity failure that silently disables the cache
/// should not.
fn warn_cache_unreachable(logger: &mut Logger, repo: &str, err: &ocicas::Error) {
    if !err.is_unreachable() {
        return;
    }
    let host = cache_host(repo).to_string();
    let first_seen = match unreachable_warned().lock() {
        Ok(mut warned) => warned.insert(host.clone()),
        Err(_) => return,
    };
    if first_seen {
        logger.errf(
            Color::Yellow,
            &format!(
                "task: WARNING: cache registry {host} unreachable, continuing without cache ({err})\n"
            ),
        );
    }
}

/// Attempt to download and restore a cached entry into `dir`. On success
/// returns `(true, meta)`; the caller verifies the metadata against the current
/// task state. A miss or any recoverable failure returns `(false, _)`.
pub async fn cache_restore(
    url: &CacheUrl,
    task_name: &str,
    dir: &Path,
    logger: &mut Logger,
) -> (bool, CacheMeta) {
    match url {
        CacheUrl::Zip { path } => restore_file(Path::new(path), task_name, dir, logger),
        CacheUrl::Oci { repo, tag, opts } => {
            restore_oci(repo, tag, opts.clone(), task_name, dir, logger).await
        }
        CacheUrl::Redis => {
            logger.verbose_errf(
                Color::Yellow,
                "task: redis cache not yet supported in the Rust port\n",
            );
            (false, CacheMeta::default())
        }
    }
}

fn restore_file(
    zip_path: &Path,
    task_name: &str,
    dir: &Path,
    logger: &mut Logger,
) -> (bool, CacheMeta) {
    if !zip_path.exists() {
        return (false, CacheMeta::default()); // miss
    }
    match archive::extract_archive(zip_path, dir) {
        Ok(meta) => {
            if meta.generates.is_empty() {
                logger.errf(
                    Color::Yellow,
                    &format!(
                        "task: WARNING: cache for {task_name:?} has no generates checksum, rejecting\n"
                    ),
                );
                return (false, CacheMeta::default());
            }
            logger.errf(
                Color::Magenta,
                &format!("task: {task_name:?} restored from cache\n"),
            );
            (true, meta)
        }
        Err(e) => {
            logger.verbose_errf(Color::Yellow, &format!("task: cache extract: {e}\n"));
            (false, CacheMeta::default())
        }
    }
}

async fn restore_oci(
    repo: &str,
    tag: &str,
    opts: ocicas::RemoteOptions,
    task_name: &str,
    dir: &Path,
    logger: &mut Logger,
) -> (bool, CacheMeta) {
    let store = match ocicas::Store::open(repo, opts).await {
        Ok(s) => s,
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            return (false, CacheMeta::default());
        }
    };
    match store.resolve_annotations(tag).await {
        Ok(None) => return (false, CacheMeta::default()), // miss
        Ok(Some(ann)) => {
            if ann
                .get(oci::ANN_GENERATES)
                .map(String::as_str)
                .unwrap_or("")
                .is_empty()
            {
                logger.errf(
                    Color::Yellow,
                    &format!(
                        "task: WARNING: cache for {task_name:?} has no generates checksum, rejecting\n"
                    ),
                );
                return (false, CacheMeta::default());
            }
        }
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            return (false, CacheMeta::default());
        }
    }
    // Take metadata from the annotations pull returns, not the pre-check ones:
    // the tag may have been repushed between the two resolves.
    match store.pull(tag, dir).await {
        Ok((_idx, ann)) => {
            logger.errf(
                Color::Magenta,
                &format!("task: {task_name:?} restored from cache\n"),
            );
            (true, meta_from_annotations(&ann))
        }
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            (false, CacheMeta::default())
        }
    }
}

fn meta_from_annotations(ann: &std::collections::BTreeMap<String, String>) -> CacheMeta {
    CacheMeta {
        task: ann.get(oci::ANN_TASK).cloned().unwrap_or_default(),
        sources: ann.get(oci::ANN_SOURCES).cloned().unwrap_or_default(),
        generates: ann.get(oci::ANN_GENERATES).cloned().unwrap_or_default(),
    }
}

/// Export a task's generated files to the cache. `temp_dir` is the fingerprint
/// state directory; the task's up-to-date status and file list are derived from
/// it. A no-op when the task is not up-to-date or has no generated files.
pub async fn cache_save(
    url: &CacheUrl,
    task: &Task,
    dir: &Path,
    temp_dir: &str,
    logger: &mut Logger,
) {
    let task_name = task.name().to_string();
    let mut checker = ChecksumChecker::new(temp_dir, task.clone());
    let st = match checker.status() {
        Ok(st) if st.up_to_date && !st.cache_files.is_empty() => st,
        Ok(_) => return,
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache save {task_name:?}: {e}\n"),
            );
            return;
        }
    };
    let source_value = checker.source_value().to_string();

    match url {
        CacheUrl::Zip { path } => {
            save_file(Path::new(path), &task_name, dir, &source_value, &st, logger);
        }
        CacheUrl::Oci { repo, tag, opts } => {
            save_oci(
                repo,
                tag,
                opts.clone(),
                &task_name,
                dir,
                &source_value,
                &st,
                logger,
            )
            .await;
        }
        CacheUrl::Redis => {
            logger.verbose_errf(
                Color::Yellow,
                "task: redis cache not yet supported in the Rust port\n",
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn save_file(
    dest: &Path,
    task_name: &str,
    dir: &Path,
    source_value: &str,
    st: &crate::fingerprint::TaskStatus,
    logger: &mut Logger,
) {
    if archive::archive_matches(dir, dest, &st.cache_files) {
        return;
    }
    let meta = CacheMeta {
        task: task_name.to_string(),
        sources: source_value.to_string(),
        generates: st.generates_hash.clone(),
    };
    let tmp = temp_zip_path();
    if let Err(e) = archive::write_archive(&tmp, dir, &st.cache_files, &meta) {
        let _ = std::fs::remove_file(&tmp);
        logger.verbose_errf(
            Color::Yellow,
            &format!("task: cache save {task_name:?}: {e}\n"),
        );
        return;
    }
    if let Some(parent) = dest.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        let _ = std::fs::remove_file(&tmp);
        logger.verbose_errf(Color::Yellow, &format!("task: cache save mkdir: {e}\n"));
        return;
    }
    if let Err(e) = rename_or_copy(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        logger.verbose_errf(Color::Yellow, &format!("task: cache save {dest:?}: {e}\n"));
        return;
    }
    logger.verbose_errf(
        Color::Magenta,
        &format!("task: {task_name:?} saved to cache\n"),
    );
}

#[allow(clippy::too_many_arguments)]
async fn save_oci(
    repo: &str,
    tag: &str,
    opts: ocicas::RemoteOptions,
    task_name: &str,
    dir: &Path,
    source_value: &str,
    st: &crate::fingerprint::TaskStatus,
    logger: &mut Logger,
) {
    let store = match ocicas::Store::open(repo, opts).await {
        Ok(s) => s,
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache save {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            return;
        }
    };
    // A tag already carrying the same generates checksum is left untouched.
    if let Ok(Some(ann)) = store.resolve_annotations(tag).await
        && ann.get(oci::ANN_GENERATES).map(String::as_str) == Some(st.generates_hash.as_str())
    {
        return;
    }

    let mut rels = Vec::with_capacity(st.cache_files.len());
    for f in &st.cache_files {
        let p = Path::new(f);
        let rel = if p.is_absolute() {
            match p.strip_prefix(dir) {
                Ok(r) => r.to_path_buf(),
                Err(_) => {
                    logger.verbose_errf(
                        Color::Yellow,
                        &format!("task: cache save {task_name:?}: {f:?} is outside {dir:?}\n"),
                    );
                    return;
                }
            }
        } else {
            p.to_path_buf()
        };
        rels.push(rel.to_string_lossy().replace('\\', "/"));
    }

    let mut ann = std::collections::BTreeMap::new();
    ann.insert(oci::ANN_TASK.to_string(), task_name.to_string());
    ann.insert(oci::ANN_SOURCES.to_string(), source_value.to_string());
    ann.insert(oci::ANN_GENERATES.to_string(), st.generates_hash.clone());

    match store.push(tag, dir, &rels, ann).await {
        Ok(stats) => {
            let total = stats.pushed.saturating_add(stats.skipped);
            let mb = stats.bytes as f64 / 1e6;
            logger.errf(
                Color::Magenta,
                &format!(
                    "task: {task_name:?} saved to cache (pushed {}/{} chunks, {:.1} MB)\n",
                    stats.pushed, total, mb
                ),
            );
        }
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache save {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
        }
    }
}

/// A unique temp path for a cache zip in the system temp directory.
fn temp_zip_path() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("task-cache-{}-{id}.zip", std::process::id()))
}

/// Move `src` to `dst`, falling back to copy+remove when they are on different
/// filesystems (temp dir vs. project dir).
fn rename_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dst)?;
            std::fs::remove_file(src)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_and_oci_and_redis() {
        // SAFETY: single-threaded test setup for the default CAS dir.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        assert!(matches!(
            CacheUrl::parse("file:///tmp/x.zip"),
            Ok(Some(CacheUrl::Zip { .. }))
        ));
        assert!(matches!(
            CacheUrl::parse("oci://host/repo:tag"),
            Ok(Some(CacheUrl::Oci { .. }))
        ));
        assert!(matches!(
            CacheUrl::parse("redis://localhost/x"),
            Ok(Some(CacheUrl::Redis))
        ));
        assert!(matches!(CacheUrl::parse(""), Ok(None)));
        assert!(CacheUrl::parse("ftp://host/x").is_err());
    }

    #[test]
    fn cache_host_extracts_leading_segment() {
        assert_eq!(cache_host("host/repo:tag"), "host");
        assert_eq!(cache_host("host"), "host");
        assert_eq!(cache_host("host:5000/repo"), "host:5000");
    }
}
