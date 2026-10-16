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
pub mod vk;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::ast::Task;
use crate::fingerprint::{CacheMatcher, ChecksumChecker};
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

/// Report a failure to save/push a task's cache entry as a visible warning so a
/// silently disabled cache does not go unnoticed. An unreachable registry keeps
/// its once-per-host dedup (a build pushes many tasks); every other failure is
/// warned per occurrence, since those are rarer and task-specific.
fn warn_cache_save_failed(logger: &mut Logger, repo: &str, task_name: &str, err: &ocicas::Error) {
    if err.is_unreachable() {
        warn_cache_unreachable(logger, repo, err);
    } else {
        logger.errf(
            Color::Yellow,
            &format!("task: WARNING: cache save {task_name:?} failed: {err}\n"),
        );
    }
}

/// A restored cache entry: its metadata, and what it wrote.
pub struct Restored {
    pub meta: CacheMeta,
    pub written: ocicas::restore::Written,
}

/// Attempt to download and restore a cached entry for `task` into `dir`. On
/// success returns what was restored; the caller verifies the generates
/// checksum against the restored files, and undoes the restore if it does not
/// match. A miss or any recoverable failure returns `None`, having written
/// nothing; an error means a restore failed partway and could not be undone,
/// so the task must not run. `source_value` is the task's current sources checksum: an
/// entry cached for other sources, or for another task, is refused before
/// anything is written, as is one holding files outside the task's generates.
pub async fn cache_restore(
    url: &CacheUrl,
    task: &Task,
    source_value: &str,
    dir: &Path,
    guarded: &Guarded<'_>,
    logger: &mut Logger,
) -> Result<Option<Restored>, CacheError> {
    recover_restores(guarded.temp_dir, logger);
    let restore = TaskRestore::new(task, source_value, dir, guarded);
    match url {
        CacheUrl::Zip { path } => restore_file(Path::new(path), task, &restore, dir, logger),
        CacheUrl::Oci { repo, tag, opts } => {
            restore_oci(repo, tag, opts.clone(), task, &restore, dir, logger).await
        }
        CacheUrl::Redis => {
            logger.verbose_errf(
                Color::Yellow,
                "task: redis cache not yet supported in the Rust port\n",
            );
            Ok(None)
        }
    }
}

/// What a task's restore accepts, the same for every transport: an entry
/// cached for this task and these sources, with a generates checksum, holding
/// only the task's generates. Both transports write an entry from those alone
/// (see [`cache_save`]), so anything else is not one Task wrote.
/// What a cached link may never lead to besides `.git` (see
/// [`archive::RestorePolicy::protected`]): Task's own state, and the Taskfiles
/// it read, which a command writing through the link would rewrite.
pub struct Guarded<'a> {
    /// The fingerprint temp dir.
    pub temp_dir: &'a str,
    /// Every Taskfile and dotenv file read.
    pub taskfiles: &'a [String],
}

/// Where restores journal the originals they move aside, under the temp dir.
pub fn journal_dir(temp_dir: &str) -> std::path::PathBuf {
    Path::new(temp_dir).join("restore")
}

/// Clean up after restores cut short (see [`ocicas::restore::recover`]),
/// reporting what was done.
pub fn recover_restores(temp_dir: &str, logger: &mut Logger) {
    for done in ocicas::restore::recover(&journal_dir(temp_dir)) {
        logger.errf(Color::Yellow, &format!("task: WARNING: {done}\n"));
    }
}

/// How many entries [`TaskRestore::protects`] looks at below a link's target
/// for a `.git` directory before giving up, and refusing the link.
const PROTECT_WALK_BUDGET: usize = 100_000;

struct TaskRestore {
    /// The task's `cache: outside_links`.
    outside_links: bool,
    local_name: String,
    source_value: String,
    matcher: CacheMatcher,
    root: String,
    /// The temp dir and the Taskfiles, relative to the root (those under it).
    temp_dir: Option<Vec<String>>,
    taskfiles: Vec<Vec<String>>,
    journal: std::path::PathBuf,
}

