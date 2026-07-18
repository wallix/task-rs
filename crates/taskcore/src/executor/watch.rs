//! Watch mode: re-run tasks when their source files change.
//!
//! Ports Go `watch.go`. Filesystem events come from the `notify` crate; events
//! are debounced over a short window (the `--interval`, the Taskfile
//! `interval:`, or [`DEFAULT_WATCH_INTERVAL_MS`]). On a relevant change the run
//! context is cancelled and the tasks are re-run. Directories holding source
//! files are (re)registered periodically so that files created in
//! previously-empty directories are picked up.

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::ast::Task;
use crate::call::Call;
use crate::fingerprint;
use crate::logger::Color;

use super::{DEFAULT_WATCH_INTERVAL_MS, Executor, ExecutorError};

/// Directory path fragments that are never watched.
const IGNORE_PATHS: &[&str] = &["/.task", "/.git", "/.hg", "/node_modules"];

/// Reports whether a path lies within an ignored directory. Ports Go
/// `ShouldIgnore`.
pub fn should_ignore(path: &str) -> bool {
    IGNORE_PATHS
        .iter()
        .any(|p| path.contains(&format!("{p}/")) || path.ends_with(p))
}

impl Executor {
    /// Watches the given tasks, re-running them when their sources change until
    /// the process is interrupted. Ports Go `watchTasks`.
    pub(crate) async fn watch_tasks(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
        let names: Vec<&str> = calls.iter().map(|c| c.task.as_str()).collect();
        self.logger().borrow_mut().errf(
            Color::Green,
            &format!("task: Started watching for tasks: {}\n", names.join(", ")),
        );

        // Initial run of every task.
        for call in calls {
            self.run_watch_call(call).await;
        }

        let wait = self.watch_interval();

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )
        .map_err(|e| ExecutorError::Io(e.to_string()))?;

        let mut watched: HashSet<String> = HashSet::new();
        self.register_watched_dirs(&mut watcher, calls, &mut watched)
            .await?;

        // Block for the first event, then coalesce any that arrive within the
        // debounce window, and re-run when a relevant source changed.
        while let Ok(first) = rx.recv() {
            let mut events = vec![first];
            let deadline = std::time::Instant::now()
                .checked_add(wait)
                .unwrap_or_else(std::time::Instant::now);
            while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
                match rx.recv_timeout(remaining) {
                    Ok(ev) => events.push(ev),
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            let relevant = self.events_touch_sources(&events, calls).await;
            if !relevant {
                continue;
            }

            self.compiler().reset_cache();
            for call in calls {
                self.run_watch_call(call).await;
            }

            // Pick up directories created since the last scan.
            self.register_watched_dirs(&mut watcher, calls, &mut watched)
                .await?;
        }

        Ok(())
    }

    /// The debounce window: the explicit interval, then the Taskfile interval,
    /// then the default.
    fn watch_interval(&self) -> Duration {
        if self.interval_ms != 0 {
            return Duration::from_millis(self.interval_ms);
        }
        if let Some(tf) = &self.taskfile
            && !tf.interval.is_empty()
            && let Ok(d) = crate::goext::parse_duration(&tf.interval)
        {
            return d;
        }
        Duration::from_millis(DEFAULT_WATCH_INTERVAL_MS)
    }

    /// Runs one watched task, logging completion or a non-cancellation error.
    async fn run_watch_call(self: &Rc<Self>, call: &Call) {
        match self.run_task(call.clone()).await {
            Ok(()) => {
                self.logger().borrow_mut().errf(
                    Color::Green,
                    &format!("task: task \"{}\" finished running\n", call.task),
                );
            }
            Err(e) if !e.is_context_error() => {
                self.logger()
                    .borrow_mut()
                    .errf(Color::Red, &format!("{e}\n"));
            }
            Err(_) => {}
        }
    }

