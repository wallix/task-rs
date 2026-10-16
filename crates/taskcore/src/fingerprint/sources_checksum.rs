//! The checksum-based up-to-date checker.

use std::path::Path;

use serde::Serialize;

use crate::ast::{Cmd, Glob, Task};
use crate::filepathext;

use super::checksum::{Datum, Snapshot, hinted_checksum, hinted_snapshot};
use super::glob::{cache_globs, globs, hinted_globs};
use super::offload::off_thread;
use super::pool::Workers;

/// Validates whether a task is up to date by comparing a checksum of its source
/// files against a stored value. It is bound to a single task and precomputes
/// source metadata at construction time.
pub struct ChecksumChecker {
    temp_dir: String,
    task: Task,
    spec: Spec,
    /// From `task.source_hash` or lazily computed; empty when the task has no
    /// sources.
    source_hash: String,
    /// The sources as `is_up_to_date` read them, before execution: what a
    /// [`PostRun`] is compared against to detect drift and name its files.
    pre_exec: Option<Snapshot>,
}

/// The sources and generates as read once after a run, by
/// [`ChecksumChecker::post_run`]: what the drift check compares and what
/// [`ChecksumChecker::publish`] records, so neither is read again.
#[derive(Debug)]
pub struct PostRun {
    sources: Option<Snapshot>,
    /// Kept as a result: an unreadable generate fails only a publish.
    generates: std::io::Result<String>,
}

/// The hashes a successful run recorded, for the cache entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    pub sources: String,
    pub generates: String,
}

/// The full fingerprint state for a task, including which parts are up to date.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TaskStatus {
    pub task: String,
    pub up_to_date: bool,
    pub sources_up_to_date: bool,
    pub generates_up_to_date: bool,
    pub checksum_file: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sources_hash: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_data: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub generates_hash: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub generate_files: Vec<String>,
    /// Files to include in cache (full glob, ignoring fingerprint).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cache_files: Vec<String>,
}

/// What a reading of a task's fingerprint needs: owned, so a reading can run
/// on another thread ([`super::offload`]) while the checker stays on the
/// engine.
#[derive(Clone, Debug)]
struct Spec {
    dir: String,
    has_sources: bool,
    sources: Vec<Glob>,
    data: Vec<Datum>,
    generates: Vec<Glob>,
}

// Readings move these across threads.
const _: fn() = || {
    fn assert_send<T: Send + 'static>() {}
    assert_send::<Spec>();
    assert_send::<Snapshot>();
    assert_send::<PostRun>();
    assert_send::<TaskStatus>();
};

impl Spec {
    fn sources_snapshot(&self) -> std::io::Result<Snapshot> {
        super::passes::note(&self.dir);
        let files = hinted_globs(&self.dir, &self.sources)?;
        hinted_snapshot(&self.dir, &files, &self.data, Workers::Auto)
    }

    fn generates_checksum(&self) -> std::io::Result<String> {
        let files = hinted_globs(&self.dir, &self.generates)?;
        hinted_checksum(&self.dir, &files, &[])
    }

    /// The sources and the stored state compared at the up-to-date check.
    fn up_to_date(&self, checksum_file: &str) -> std::io::Result<(Snapshot, bool)> {
        let sources = self.sources_snapshot()?;
        let stored = std::fs::read_to_string(checksum_file).unwrap_or_default();
        let (old_sources, old_generates) = split_hashes(&stored);
        let generates = self.generates_checksum()?;
        let ok = old_sources == sources.hash && old_generates == generates;
        Ok((sources, ok))
    }

    fn post_run(&self) -> std::io::Result<PostRun> {
        if !self.has_sources {
            return Ok(PostRun {
                sources: None,
                generates: Ok(String::new()),
            });
        }
        let sources = self.sources_snapshot()?;
        Ok(PostRun {
            sources: Some(sources),
            generates: self.generates_checksum(),
        })
    }

