//! `--status`: report each task's fingerprint up-to-date state without running
//! it. Informational — it prints the status and succeeds; a real compile or I/O
//! failure still errors. Ports Go `fingerprint.go` (`Status`/`StatusJSON`).

use std::rc::Rc;

use crate::ast::NAMESPACE_SEPARATOR;
use crate::call::Call;
use crate::editors::{EditorLocation, EditorTask, Namespace};
use crate::fingerprint::{ChecksumChecker, TaskStatus};
use crate::logger::Color;

use super::{Executor, ExecutorError};

impl Executor {
    /// Prints the fingerprint status of each task. With `as_json`, emits the
    /// statuses as a pretty-printed JSON array instead.
    pub async fn status(
        self: &Rc<Self>,
        calls: &[Call],
        as_json: bool,
    ) -> Result<(), ExecutorError> {
        let mut statuses: Vec<TaskStatus> = Vec::new();
        for call in calls {
            let task = self.compiled_task(call).await?;
            if task.sources.is_empty() && task.generates.is_empty() {
                if as_json {
                    statuses.push(TaskStatus {
                        task: task.name().to_string(),
                        ..Default::default()
                    });
                } else {
                    self.logger().borrow_mut().outf(
                        Color::Yellow,
                        &format!("task: {:?} has no sources or generates\n", task.name()),
                    );
                }
                continue;
            }
            let checker = ChecksumChecker::new(&self.temp_dir.fingerprint, task.clone());
            statuses.push(checker.status()?);
        }

        if as_json {
            let json = serde_json::to_string_pretty(&statuses)
                .map_err(|e| ExecutorError::Io(format!("encoding status JSON: {e}")))?;
            let logger = self.logger();
            let mut logger = logger.borrow_mut();
            logger.outf(Color::None, &json);
            logger.outf(Color::None, "\n");
            return Ok(());
        }

        for (i, st) in statuses.iter().enumerate() {
            if i > 0 {
                self.logger().borrow_mut().outf(Color::None, "\n");
            }
            self.print_status(st);
        }
        Ok(())
    }

    /// Builds the editor-integration JSON tree (`--list-all --json`). Mirrors Go
    /// `ToEditorOutput`: each listed task is turned into an editor task with its
    /// fingerprint state, then placed flat or nested by its `:`-split name.
    pub async fn editor_output(&self, all: bool, nested: bool) -> Namespace {
        let root_location = self
            .taskfile
            .as_ref()
            .map(|t| t.location.clone())
            .unwrap_or_default();
        let mut root = Namespace {
            location: root_location,
            ..Default::default()
        };
        for summary in self.list_tasks(all) {
            let editor_task = EditorTask {
                up_to_date: self.task_up_to_date(&summary.task).await,
                location: summary.location.map(|l| EditorLocation {
                    line: l.line,
                    column: l.column,
                    taskfile: l.taskfile,
                }),
                name: summary.name,
                task: summary.task.clone(),
                desc: summary.desc,
                summary: summary.summary,
                aliases: summary.aliases,
            };
            if nested {
                let path: Vec<&str> = summary.task.split(NAMESPACE_SEPARATOR).collect();
                root.add_namespaced(&path, editor_task);
            } else {
                root.tasks.push(editor_task);
            }
        }
        root
    }

    /// Fingerprint check for the editor output: compiles the task and returns its
    /// up-to-date state, mirroring Go's `IsUpToDate` (a task with no recorded
    /// checksum is reported not up to date). `None` only when the task cannot be
    /// compiled or fingerprinted at all.
    async fn task_up_to_date(&self, name: &str) -> Option<bool> {
        let task = self.compiled_task(&Call::new(name)).await.ok()?;
        let mut checker = ChecksumChecker::new(&self.temp_dir.fingerprint, task);
        checker.is_up_to_date().ok()
    }

    fn print_status(&self, st: &TaskStatus) {
        if st.checksum_file.is_empty() {
            // A task with no sources/generates already got its message above.
            return;
        }
        let logger = self.logger();
        let mut l = logger.borrow_mut();

        if st.up_to_date {
            l.outf(
                Color::Green,
                &format!("task: {:?} is up to date\n", st.task),
            );
        } else {
            l.outf(
                Color::Red,
                &format!("task: {:?} is not up to date\n", st.task),
            );
        }
        l.outf(
            Color::None,
            &format!("  checksum file: {}\n", st.checksum_file),
        );

        // `genrule:` entries relate to generates and `cmd[` entries to commands,
        // though both live in the source hash; group them for a clearer display.
        let mut src_entries = Vec::new();
        let mut genrule_entries = Vec::new();
        let mut cmd_entries = Vec::new();
        for d in &st.source_data {
            if d.starts_with("genrule:") {
                genrule_entries.push(d);
            } else if d.starts_with("cmd[") {
                cmd_entries.push(d);
            } else {
                src_entries.push(d);
            }
        }

        if st.sources_up_to_date {
            l.outf(
                Color::None,
                &format!("  sources: up to date (hash: {})\n", st.sources_hash),
            );
        } else {
            l.outf(
                Color::None,
                &format!("  sources: changed (stored hash: {})\n", st.sources_hash),
            );
        }
        for d in &src_entries {
            l.outf(Color::None, &format!("    {d}\n"));
        }
        for f in &st.source_files {
            l.outf(Color::None, &format!("    file: {f}\n"));
        }
        for d in &cmd_entries {
            l.outf(Color::None, &format!("    {d}\n"));
        }

        if st.generates_up_to_date {
            l.outf(
                Color::None,
                &format!("  generates: up to date (hash: {})\n", st.generates_hash),
            );
        } else {
            l.outf(
                Color::None,
                &format!(
                    "  generates: changed (stored hash: {})\n",
                    st.generates_hash
                ),
            );
        }
        for d in &genrule_entries {
            l.outf(Color::None, &format!("    {d}\n"));
        }
        for f in &st.generate_files {
            l.outf(Color::None, &format!("    file: {f}\n"));
        }
    }
}