/// `path`'s components relative to `root`, if it lies under it.
fn rel_parts(root: &Path, path: &str) -> Option<Vec<String>> {
    let path = Path::new(path);
    let rel = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    Some(
        rel.components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect(),
    )
}

impl TaskRestore {
    fn new(task: &Task, source_value: &str, dir: &Path, guarded: &Guarded<'_>) -> Self {
        Self {
            temp_dir: rel_parts(dir, guarded.temp_dir),
            journal: journal_dir(guarded.temp_dir),
            taskfiles: guarded
                .taskfiles
                .iter()
                .filter_map(|t| rel_parts(dir, t))
                .collect(),
            outside_links: task
                .cache
                .as_ref()
                .and_then(|c| c.outside_links)
                .unwrap_or(false),
            local_name: task.local_name(),
            source_value: source_value.to_string(),
            matcher: CacheMatcher::new(&task.compute_dir().to_string_lossy(), &task.generates),
            root: dir.to_string_lossy().into_owned(),
        }
    }

    fn check_meta(&self, meta: &CacheMeta) -> Result<(), CacheError> {
        let (local_name, source_value) = (&self.local_name, &self.source_value);
        if meta.generates.is_empty() {
            return Err(CacheError::rejected("it has no generates checksum"));
        }
        // Both are always recorded on save; one missing is not an entry Task
        // wrote for this task.
        if meta.task != *local_name {
            return Err(CacheError::rejected(format!(
                "task name mismatch: cached {:?}, expected {local_name:?}",
                meta.task
            )));
        }
        if meta.sources.is_empty() {
            return Err(CacheError::rejected("it has no sources checksum"));
        }
        if meta.sources != *source_value {
            return Err(CacheError::rejected(format!(
                "sources checksum mismatch: cached {}, got {source_value}",
                meta.sources
            )));
        }
        Ok(())
    }

    fn accept(&self, rel: &str) -> bool {
        self.matcher.matches(&self.root, rel)
    }

    /// Why a link may not lead to `dest`, if it may not: it is, or holds, a
    /// path a command writing through the link must never rewrite — the root
    /// itself, a `.git` directory or file at any depth, the temp dir, a
    /// Taskfile or dotenv file Task read, the default Taskfile names and
    /// `.taskrc.yml`/`.taskrc.yaml` in the root.
    fn protects(&self, root: &std::fs::File, dest: &[String]) -> Option<String> {
        let same = |a: &String, b: &String| a.eq_ignore_ascii_case(b);
        // `dest` is `p`, or a directory above it.
        let covers =
            |p: &[String]| dest.len() <= p.len() && dest.iter().zip(p).all(|(a, b)| same(a, b));
        let within =
            |p: &[String]| dest.len() >= p.len() && p.iter().zip(dest).all(|(a, b)| same(a, b));
        let root_file = |name: &str| matches!(dest, [d] if d.eq_ignore_ascii_case(name));
        if dest.is_empty() {
            return Some("the root directory".into());
        }
        if dest.iter().any(|c| archive::is_git_name(c)) {
            return Some("inside a .git directory".into());
        }
        if self
            .temp_dir
            .as_deref()
            .is_some_and(|t| covers(t) || within(t))
        {
            return Some("Task's temp directory, or one holding it".into());
        }
        if let Some(f) = self.taskfiles.iter().find(|t| covers(t)) {
            return Some(format!(
                "{:?}, or a directory holding it, which Task reads",
                f.join("/")
            ));
        }
        if crate::reader::DEFAULT_TASKFILES
            .iter()
            .chain(&[".taskrc.yml", ".taskrc.yaml"])
            .any(|n| root_file(n))
        {
            return Some("a file Task reads".into());
        }
        match ocicas::restore::find_below(root, dest, PROTECT_WALK_BUDGET, archive::is_git_name) {
            Ok(ocicas::restore::Found::Nothing) => None,
            Ok(ocicas::restore::Found::Hit(git)) => {
                Some(format!("which holds the repository {:?}", git.join("/")))
            }
            Ok(ocicas::restore::Found::TooMany) => Some(format!(
                "which holds more than {PROTECT_WALK_BUDGET} entries to check for a .git directory"
            )),
            Err(e) => Some(format!(
                "which could not be checked for a .git directory: {e}"
            )),
        }
    }