    fn status(&self, task: String, checksum_file: String) -> std::io::Result<TaskStatus> {
        let stored = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        let (old_sources_hash, old_generates_hash) = split_hashes(&stored);

        let current_sources_hash = self.sources_snapshot()?.hash;
        let new_generates_hash = self.generates_checksum()?;

        let sources_files = globs(&self.dir, &self.sources).unwrap_or_default();
        let generates = globs(&self.dir, &self.generates).unwrap_or_default();
        let cache_files = cache_globs(&self.dir, &self.generates).unwrap_or_default();

        let src_ok = old_sources_hash == current_sources_hash;
        let gen_ok = old_generates_hash == new_generates_hash;

        Ok(TaskStatus {
            task,
            up_to_date: src_ok && gen_ok,
            sources_up_to_date: src_ok,
            generates_up_to_date: gen_ok,
            checksum_file,
            sources_hash: old_sources_hash.to_string(),
            source_files: sources_files,
            source_data: self.data.iter().map(Datum::display).collect(),
            generates_hash: old_generates_hash.to_string(),
            generate_files: generates,
            cache_files,
        })
    }
}

impl ChecksumChecker {
    /// Creates a checker bound to `task`. It precomputes the source globs and
    /// metadata but does not access disk for the source hash: it reuses
    /// `task.source_hash` when available (set during compilation). When that is
    /// empty, [`ChecksumChecker::source_value`] computes it lazily.
    pub fn new(temp_dir: impl Into<String>, task: Task) -> Self {
        let dir = task.compute_dir().to_string_lossy().into_owned();
        let (sources, data) = build_checksum_data(&dir, &task);
        ChecksumChecker {
            temp_dir: temp_dir.into(),
            source_hash: task.source_hash.clone(),
            spec: Spec {
                dir,
                has_sources: !task.sources.is_empty(),
                sources,
                data,
                generates: task.generates.clone(),
            },
            task,
            pre_exec: None,
        }
    }

    /// Reports whether the task is up to date: its sources and generates hashes
    /// match the stored values. A task with no sources is never up to date.
    /// Reads off the engine's thread.
    pub async fn is_up_to_date(&mut self) -> std::io::Result<bool> {
        if !self.spec.has_sources {
            return Ok(false);
        }
        let (spec, file) = (self.spec.clone(), self.checksum_file_path()?);
        let (sources, ok) = off_thread(move || spec.up_to_date(&file)).await?;
        // Nothing before needed the sources value: this reading is the run's.
        self.adopt_source_value(&sources.hash);
        self.pre_exec = Some(sources);
        Ok(ok)
    }

    /// Reads the sources and generates once after the run, off the engine's
    /// thread, for [`ChecksumChecker::drifted`] and
    /// [`ChecksumChecker::publish`]. Without sources there is nothing to read.
    /// A sources error is returned; a generates one is kept for the publish.
    pub async fn post_run(&self) -> std::io::Result<PostRun> {
        let spec = self.spec.clone();
        off_thread(move || spec.post_run()).await
    }

    /// Reports whether the sources changed during the run: `post` differs
    /// from what [`ChecksumChecker::is_up_to_date`] read. Never without that
    /// reading (`--force`).
    pub fn drifted(&self, post: &PostRun) -> bool {
        match (&self.pre_exec, &post.sources) {
            (Some(pre), Some(now)) => pre.hash != now.hash,
            _ => false,
        }
    }

    /// Names the sources added, removed or whose content changed during the
    /// run, relative to the task directory and sorted, to point the drift
    /// warning at them. Content-based, not an atomic view of the tree: an
    /// empty result does not disprove drift, which
    /// [`ChecksumChecker::drifted`] alone decides.
    pub fn changed_sources(&self, post: &PostRun) -> Vec<String> {
        match (&self.pre_exec, &post.sources) {
            (Some(pre), Some(now)) => pre.changed(now),
            _ => Vec::new(),
        }
    }

    /// Records `post` as up to date and returns what it recorded; `None`
    /// without sources. Fails on a generates error `post` kept.
    pub fn publish(&self, post: PostRun) -> std::io::Result<Option<Recorded>> {
        let Some(sources) = post.sources else {
            return Ok(None);
        };
        let recorded = Recorded {
            sources: sources.hash,
            generates: post.generates?,
        };
        self.write_state(&recorded.sources, &recorded.generates)?;
        Ok(Some(recorded))
    }

