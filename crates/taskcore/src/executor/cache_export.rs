//! Whole-project cache export/import: collect the fingerprint state (checksum
//! files and generated files) for a set of tasks and their dependencies into a
//! single ZIP, or restore it. Ports the `ExportCache`/`ImportCache`/
//! `collectCacheFiles` half of Go `cache.go`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use cap_primitives::fs::{open_ambient_dir, open_dir_nofollow, remove_dir_all};

use crate::cache::archive::{self, CacheMeta};
use crate::call::Call;
use crate::fingerprint::ChecksumChecker;
use crate::logger::Color;

use super::{Executor, ExecutorError};

impl Executor {
    /// Exports the up-to-date fingerprint state (checksum + generated files) for
    /// the given tasks and their dependencies to `zip_path`. Setup tasks run
    /// first so their outputs exist. Skips writing when an identical archive
    /// already exists. Ports Go `ExportCache`.
    ///
    /// # Panics
    ///
    /// Must be awaited inside a [`tokio::task::LocalSet`]: dependencies, setup
    /// tasks and nested `task:` commands are queued with `spawn_local`, which
    /// panics outside one.
    pub async fn export_cache(
        self: &Rc<Self>,
        zip_path: &Path,
        calls: &[Call],
    ) -> Result<(), ExecutorError> {
        self.run_setup_for_calls(calls).await?;

        let mut export_files: BTreeMap<String, String> = BTreeMap::new();
        self.collect_cache_files(&mut export_files, calls).await?;

        if export_files.is_empty() {
            self.logger()
                .borrow_mut()
                .errf(Color::Yellow, "task: no up-to-date tasks to export\n");
            return Ok(());
        }

        let files: Vec<String> = export_files.keys().cloned().collect();
        let base = Path::new(&self.dir);

        if archive::archive_matches(base, zip_path, &files) {
            self.logger().borrow_mut().outf(
                Color::Magenta,
                &format!("task: cache {:?} is unmodified\n", zip_path.display()),
            );
            return Ok(());
        }

        self.logger().borrow_mut().outf(
            Color::Magenta,
            &format!("task: exporting cache to {:?}\n", zip_path.display()),
        );

        // A whole-project export carries no per-task metadata comment.
        archive::write_archive(zip_path, base, &files, &CacheMeta::default())?;
        Ok(())
    }

    /// Restores files from an archive created by [`Executor::export_cache`],
    /// then runs setup tasks so preparation steps are applied. Ports Go
    /// `ImportCache`.
    ///
    /// Extraction stops at the first failing entry. Discard the project's
    /// checksums on failure, since some may already describe a partial tree.
    /// Report cleanup failures alongside the original extraction error.
    ///
    /// # Panics
    ///
    /// Must be awaited inside a [`tokio::task::LocalSet`]: dependencies, setup
    /// tasks and nested `task:` commands are queued with `spawn_local`, which
    /// panics outside one.
    pub async fn import_cache(
        self: &Rc<Self>,
        zip_path: &Path,
        calls: &[Call],
    ) -> Result<(), ExecutorError> {
        // Hold the trusted parent before extraction can replace path entries.
        let cleanup =
            ChecksumCleanup::prepare(Path::new(&self.dir), Path::new(&self.temp_dir.fingerprint));
        self.logger().borrow_mut().outf(
            Color::Magenta,
            &format!("task: importing cache from {:?}\n", zip_path.display()),
        );
        if let Err(e) = archive::extract_archive(zip_path, Path::new(&self.dir)) {
            self.discard_checksums(cleanup);
            return Err(e.into());
        }
        self.run_setup_for_calls(calls).await
    }