    /// The restore policy for this task, given its meta and path checks.
    fn policy<'a>(
        &'a self,
        check: Option<&'a archive::MetaCheck<'a>>,
        accept: &'a dyn Fn(&str) -> bool,
        protected: &'a archive::Protected<'a>,
    ) -> archive::RestorePolicy<'a> {
        archive::RestorePolicy {
            check_meta: check,
            accept: Some(accept),
            outside_links: self.outside_links,
            protected: Some(protected),
            journal: Some(&self.journal),
        }
    }
}

/// Warn on rejected entries, which are not what Task wrote; report other
/// restore failures only at verbose level.
fn report_restore_error(logger: &mut Logger, task_name: &str, what: &str, e: &CacheError) {
    if let CacheError::Rejected(e) = e {
        logger.errf(
            Color::Yellow,
            &format!(
                "task: WARNING: cache for {task_name:?} rejected: {e}, running task normally\n"
            ),
        );
    } else {
        logger.verbose_errf(Color::Yellow, &format!("task: {what}: {e}\n"));
    }
}

fn restore_file(
    zip_path: &Path,
    task: &Task,
    restore: &TaskRestore,
    dir: &Path,
    logger: &mut Logger,
) -> Result<Option<Restored>, CacheError> {
    let task_name = task.name();
    if !zip_path.exists() {
        return Ok(None); // miss
    }
    let check = |m: &CacheMeta| restore.check_meta(m);
    let accept = |rel: &str| restore.accept(rel);
    let protected = |root: &std::fs::File, dest: &[String]| restore.protects(root, dest);
    let policy = restore.policy(Some(&check), &accept, &protected);
    match archive::extract_archive(zip_path, dir, &policy) {
        Ok(archive::Extracted { meta, written }) => {
            logger.errf(
                Color::Magenta,
                &format!("task: {task_name:?} restored from cache\n"),
            );
            Ok(Some(Restored { meta, written }))
        }
        Err(e @ CacheError::Unrecoverable(_)) => Err(e),
        Err(e) => {
            report_restore_error(logger, task_name, "cache extract", &e);
            Ok(None)
        }
    }
}

/// Check an OCI entry's annotations and file list against `restore`, as
/// [`archive::extract_archive`] checks a zip's, before anything is fetched.
fn vet_oci(
    restore: &TaskRestore,
    dir: &Path,
    meta: &CacheMeta,
    index: &ocicas::Index,
) -> Result<(), CacheError> {
    restore.check_meta(meta)?;
    let accept = |rel: &str| restore.accept(rel);
    let protected = |root: &std::fs::File, dest: &[String]| restore.protects(root, dest);
    let policy = restore.policy(None, &accept, &protected);
    let mut set = std::collections::BTreeMap::new();
    for f in &index.files {
        let parts = ocicas::restore::components(&f.path, false)?;
        policy.check_entry(&f.path, &parts)?;
        set.insert(
            parts,
            if f.symlink.is_empty() {
                ocicas::restore::Pending::File
            } else {
                ocicas::restore::Pending::Link(f.symlink.as_bytes().to_vec())
            },
        );
    }
    ocicas::restore::check_unique(set.keys().map(Vec::as_slice))
        .map_err(|e| CacheError::rejected(e.to_string()))?;
    if set.len() != index.files.len() {
        return Err(CacheError::rejected("the entry names one file twice"));
    }
    policy.check_links(dir, &set)
}

