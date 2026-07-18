//! Whole-project cache export/import: collect the fingerprint state (checksum
//! files and generated files) for a set of tasks and their dependencies into a
//! single ZIP, or restore it. Ports the `ExportCache`/`ImportCache`/
//! `collectCacheFiles` half of Go `cache.go`.

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

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
    pub async fn import_cache(
        self: &Rc<Self>,
        zip_path: &Path,
        calls: &[Call],
    ) -> Result<(), ExecutorError> {
        self.logger().borrow_mut().outf(
            Color::Magenta,
            &format!("task: importing cache from {:?}\n", zip_path.display()),
        );
        archive::extract_archive(zip_path, Path::new(&self.dir))?;
        self.run_setup_for_calls(calls).await
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