    /// Returns the full fingerprint state for the task, read off the engine's
    /// thread.
    pub async fn status(&self) -> std::io::Result<TaskStatus> {
        let (spec, name, file) = (
            self.spec.clone(),
            self.task.name().to_string(),
            self.checksum_file_path()?,
        );
        off_thread(move || spec.status(name, file)).await
    }

    /// Returns the sources checksum for use as the `CHECKSUM` template variable,
    /// lock keys, and cache keys, computing it lazily from disk (off the
    /// engine's thread) on first call when it was not precomputed during
    /// compilation. A reading error leaves it empty.
    pub async fn source_value(&mut self) -> &str {
        if self.source_hash.is_empty() && self.spec.has_sources {
            let spec = self.spec.clone();
            self.source_hash = off_thread(move || Ok(spec.sources_snapshot()?.hash))
                .await
                .unwrap_or_default();
        }
        &self.source_hash
    }

    /// The sources value as chosen so far, without computing it.
    pub fn known_source_value(&self) -> &str {
        &self.source_hash
    }

    /// Whether the run's sources value is what the up-to-date check read: a
    /// value taken earlier (compile, lock) may predate changes to the sources.
    pub fn source_value_is_the_reading(&self) -> bool {
        self.pre_exec
            .as_ref()
            .is_some_and(|pre| pre.hash == self.source_hash)
    }

    /// Makes `value` the run's sources value unless one was chosen already.
    pub fn adopt_source_value(&mut self, value: &str) {
        if self.source_hash.is_empty() && self.spec.has_sources {
            self.source_hash = value.to_string();
        }
    }

    /// The cache entry and lock checksum (`hash::cache_checksum`): the
    /// compile's, or one from [`ChecksumChecker::source_value`] and the
    /// compiled vars digest. Empty without a sources value.
    pub async fn cache_checksum(&mut self) -> String {
        if !self.task.cache_checksum.is_empty() {
            return self.task.cache_checksum.clone();
        }
        let digest = self.task.cache_vars_digest.clone();
        crate::hash::join_cache_checksum(self.source_value().await, &digest)
    }

    /// Records the current sources and generates hashes as up to date, read
    /// on the calling thread: for tests setting up state.
    #[cfg(test)]
    pub fn set_up_to_date(&self) -> std::io::Result<()> {
        if !self.spec.has_sources {
            return Ok(());
        }
        let new_sources_hash = self.spec.sources_snapshot()?.hash;
        let new_generates_hash = self.spec.generates_checksum()?;
        self.write_state(&new_sources_hash, &new_generates_hash)
    }

    /// Records the task as up to date with the generates hash the caller just
    /// computed and the sources read afresh, off the engine's thread.
    pub async fn set_up_to_date_with_generates(&self, generates: &str) -> std::io::Result<()> {
        if !self.spec.has_sources {
            return Ok(());
        }
        let spec = self.spec.clone();
        let sources = off_thread(move || Ok(spec.sources_snapshot()?.hash)).await?;
        self.write_state(&sources, generates)
    }

    fn write_state(&self, sources: &str, generates: &str) -> std::io::Result<()> {
        let checksum_dir = Path::new(&self.temp_dir).join("checksum");
        let _ = std::fs::create_dir_all(&checksum_dir);
        std::fs::write(
            self.checksum_file_path()?,
            format!("{sources}\n{generates}\n"),
        )
    }

    /// The files a cache entry for the task archives, walked off the engine's
    /// thread.
    pub async fn cache_files(&self) -> std::io::Result<Vec<String>> {
        let (dir, generates) = (self.spec.dir.clone(), self.spec.generates.clone());
        off_thread(move || cache_globs(&dir, &generates)).await
    }