    /// Reports whether any event touches a file among the tasks' sources (a
    /// removal always counts). Ignored directories are skipped.
    async fn events_touch_sources(&self, events: &[notify::Result<Event>], calls: &[Call]) -> bool {
        let sources = match self.collect_sources(calls).await {
            Ok(s) => s,
            Err(e) => {
                self.logger()
                    .borrow_mut()
                    .errf(Color::Red, &format!("{e}\n"));
                return false;
            }
        };
        let source_set: HashSet<&String> = sources.iter().collect();

        for res in events {
            let Ok(event) = res else { continue };
            let is_remove = matches!(event.kind, EventKind::Remove(_));
            for path in &event.paths {
                let p = path.to_string_lossy().into_owned();
                if should_ignore(&p) {
                    continue;
                }
                if is_remove || source_set.contains(&p) {
                    return true;
                }
            }
        }
        false
    }

    /// Registers every directory that holds a source file, skipping ignored and
    /// already-watched directories. Ports Go `registerWatchedDirs`.
    async fn register_watched_dirs(
        &self,
        watcher: &mut RecommendedWatcher,
        calls: &[Call],
        watched: &mut HashSet<String>,
    ) -> Result<(), ExecutorError> {
        let files = self.collect_sources(calls).await?;
        for f in files {
            let dir = Path::new(&f)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if dir.is_empty() || watched.contains(&dir) || should_ignore(&dir) {
                continue;
            }
            if watcher
                .watch(Path::new(&dir), RecursiveMode::NonRecursive)
                .is_ok()
            {
                watched.insert(dir);
            }
        }
        Ok(())
    }

    /// Collects the unique set of source files across the tasks and their
    /// dependency/command subtasks. Ports Go `collectSources`.
    async fn collect_sources(&self, calls: &[Call]) -> Result<Vec<String>, ExecutorError> {
        let mut sources = Vec::new();
        let mut visited = HashSet::new();
        for call in calls {
            self.traverse(call, &mut sources, &mut visited).await?;
        }
        sources.sort();
        sources.dedup();
        Ok(sources)
    }

    /// Recursively compiles a call and its dep/cmd subtasks, collecting each
    /// task's expanded source globs. Ports Go `traverse`.
    async fn traverse(
        &self,
        call: &Call,
        sources: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) -> Result<(), ExecutorError> {
        // Iterative worklist to avoid boxing a recursive async fn.
        let mut stack = vec![call.clone()];
        while let Some(current) = stack.pop() {
            let task = self.compiled_task(&current).await?;
            let key = task.name().to_string();
            if !visited.insert(key) {
                continue;
            }
            self.collect_task_sources(&task, sources)?;
            for dep in &task.deps {
                if !dep.task.is_empty() {
                    stack.push(Call {
                        task: dep.task.clone(),
                        vars: dep.vars.clone().unwrap_or_default(),
                        ..Default::default()
                    });
                }
            }
            for cmd in &task.cmds {
                if !cmd.task.is_empty() {
                    stack.push(Call {
                        task: cmd.task.clone(),
                        vars: cmd.vars.clone().unwrap_or_default(),
                        ..Default::default()
                    });
                }
            }
        }
        Ok(())
    }

    /// Expands a task's source globs into concrete file paths.
    fn collect_task_sources(
        &self,
        task: &Task,
        sources: &mut Vec<String>,
    ) -> Result<(), ExecutorError> {
        let dir = task.compute_dir();
        let dir = dir.to_string_lossy();
        let files = fingerprint::globs(&dir, &task.sources)?;
        sources.extend(files);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::should_ignore;

    #[test]
    fn ignores_dot_dirs() {
        // Ports Go `TestShouldIgnore`.
        assert!(should_ignore("/.git/hooks"));
        assert!(!should_ignore("/.github/workflows/build.yaml"));
    }

    #[test]
    fn ignores_task_and_node_modules() {
        assert!(should_ignore("/project/.task/checksum/x"));
        assert!(should_ignore("/project/node_modules/pkg/index.js"));
        assert!(should_ignore("/project/.hg"));
        assert!(!should_ignore("/project/src/main.rs"));
    }
}