    /// An archive can restore tasks absent from `calls`. Clear all project
    /// checksums without compiling tasks against partially restored outputs.
    fn discard_checksums(&self, cleanup: io::Result<ChecksumCleanup>) {
        let dir = Path::new(&self.temp_dir.fingerprint).join("checksum");
        if let Err(e) = cleanup.and_then(ChecksumCleanup::discard)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            // Preserve the extraction error, but expose retained stale state.
            self.logger().borrow_mut().errf(
                Color::Yellow,
                &format!(
                    "task: could not discard checksums at {}: {e}; tasks may still appear up to date\n",
                    dir.display()
                ),
            );
        }
    }

    /// Runs the setup tasks of each call, so their outputs exist before a cache
    /// export and preparation steps are applied after an import. Ports Go
    /// `runSetupForCalls`.
    async fn run_setup_for_calls(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
        for call in calls {
            let task = self.compiled_task(call).await?;
            for dep in &task.setup {
                let setup_call = Call {
                    task: dep.task.clone(),
                    vars: dep.vars.clone().unwrap_or_default(),
                    silent: dep.silent,
                    indirect: true,
                };
                self.run_task(setup_call).await?;
            }
        }
        Ok(())
    }

    /// Collects the checksum file and generated files for each up-to-date task,
    /// keyed by path (value = owning task name for duplicate diagnostics). Ports
    /// Go `collectCacheFiles`.
    async fn collect_cache_files(
        &self,
        files: &mut BTreeMap<String, String>,
        calls: &[Call],
    ) -> Result<(), ExecutorError> {
        for call in calls {
            let task = self.compiled_task(call).await?;
            if task.sources.is_empty() && task.generates.is_empty() {
                continue;
            }
            let checker = ChecksumChecker::new(&self.temp_dir.fingerprint, task.clone());
            let st = checker.status()?;
            if !st.up_to_date {
                self.logger().borrow_mut().errf(
                    Color::Yellow,
                    &format!(
                        "task: {:?} not up to date, skipped from export\n",
                        task.name()
                    ),
                );
                continue;
            }
            if !st.checksum_file.is_empty() {
                if let Some(existing) = files.get(&st.checksum_file) {
                    self.logger().borrow_mut().errf(
                        Color::Yellow,
                        &format!(
                            "task: checksum {:?} used by both {:?} and {:?}\n",
                            st.checksum_file,
                            existing,
                            task.name()
                        ),
                    );
                } else {
                    files.insert(st.checksum_file.clone(), task.name().to_string());
                }
            }
            for f in &st.cache_files {
                files.insert(f.clone(), task.name().to_string());
            }
        }
        Ok(())
    }
}

/// A trusted directory held across extraction, plus the path to the project's
/// fingerprint directory. Resolve each remaining component without symlinks.
struct ChecksumCleanup {
    parent: File,
    relative: PathBuf,
}

impl ChecksumCleanup {
    fn prepare(project: &Path, fingerprint: &Path) -> io::Result<Self> {
        if let Ok(relative) = fingerprint.strip_prefix(project) {
            return Ok(Self {
                parent: open_ambient_dir(project, cap_primitives::ambient_authority())?,
                relative: relative.to_path_buf(),
            });
        }

        // An external TASK_TEMP_DIR is user-selected. Anchor its closest
        // existing parent before the archive can create any missing children.
        for ancestor in fingerprint.ancestors().skip(1) {
            match open_ambient_dir(ancestor, cap_primitives::ambient_authority()) {
                Ok(parent) => {
                    let relative = fingerprint
                        .strip_prefix(ancestor)
                        .map_err(io::Error::other)?;
                    return Ok(Self {
                        parent,
                        relative: relative.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no parent for fingerprint directory"))
    }

    fn discard(self) -> io::Result<()> {
        let mut parent = self.parent;
        for component in self.relative.components() {
            match component {
                Component::CurDir => continue,
                Component::Normal(name) => {
                    parent = open_dir_nofollow(&parent, Path::new(name))?;
                }
                _ => return Err(io::Error::other("invalid fingerprint directory path")),
            }
        }
        remove_dir_all(&parent, Path::new("checksum"))
    }
}