    /// Removes the stored checksum after a failed run so the next invocation
    /// re-runs the task.
    pub fn on_error(&self) -> std::io::Result<()> {
        if !self.spec.has_sources {
            return Ok(());
        }
        match std::fs::remove_file(self.checksum_file_path()?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Returns the checker kind identifier.
    pub fn kind(&self) -> &'static str {
        "checksum"
    }

    /// Computes the current generates hash from disk, off the engine's thread.
    pub async fn generates_checksum(&self) -> std::io::Result<String> {
        let spec = self.spec.clone();
        off_thread(move || spec.generates_checksum()).await
    }

    #[cfg(test)]
    fn sources_checksum(&self) -> std::io::Result<String> {
        Ok(self.spec.sources_snapshot()?.hash)
    }

    /// Namespaced copies share state when their compiled build inputs match.
    fn checksum_file_path(&self) -> std::io::Result<String> {
        let digest = crate::hash::fingerprint(&self.task).map_err(std::io::Error::other)?;
        let key = format!("{}-{digest}", normalize_filename(&self.task.local_name()));
        Ok(Path::new(&self.temp_dir)
            .join("checksum")
            .join(key)
            .to_string_lossy()
            .into_owned())
    }
}

/// The sources globs to walk and the data the sources checksum covers.
fn build_checksum_data(dir: &str, task: &Task) -> (Vec<Glob>, Vec<Datum>) {
    let mut sources = Vec::new();
    let mut data = Vec::new();
    for source in &task.sources {
        if let Some(value) = source.glob.strip_prefix("value:") {
            data.push(Datum::Value(value.to_string()));
        } else {
            sources.push(source.clone());
            data.push(Datum::SrcRule {
                group: source.group,
                negate: source.negate,
                glob: rel_glob(dir, &source.glob),
            });
        }
    }
    for (i, cmd) in task.raw_cmds.iter().enumerate() {
        data.push(serialize_cmd(i, cmd));
    }
    for g in &task.generates {
        data.push(Datum::GenRule {
            group: g.group,
            negate: g.negate,
            glob: rel_glob(dir, &g.glob),
        });
    }
    // Shown in this order by `--status`; the checksum sorts by encoding.
    data.sort_by_cached_key(Datum::display);
    (sources, data)
}

fn serialize_cmd(idx: usize, c: &Cmd) -> Datum {
    Datum::Cmd {
        index: idx as u64,
        cmd: c.cmd.clone(),
    }
}

/// Returns `glob` relative to `dir` so checksums are stable across workspace
/// paths (e.g. CI runners with different base directories). The result may
/// start with `../` when the glob is outside `dir`; that is fine because the
/// relative path is still deterministic.
fn rel_glob(dir: &str, glob: &str) -> String {
    filepathext::rel_str(dir, glob).unwrap_or_else(|| glob.to_string())
}

/// Splits stored checksum content into the sources and generates hashes.
fn split_hashes(stored: &str) -> (&str, &str) {
    let trimmed = stored.trim();
    match trimmed.split_once('\n') {
        Some((sources, generates)) => (sources, generates),
        None => (trimmed, ""),
    }
}

/// Replaces characters outside `[A-Za-z0-9]` with `-`, mirroring the Go
/// implementation's regex `[^A-z0-9]`. Note that `A-z` also spans the six ASCII
/// characters between `Z` and `a` (`[ \ ] ^ _ ``), which are therefore
/// preserved, matching Go's behavior exactly.
fn normalize_filename(f: &str) -> String {
    f.chars()
        .map(|c| {
            let preserved =
                c.is_ascii_alphanumeric() || matches!(c, '[' | '\\' | ']' | '^' | '_' | '`');
            if preserved { c } else { '-' }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Glob;
    use crate::fingerprint::testutil::{join, now, tmp, write_file};

    fn new_task(dir: &str, sources: Vec<Glob>, generates: Vec<Glob>) -> Task {
        Task {
            task: "test-task".to_string(),
            dirs: vec![dir.to_string()],
            sources,
            generates,
            ..Default::default()
        }
    }

    fn g(pattern: &str) -> Glob {
        Glob {
            glob: pattern.to_string(),
            ..Default::default()
        }
    }

    fn g_fp(pattern: &str, fingerprint: &str) -> Glob {
        Glob {
            glob: pattern.to_string(),
            fingerprint: fingerprint.to_string(),
            ..Default::default()
        }
    }

    /// Rules carry the group of a folded entry, and only that.
    #[test]
    fn rules_carry_folded_groups() {
        let dir = tmp();
        let mut excluded = g("lib/*.tmp");
        excluded.negate = true;
        excluded.group = 1;
        let task = new_task(&dir, vec![g("main.c"), excluded], vec![g("out/*")]);
        let checker = ChecksumChecker::new(tmp(), task);
        let data = now(checker.status()).unwrap().source_data;
        assert_eq!(
            data,
            ["genrule:out/*", "srcrule:main.c", "srcrule@1:!lib/*.tmp"]
        );
    }

    #[test]
    fn status_with_fingerprint() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "package.json", "{}");
        write_file(&dir, "node_modules/.yarn-state.yml", "state");
        write_file(&dir, "node_modules/vite/bin/vite.js", "vite");
        write_file(&dir, "node_modules/react/index.js", "react");

        let task = new_task(
            &dir,
            vec![g("package.json")],
            vec![g_fp("node_modules/**/*", "node_modules/.yarn-state.yml")],
        );

        let checker = ChecksumChecker::new(tmp_dir, task);
        let st = now(checker.status()).unwrap();

        assert_eq!(
            st.generate_files,
            vec![join(&dir, "node_modules/.yarn-state.yml")]
        );
        assert_eq!(
            st.cache_files,
            vec![
                join(&dir, "node_modules/.yarn-state.yml"),
                join(&dir, "node_modules/react/index.js"),
                join(&dir, "node_modules/vite/bin/vite.js"),
            ]
        );
    }

    #[test]
    fn status_without_fingerprint() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "main.go", "package main");
        write_file(&dir, "build/app", "binary");
        write_file(&dir, "build/app.map", "map");

