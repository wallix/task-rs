//! The checksum-based up-to-date checker.

use std::path::Path;

use serde::Serialize;

use crate::ast::{Cmd, Glob, Task};
use crate::filepathext;

use super::checksum::checksum_files;
use super::glob::{cache_globs, globs};

/// Validates whether a task is up to date by comparing a checksum of its source
/// files against a stored value. It is bound to a single task and precomputes
/// source metadata at construction time.
pub struct ChecksumChecker {
    temp_dir: String,
    task: Task,
    sources_globs: Vec<Glob>,
    src_data: Vec<String>,
    /// From `task.source_hash` or lazily computed; empty when the task has no
    /// sources.
    source_hash: String,
    /// A snapshot of the sources checksum taken at `is_up_to_date` time (before
    /// execution). `sources_changed` compares it against a fresh disk
    /// computation to detect drift.
    pre_exec_disk_hash: String,
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

impl ChecksumChecker {
    /// Creates a checker bound to `task`. It precomputes the source globs and
    /// metadata but does not access disk for the source hash: it reuses
    /// `task.source_hash` when available (set during compilation). When that is
    /// empty, [`ChecksumChecker::source_value`] computes it lazily.
    pub fn new(temp_dir: impl Into<String>, task: Task) -> Self {
        let mut c = ChecksumChecker {
            temp_dir: temp_dir.into(),
            source_hash: task.source_hash.clone(),
            task,
            sources_globs: Vec::new(),
            src_data: Vec::new(),
            pre_exec_disk_hash: String::new(),
        };
        let (globs, data) = c.build_checksum_data();
        c.sources_globs = globs;
        c.src_data = data;
        c
    }

    fn build_checksum_data(&self) -> (Vec<Glob>, Vec<String>) {
        let dir = self.compute_dir();
        let mut sources = Vec::new();
        let mut data = Vec::new();
        for source in &self.task.sources {
            if source.glob.starts_with("value:") {
                data.push(source.glob.clone());
            } else {
                sources.push(source.clone());
                let mut s = rel_glob(&dir, &source.glob);
                if source.negate {
                    s = format!("!{s}");
                }
                data.push(format!("srcrule:{s}"));
            }
        }
        for (i, cmd) in self.task.raw_cmds.iter().enumerate() {
            data.push(serialize_cmd(i, cmd));
        }
        for gen_rule in &self.task.generates {
            let mut s = rel_glob(&dir, &gen_rule.glob);
            if gen_rule.negate {
                s = format!("!{s}");
            }
            data.push(format!("genrule:{s}"));
        }
        data.sort();
        (sources, data)
    }

    /// Reports whether the task is up to date: its sources and generates hashes
    /// match the stored values. A task with no sources is never up to date.
    pub fn is_up_to_date(&mut self) -> std::io::Result<bool> {
        if self.task.sources.is_empty() {
            return Ok(false);
        }

        let current_sources_hash = self.sources_checksum()?;
        self.pre_exec_disk_hash = current_sources_hash.clone();

        let checksum_file = self.checksum_file_path();
        let stored = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        let (old_sources_hash, old_generates_hash) = split_hashes(&stored);

        let new_generates_hash = self.generates_checksum()?;

        Ok(old_sources_hash == current_sources_hash && old_generates_hash == new_generates_hash)
    }

    /// Reports whether source files were modified during task execution by
    /// comparing the disk snapshot taken at [`ChecksumChecker::is_up_to_date`]
    /// time against the current state.
    pub fn sources_changed(&self) -> std::io::Result<bool> {
        if self.pre_exec_disk_hash.is_empty() {
            return Ok(false);
        }
        let current = self.sources_checksum()?;
        Ok(current != self.pre_exec_disk_hash)
    }

    /// Returns the full fingerprint state for the task.
    pub fn status(&self) -> std::io::Result<TaskStatus> {
        let checksum_file = self.checksum_file_path();
        let stored = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        let (old_sources_hash, old_generates_hash) = split_hashes(&stored);

        let current_sources_hash = self.sources_checksum()?;
        let new_generates_hash = self.generates_checksum()?;

        let dir = self.compute_dir();
        let sources_files = globs(&dir, &self.sources_globs).unwrap_or_default();
        let generates = globs(&dir, &self.task.generates).unwrap_or_default();
        let cache_files = cache_globs(&dir, &self.task.generates).unwrap_or_default();

        let src_ok = old_sources_hash == current_sources_hash;
        let gen_ok = old_generates_hash == new_generates_hash;

        Ok(TaskStatus {
            task: self.task.name().to_string(),
            up_to_date: src_ok && gen_ok,
            sources_up_to_date: src_ok,
            generates_up_to_date: gen_ok,
            checksum_file,
            sources_hash: old_sources_hash.to_string(),
            source_files: sources_files,
            source_data: self.src_data.clone(),
            generates_hash: old_generates_hash.to_string(),
            generate_files: generates,
            cache_files,
        })
    }