async fn restore_oci(
    repo: &str,
    tag: &str,
    opts: ocicas::RemoteOptions,
    task: &Task,
    restore: &TaskRestore,
    dir: &Path,
    logger: &mut Logger,
) -> Result<Option<Restored>, CacheError> {
    let task_name = task.name();
    let store = match ocicas::Store::open(repo, opts).await {
        Ok(s) => s,
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            return Ok(None);
        }
    };
    match store.resolve_annotations(tag).await {
        Ok(None) => return Ok(None), // miss
        Ok(Some(_)) => {}
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            return Ok(None);
        }
    }
    // Vet the annotations and the file list the index fetch returns — not the
    // pre-check's, as the tag may have been repushed in between — before any
    // chunk is fetched or file written.
    let pulled = match store.pull_index(tag).await {
        Ok(p) => p,
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            return Ok(None);
        }
    };
    let meta = meta_from_annotations(&pulled.annotations);
    if let Err(e) = vet_oci(restore, dir, &meta, &pulled.index) {
        report_restore_error(
            logger,
            task_name,
            &format!("cache restore {task_name:?}"),
            &e,
        );
        return Ok(None);
    }
    match store
        .assemble_pulled(&pulled, dir, Some(&restore.journal))
        .await
    {
        Ok(written) => {
            logger.errf(
                Color::Magenta,
                &format!("task: {task_name:?} restored from cache\n"),
            );
            Ok(Some(Restored { meta, written }))
        }
        Err(ocicas::Error::Unrecoverable(m)) => Err(CacheError::Unrecoverable(m)),
        Err(e) => {
            logger.verbose_errf(
                Color::Yellow,
                &format!("task: cache restore {task_name:?}: {e}\n"),
            );
            warn_cache_unreachable(logger, repo, &e);
            Ok(None)
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

/// Export a task's generated files to the cache, with the hashes the run just
/// `recorded` as up to date. A no-op when the task has no generated files.
pub async fn cache_save(
    url: &CacheUrl,
    task: &Task,
    recorded: &crate::fingerprint::Recorded,
    dir: &Path,
    guarded: &Guarded<'_>,
    logger: &mut Logger,
) {
    let temp_dir = guarded.temp_dir;
    let task_name = task.name().to_string();
    // The annotation names the task as its own Taskfile does — the identity
    // the entry is keyed by — so a copy reached through another include
    // verifies against it.
    let entry_task = task.local_name();
    let mut checker = ChecksumChecker::new(temp_dir, task.clone());
    // The hashes the run just recorded, not a fresh reading.
    let st = match checker.cache_files() {
        Ok(cache_files) if !cache_files.is_empty() => crate::fingerprint::TaskStatus {
            up_to_date: true,
            sources_hash: recorded.sources.clone(),
            generates_hash: recorded.generates.clone(),
            cache_files,
            ..Default::default()
        },
        Ok(_) => return,
        Err(e) => {
            logger.errf(
                Color::Yellow,
                &format!("task: WARNING: cache save {task_name:?} failed: {e}\n"),
            );
            return;
        }
    };
    let source_value = checker.source_value().to_string();
    let (files, refused) = archive::split_unrestorable(dir, &st.cache_files);
    if !refused.is_empty() {
        logger.errf(
            Color::Yellow,
            &format!(
                "task: WARNING: cache save {task_name:?}: leaving out {} file(s) inside .git or named so no restore can write them\n",
                refused.len()
            ),
        );
    }
    if files.is_empty() {
        return;
    }
    // An entry whose links its restore would refuse would only ever miss.
    let restore = TaskRestore::new(task, &source_value, dir, guarded);
    let accept = |_: &str| true;
    let protected = |root: &std::fs::File, dest: &[String]| restore.protects(root, dest);
    if let Err(e) = restore
        .policy(None, &accept, &protected)
        .check_links(dir, &archive::links_of(dir, &files))
    {
        logger.errf(
            Color::Yellow,
            &format!("task: WARNING: cache save {task_name:?} skipped: {e}\n"),
        );
        return;
    }
    let st = crate::fingerprint::TaskStatus {
        cache_files: files,
        ..st
    };

    match url {
        CacheUrl::Zip { path } => {
            let dest = Path::new(path);
            save_file(dest, &entry_task, dir, &source_value, &st, logger);
        }
        CacheUrl::Oci { repo, tag, opts } => {
            save_oci(
                repo,
                tag,
                opts.clone(),
                &entry_task,
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
        logger.errf(
            Color::Yellow,
            &format!("task: WARNING: cache save {task_name:?} failed: {e}\n"),
        );
        return;
    }
    if let Some(parent) = dest.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        let _ = std::fs::remove_file(&tmp);
        logger.errf(
            Color::Yellow,
            &format!("task: WARNING: cache save {task_name:?} failed: mkdir {parent:?}: {e}\n"),
        );
        return;
    }
    if let Err(e) = rename_or_copy(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        logger.errf(
            Color::Yellow,
            &format!("task: WARNING: cache save {task_name:?} failed: {dest:?}: {e}\n"),
        );
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
            warn_cache_save_failed(logger, repo, task_name, &e);
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
                    logger.errf(
                        Color::Yellow,
                        &format!(
                            "task: WARNING: cache save {task_name:?} failed: {f:?} is outside {dir:?}\n"
                        ),
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
            warn_cache_save_failed(logger, repo, task_name, &e);
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

    fn oci_index(paths: &[&str]) -> ocicas::Index {
        let mut idx: ocicas::Index = serde_json::from_value(serde_json::json!({
            "version": ocicas::INDEX_VERSION,
            "chunker": {"algo": "", "min": 0, "avg": 0, "max": 0},
            "compression": {"algo": "zstd", "level": 3},
            "files": [],
        }))
        .unwrap();
        for p in paths {
            idx.files.push(ocicas::FileEntry {
                path: (*p).to_string(),
                mode: 0o644,
                size: 0,
                symlink: String::new(),
            });
        }
        idx
    }

    #[test]
    fn oci_restore_takes_only_the_tasks_generates() {
        let root = std::env::temp_dir().join(format!("task-oci-vet-{}", std::process::id()));
        std::fs::create_dir_all(root.join("packages/lib")).unwrap();
        let taskfiles = [root.join("Taskfile.yml").to_string_lossy().into_owned()];
        let temp_dir = root.join(".task").to_string_lossy().into_owned();
        let guarded = Guarded {
            temp_dir: &temp_dir,
            taskfiles: &taskfiles,
        };
        let task = Task {
            task: "build".into(),
            dirs: vec![root.to_string_lossy().into_owned()],
            generates: vec![crate::ast::Glob {
                glob: "out/**".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let restore = TaskRestore::new(&task, "src1", &root, &guarded);
        let meta = CacheMeta {
            task: "build".into(),
            sources: "src1".into(),
            generates: "gen".into(),
        };
        assert!(vet_oci(&restore, &root, &meta, &oci_index(&["out/a", "out/b/c"])).is_ok());
        let mut linked = oci_index(&["out/a"]);
        linked.files.push(ocicas::FileEntry {
            path: "out/link".into(),
            mode: 0o777,
            size: 0,
            symlink: "/etc/passwd".into(),
        });
        let err = vet_oci(&restore, &root, &meta, &linked).unwrap_err();
        assert!(matches!(err, CacheError::Rejected(_)), "{err}");
        let mut trusting = task.clone();
        trusting.cache = Some(crate::ast::Cache {
            outside_links: Some(true),
            ..Default::default()
        });
        let trusting = TaskRestore::new(&trusting, "src1", &root, &guarded);
        assert!(vet_oci(&trusting, &root, &meta, &linked).is_ok());
        // Links that stay in the project, but would let the task's commands
        // rewrite a Taskfile, git state or Task's own state: refused even with
        // `outside_links`. A pnpm-style link into the workspace is fine.
        let link_to = |target: &str| {
            let mut idx = oci_index(&["out/a"]);
            idx.files.push(ocicas::FileEntry {
                path: "out/nm/link".into(),
                mode: 0o777,
                size: 0,
                symlink: target.into(),
            });
            idx
        };
        for target in [
            "../../Taskfile.yml",
            "../../taskfile.yaml",
            "../../.git/hooks",
            "../../.git/info/exclude",
            "../../.task",
            "../..",
        ] {
            for r in [&restore, &trusting] {
                let err = vet_oci(r, &root, &meta, &link_to(target)).unwrap_err();
                assert!(matches!(err, CacheError::Rejected(_)), "{target}: {err}");
            }
        }
        assert!(vet_oci(&restore, &root, &meta, &link_to("../../packages/lib")).is_ok());
        // Directories holding something protected, however deep: a nested
        // repository, an included Taskfile, a dotenv file, `.taskrc.yml`.
        std::fs::create_dir_all(root.join("vendor/deep/repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("packages/withtf")).unwrap();
        std::fs::write(root.join("packages/withtf/Taskfile.yml"), "x").unwrap();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::write(root.join("conf/.env"), "A=1").unwrap();
        let guarded2_files = [
            root.join("Taskfile.yml").to_string_lossy().into_owned(),
            root.join("packages/withtf/Taskfile.yml")
                .to_string_lossy()
                .into_owned(),
            root.join("conf/.env").to_string_lossy().into_owned(),
        ];
        let guarded2 = Guarded {
            temp_dir: &temp_dir,
            taskfiles: &guarded2_files,
        };
        let restore2 = TaskRestore::new(&task, "src1", &root, &guarded2);
        for target in [
            "../../vendor",
            "../../vendor/deep",
            "../../packages/withtf",
            "../../conf",
            "../../conf/.env",
            "../../.taskrc.yml",
        ] {
            let err = vet_oci(&restore2, &root, &meta, &link_to(target)).unwrap_err();
            assert!(matches!(err, CacheError::Rejected(_)), "{target}: {err}");
        }
        assert!(vet_oci(&restore2, &root, &meta, &link_to("../../packages/lib")).is_ok());
        // One destination named twice.
        let mut twice = oci_index(&["out/a"]);
        twice.files.push(ocicas::FileEntry {
            path: "out/a".into(),
            mode: 0o777,
            size: 0,
            symlink: "/etc/passwd".into(),
        });
        let err = vet_oci(&trusting, &root, &meta, &twice).unwrap_err();
        assert!(
            matches!(err, CacheError::Rejected(_) | CacheError::Ocicas(_)),
            "{err}"
        );
        for bad in [
            "Taskfile.yml",
            "out/.git/hooks/x",
            ".git/config",
            ".task/checksum/x",
        ] {
            let err = vet_oci(&restore, &root, &meta, &oci_index(&["out/a", bad])).unwrap_err();
            assert!(matches!(err, CacheError::Rejected(_)), "{bad}: {err}");
        }
        for wrong in [
            CacheMeta {
                task: "other".into(),
                ..meta.clone()
            },
            CacheMeta {
                sources: "src2".into(),
                ..meta.clone()
            },
            CacheMeta {
                generates: String::new(),
                ..meta.clone()
            },
            CacheMeta {
                task: String::new(),
                ..meta.clone()
            },
            CacheMeta {
                sources: String::new(),
                ..meta.clone()
            },
        ] {
            let err = vet_oci(&restore, &root, &wrong, &oci_index(&["out/a"])).unwrap_err();
            assert!(matches!(err, CacheError::Rejected(_)), "{wrong:?}: {err}");
        }
    }

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