        let task = new_task(&dir, vec![g("main.go")], vec![g("build/**/*")]);
        let checker = ChecksumChecker::new(tmp_dir, task);
        let st = now(checker.status()).unwrap();

        let expected = vec![join(&dir, "build/app"), join(&dir, "build/app.map")];
        assert_eq!(st.generate_files, expected);
        assert_eq!(st.cache_files, expected);
    }

    #[test]
    fn fingerprint_only_hashes_marker() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "package.json", "{}");
        write_file(&dir, "node_modules/.yarn-state.yml", "state-v1");
        write_file(&dir, "node_modules/pkg/index.js", "v1");

        let task = new_task(
            &dir,
            vec![g("package.json")],
            vec![g_fp("node_modules/**/*", "node_modules/.yarn-state.yml")],
        );

        ChecksumChecker::new(&tmp_dir, task.clone())
            .set_up_to_date()
            .unwrap();

        let mut checker2 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(now(checker2.is_up_to_date()).unwrap());

        // Non-marker change: still up to date.
        write_file(&dir, "node_modules/pkg/index.js", "v2-changed");
        let mut checker3 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(now(checker3.is_up_to_date()).unwrap());

        // Marker change: out of date.
        write_file(&dir, "node_modules/.yarn-state.yml", "state-v2");
        let mut checker4 = ChecksumChecker::new(&tmp_dir, task);
        assert!(!now(checker4.is_up_to_date()).unwrap());
    }

    #[test]
    fn up_to_date_transitions_and_source_change_detection() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "one");

        let task = new_task(&dir, vec![g("src.txt")], vec![]);

        // No stored checksum yet.
        let mut checker = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(!now(checker.is_up_to_date()).unwrap());

        // Record and re-check.
        checker.set_up_to_date().unwrap();
        let mut checker2 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(now(checker2.is_up_to_date()).unwrap());
        // No drift after the snapshot was taken.
        assert!(!checker2.drifted(&now(checker2.post_run()).unwrap()));

        // Modify the source: out of date, and drift detected relative to the
        // snapshot taken above, naming the changed file.
        write_file(&dir, "src.txt", "two");
        let post = now(checker2.post_run()).unwrap();
        assert!(checker2.drifted(&post));
        assert_eq!(checker2.changed_sources(&post), vec!["src.txt"]);
        let mut checker3 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(!now(checker3.is_up_to_date()).unwrap());

        // on_error clears the stored checksum.
        checker.set_up_to_date().unwrap();
        checker.on_error().unwrap();
        let mut checker4 = ChecksumChecker::new(&tmp_dir, task);
        assert!(!now(checker4.is_up_to_date()).unwrap());
    }

    #[test]
    fn changed_sources_names_added_removed_and_modified() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "a.txt", "one");
        write_file(&dir, "b.txt", "two");

        // Model a sources glob that also matches generated files.
        let task = new_task(&dir, vec![g("*.txt")], vec![]);
        let mut checker = ChecksumChecker::new(&tmp_dir, task);
        checker.set_up_to_date().unwrap();
        assert!(now(checker.is_up_to_date()).unwrap());

        write_file(&dir, "a.txt", "one-longer");
        std::fs::remove_file(join(&dir, "b.txt")).unwrap();
        write_file(&dir, "c.txt", "three");

        let post = now(checker.post_run()).unwrap();
        assert!(checker.drifted(&post));
        assert_eq!(
            checker.changed_sources(&post),
            vec!["a.txt", "b.txt", "c.txt"]
        );
    }

    /// What the run read after the commands is what gets recorded.
    #[test]
    fn publish_records_the_post_run_reading() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "one");
        write_file(&dir, "out.txt", "built");
        let task = new_task(&dir, vec![g("src.txt")], vec![g("out.txt")]);
        let mut checker = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(!now(checker.is_up_to_date()).unwrap());
        let post = now(checker.post_run()).unwrap();
        assert!(!checker.drifted(&post));
        // Changed after the reading: not what is recorded.
        write_file(&dir, "out.txt", "later");
        let recorded = checker.publish(post).unwrap().unwrap();
        assert_eq!(recorded.sources, checker.sources_checksum().unwrap());
        assert_ne!(
            recorded.generates,
            now(checker.generates_checksum()).unwrap()
        );
        let stored = std::fs::read_to_string(checker.checksum_file_path().unwrap()).unwrap();
        assert_eq!(
            stored,
            format!("{}\n{}\n", recorded.sources, recorded.generates)
        );
    }

    /// Without an up-to-date check (`--force`) there is no drift to see, and
    /// the post-run reading is recorded as a fresh one.
    #[test]
    fn forced_run_publishes_without_a_snapshot() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "one");
        let task = new_task(&dir, vec![g("src.txt")], vec![]);
        let checker = ChecksumChecker::new(&tmp_dir, task.clone());
        let post = now(checker.post_run()).unwrap();
        assert!(!checker.drifted(&post));
        assert!(checker.changed_sources(&post).is_empty());
        checker.publish(post).unwrap().unwrap();
        let mut again = ChecksumChecker::new(&tmp_dir, task);
        assert!(now(again.is_up_to_date()).unwrap());
    }

    /// An unreadable generate fails the publish, and only the publish.
    #[cfg(unix)]
    #[test]
    fn generates_error_is_kept_for_the_publish() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "one");
        let fifo = std::ffi::CString::new(join(&dir, "out.p")).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let task = new_task(&dir, vec![g("src.txt")], vec![g("out.p")]);
        let checker = ChecksumChecker::new(&tmp_dir, task);
        let post = now(checker.post_run()).unwrap();
        assert!(!checker.drifted(&post));
        let err = checker.publish(post).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn restore_records_the_generates_it_verified() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "one");
        let task = new_task(&dir, vec![g("src.txt")], vec![]);
        let checker = ChecksumChecker::new(&tmp_dir, task);
        now(checker.set_up_to_date_with_generates("gen")).unwrap();
        let stored = std::fs::read_to_string(checker.checksum_file_path().unwrap()).unwrap();
        assert_eq!(
            stored,
            format!("{}\ngen\n", checker.sources_checksum().unwrap())
        );
    }

    #[test]
    fn no_sources_is_never_up_to_date() {
        let dir = tmp();
        let tmp_dir = tmp();
        let task = new_task(&dir, vec![], vec![]);
        let mut checker = ChecksumChecker::new(tmp_dir, task);
        assert!(!now(checker.is_up_to_date()).unwrap());
        // set_up_to_date, publish and on_error are no-ops with no sources.
        assert!(checker.set_up_to_date().is_ok());
        let post = now(checker.post_run()).unwrap();
        assert!(!checker.drifted(&post));
        assert_eq!(checker.publish(post).unwrap(), None);
        assert!(checker.on_error().is_ok());
    }

    #[test]
    fn source_value_computed_lazily() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "content");
        let task = new_task(&dir, vec![g("src.txt")], vec![]);
        let mut checker = ChecksumChecker::new(tmp_dir, task);
        assert!(!now(checker.source_value()).is_empty());
    }

    #[test]
    fn kind_is_checksum() {
        let checker = ChecksumChecker::new(tmp(), new_task(&tmp(), vec![], vec![]));
        assert_eq!(checker.kind(), "checksum");
    }

    #[test]
    fn normalize_filename_matches_go_regex() {
        // Alphanumerics kept.
        assert_eq!(normalize_filename("abc123"), "abc123");
        // Namespace separator ':' is outside [A-z0-9] -> replaced.
        assert_eq!(normalize_filename("ns:build"), "ns-build");
        // Characters in the A-z gap are preserved, mirroring Go.
        assert_eq!(normalize_filename("a_b"), "a_b");
        assert_eq!(normalize_filename("a.b"), "a-b");
    }

    // Build a compiled-looking task: the local name comes from `full_name` minus
    // `namespace`, and `cmds` carry resolved values (so the digest reflects vars).
    fn included_task(
        dir: &str,
        full_name: &str,
        namespace: &str,
        taskfile: &str,
        cmd: &str,
    ) -> Task {
        Task {
            task: "build".to_string(),
            full_name: full_name.to_string(),
            namespace: namespace.to_string(),
            dirs: vec![dir.to_string()],
            sources: vec![g("src.txt")],
            cmds: vec![Cmd {
                cmd: cmd.to_string(),
                ..Default::default()
            }],
            location: Some(crate::ast::Location {
                taskfile: taskfile.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn key_of(tmp_dir: &str, task: Task) -> String {
        ChecksumChecker::new(tmp_dir.to_string(), task)
            .checksum_file_path()
            .unwrap()
    }

    #[test]
    fn checksum_key_shared_across_include_namespaces() {
        let dir = tmp();
        let tmp_dir = tmp();
        let tf = join(&dir, "leaf.yml");
        // The same task (leaf.yml `build`) reached directly and through a nested
        // include: identical local name, defining file and compiled body.
        let direct = included_task(&dir, "leaf:build", "leaf", &tf, "echo hi");
        let nested = included_task(&dir, "app:leaf:build", "app:leaf", &tf, "echo hi");
        assert_eq!(
            key_of(&tmp_dir, direct),
            key_of(&tmp_dir, nested),
            "same task via two include paths must share one checksum entry"
        );
    }

    #[test]
    fn checksum_key_differs_by_defining_file() {
        let dir = tmp();
        let tmp_dir = tmp();
        let a = included_task(&dir, "a:build", "a", &join(&dir, "a.yml"), "echo hi");
        let b = included_task(&dir, "b:build", "b", &join(&dir, "b.yml"), "echo hi");
        assert_ne!(
            key_of(&tmp_dir, a),
            key_of(&tmp_dir, b),
            "same local name in different files must stay apart"
        );
    }

    #[test]
    fn checksum_key_differs_by_resolved_vars() {
        let dir = tmp();
        let tmp_dir = tmp();
        let tf = join(&dir, "leaf.yml");
        // Same file and local name, but the include passed different vars, so
        // the compiled command differs — they must not share an entry.
        let a = included_task(&dir, "one:build", "one", &tf, "echo A");
        let b = included_task(&dir, "two:build", "two", &tf, "echo B");
        assert_ne!(key_of(&tmp_dir, a), key_of(&tmp_dir, b));
    }

    #[test]
    fn checksum_key_distinguishes_source_paths_in_a_subdirectory() {
        let dir = tmp();
        let tmp_dir = tmp();
        let mut absolute = included_task(&dir, "build", "", &join(&dir, "Taskfile.yml"), "echo hi");
        absolute.dirs.push("sub".to_string());
        absolute.sources = vec![g(&join(&dir, "src.txt"))];
        let mut relative = absolute.clone();
        relative.sources = vec![g("src.txt")];
        assert_ne!(key_of(&tmp_dir, absolute), key_of(&tmp_dir, relative));
    }
}