    /// Returns the sources checksum for use as the `CHECKSUM` template variable,
    /// lock keys, and cache keys, computing it lazily from disk on first call
    /// when it was not precomputed during compilation.
    pub fn source_value(&mut self) -> &str {
        if self.source_hash.is_empty() && !self.task.sources.is_empty() {
            self.source_hash = self.sources_checksum().unwrap_or_default();
        }
        &self.source_hash
    }

    /// Records the current sources and generates hashes as up to date.
    pub fn set_up_to_date(&self) -> std::io::Result<()> {
        if self.task.sources.is_empty() {
            return Ok(());
        }

        let new_sources_hash = self.sources_checksum()?;
        let new_generates_hash = self.generates_checksum()?;

        let checksum_dir = Path::new(&self.temp_dir).join("checksum");
        let _ = std::fs::create_dir_all(&checksum_dir);
        std::fs::write(
            self.checksum_file_path(),
            format!("{new_sources_hash}\n{new_generates_hash}\n"),
        )
    }

    /// Removes the stored checksum after a failed run so the next invocation
    /// re-runs the task.
    pub fn on_error(&self) -> std::io::Result<()> {
        if self.task.sources.is_empty() {
            return Ok(());
        }
        match std::fs::remove_file(self.checksum_file_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Returns the checker kind identifier.
    pub fn kind(&self) -> &'static str {
        "checksum"
    }

    fn sources_checksum(&self) -> std::io::Result<String> {
        self.checksum(&self.sources_globs, &self.src_data)
    }

    /// Computes the current generates hash from disk.
    pub fn generates_checksum(&self) -> std::io::Result<String> {
        self.checksum(&self.task.generates, &[])
    }

    fn checksum(&self, patterns: &[Glob], data: &[String]) -> std::io::Result<String> {
        let dir = self.compute_dir();
        let sources = globs(&dir, patterns)?;
        checksum_files(&dir, &sources, data)
    }

    fn checksum_file_path(&self) -> String {
        Path::new(&self.temp_dir)
            .join("checksum")
            .join(normalize_filename(self.task.name()))
            .to_string_lossy()
            .into_owned()
    }

    fn compute_dir(&self) -> String {
        self.task.compute_dir().to_string_lossy().into_owned()
    }
}

fn serialize_cmd(idx: usize, c: &Cmd) -> String {
    format!("cmd[{idx}]:{}", c.cmd)
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
    use crate::fingerprint::testutil::{join, tmp, write_file};

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
        let st = checker.status().unwrap();

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
        let st = checker.status().unwrap();

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
        assert!(checker2.is_up_to_date().unwrap());

        // Non-marker change: still up to date.
        write_file(&dir, "node_modules/pkg/index.js", "v2-changed");
        let mut checker3 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(checker3.is_up_to_date().unwrap());

        // Marker change: out of date.
        write_file(&dir, "node_modules/.yarn-state.yml", "state-v2");
        let mut checker4 = ChecksumChecker::new(&tmp_dir, task);
        assert!(!checker4.is_up_to_date().unwrap());
    }

    #[test]
    fn up_to_date_transitions_and_source_change_detection() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "one");

        let task = new_task(&dir, vec![g("src.txt")], vec![]);

        // No stored checksum yet.
        let mut checker = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(!checker.is_up_to_date().unwrap());

        // Record and re-check.
        checker.set_up_to_date().unwrap();
        let mut checker2 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(checker2.is_up_to_date().unwrap());
        // No drift after the snapshot was taken.
        assert!(!checker2.sources_changed().unwrap());

        // Modify the source: out of date, and drift detected relative to the
        // snapshot taken above.
        write_file(&dir, "src.txt", "two");
        assert!(checker2.sources_changed().unwrap());
        let mut checker3 = ChecksumChecker::new(&tmp_dir, task.clone());
        assert!(!checker3.is_up_to_date().unwrap());

        // on_error clears the stored checksum.
        checker.set_up_to_date().unwrap();
        checker.on_error().unwrap();
        let mut checker4 = ChecksumChecker::new(&tmp_dir, task);
        assert!(!checker4.is_up_to_date().unwrap());
    }

    #[test]
    fn no_sources_is_never_up_to_date() {
        let dir = tmp();
        let tmp_dir = tmp();
        let task = new_task(&dir, vec![], vec![]);
        let mut checker = ChecksumChecker::new(tmp_dir, task);
        assert!(!checker.is_up_to_date().unwrap());
        // set_up_to_date and on_error are no-ops with no sources.
        assert!(checker.set_up_to_date().is_ok());
        assert!(checker.on_error().is_ok());
    }

    #[test]
    fn source_value_computed_lazily() {
        let dir = tmp();
        let tmp_dir = tmp();
        write_file(&dir, "src.txt", "content");
        let task = new_task(&dir, vec![g("src.txt")], vec![]);
        let mut checker = ChecksumChecker::new(tmp_dir, task);
        assert!(!checker.source_value().is_empty());
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
}
