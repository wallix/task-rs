//! The execution engine: the orchestrator that reads a Taskfile, compiles
//! tasks, resolves their dependency graph, and runs them with fingerprint-based
//! up-to-date detection, remote caching, and optional file watching.
//!
//! [`Executor`] mirrors the Go `Executor` struct. It is configured through the
//! `with_*` builder setters, initialized with [`Executor::setup`], then driven
//! with [`Executor::run`] (or the lower-level [`Executor::run_task`]).
//!
//! ## Concurrency model
//!
//! The engine runs dependencies concurrently and deduplicates repeated calls to
//! the same task+vars, matching the Go executor's goroutine + `errgroup` +
//! per-hash `sync.Once` design. Because the output styles are single-threaded
//! (`Rc`-based, matching their reference counterparts), the engine runs on a
//! current-thread runtime and schedules concurrent work with
//! [`tokio::task::LocalSet`]/`spawn_local` rather than the multi-thread
//! scheduler. The CLI drives the async methods inside a `LocalSet`.
//!
//! Shared engine state lives behind [`std::rc::Rc`] with per-field interior
//! mutability ([`RefCell`]/[`tokio::sync::Mutex`]). The concurrency cap is a
//! [`ConcurrencyLimiter`] (a tokio semaphore); the run-once map holds a shared
//! [`OnceCell`] future result per task hash so concurrent callers await a single
//! execution.

mod cache_export;
mod error;
mod prompt;
mod prompter;
mod run;
mod setup;
mod signals;
mod status;
mod watch;

pub use error::ExecutorError;
pub use prompter::{PromptError, Prompter};
pub use run::MatchingTask;
pub use watch::should_ignore;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use tokio::sync::{Mutex, OnceCell};

use crate::ast::{self, Taskfile, Vars};
use crate::call::Call;
use crate::compiler::Compiler;
use crate::concurrency::ConcurrencyLimiter;
use crate::logger::Logger;
use crate::output::Output;
use crate::sort::Sorter;

/// The maximum number of times a task can be called, guarding against cyclic
/// dependencies. Ports Go `MaximumTaskCall`.
pub const MAXIMUM_TASK_CALL: i32 = 1000;

/// The default debounce window for watch events.
pub const DEFAULT_WATCH_INTERVAL_MS: u64 = 100;

/// The temporary directory locations used by the executor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TempDir {
    /// Where fingerprint checksum state is stored.
    pub fingerprint: String,
}

/// A task's listing metadata, returned by [`Executor::list_tasks`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskSummary {
    /// The display name (label, full name, or key).
    pub name: String,
    /// The raw task key (`task.task`), unaffected by `label`.
    pub task: String,
    /// The task description.
    pub desc: String,
    /// The task summary text.
    pub summary: String,
    /// The task's aliases.
    pub aliases: Vec<String>,
    /// Where the task is defined, for editor integrations.
    pub location: Option<ast::Location>,
}

/// The result of one task execution, shared between concurrent callers that
/// deduplicate on the same task hash.
type RunOnceResult = Result<(), Arc<ExecutorError>>;

/// Processes Taskfiles and executes the tasks within them.
///
/// Configure with the `with_*` setters, call [`Executor::setup`], then run.
pub struct Executor {
    // Flags
    /// The working directory the Taskfile is resolved against.
    pub dir: String,
    /// The entrypoint Taskfile path (empty to search for a default).
    pub entrypoint: String,
    /// The temporary directory locations.
    pub temp_dir: TempDir,
    /// Always run a task even when fingerprinting would skip it.
    pub force: bool,
    /// Always run all tasks, including subtasks.
    pub force_all: bool,
    /// Keep running and re-run tasks on source changes.
    pub watch: bool,
    /// Emit extra diagnostics.
    pub verbose: bool,
    /// Suppress command echoing.
    pub silent: bool,
    /// Disable fuzzy task-name suggestions.
    pub disable_fuzzy: bool,
    /// Assume "yes" for all confirmations.
    pub assume_yes: bool,
    /// Pretend a terminal is attached (testing).
    pub assume_term: bool,
    /// Prompt for missing required variables.
    pub interactive: bool,
    /// Print commands without running them.
    pub dry: bool,
    /// Print a task summary instead of running.
    pub summary: bool,
    /// Run the tasks given on one call in parallel.
    pub parallel: bool,
    /// Colorize output.
    pub color: bool,
    /// The maximum number of concurrent tasks (0 = unlimited).
    pub concurrency: usize,
    /// The watch debounce interval in milliseconds (0 = taskfile/default).
    pub interval_ms: u64,
    /// Cancel all in-flight work as soon as one task fails.
    pub failfast: bool,
    /// Verify the Taskfile schema version.
    pub enable_version_check: bool,

    /// The directory the user invoked `task` from.
    pub user_working_dir: String,

    // Internal, populated by `setup`.
    /// The merged Taskfile.
    pub(crate) taskfile: Option<Taskfile>,
    /// The output style, overriding the Taskfile's when set.
    pub(crate) output_style: ast::Output,
    /// The sorter applied when listing tasks.
    pub(crate) task_sorter: Sorter,
    /// The logger, shared for line-prefix coloring by the output style. A
    /// default is installed at construction and replaced during setup.
    pub(crate) logger: Rc<RefCell<Logger>>,
    /// The variable compiler, installed during setup.
    pub(crate) compiler: Rc<Compiler>,
    /// The active output style wrapper, installed during setup.
    pub(crate) output: Rc<dyn Output>,
    /// Whether task env/vars override the process environment. Defaults to true;
    /// `TASK_X_ENV_PRECEDENCE=0` restores the old process-environment-wins order.
    pub(crate) env_precedence: bool,

    /// Variables collected via interactive prompts, injected into calls.
    pub(crate) prompted_vars: RefCell<Option<Vars>>,
    /// The interactive prompter (a non-interactive session when `None`).
    pub(crate) prompter: Option<Box<dyn Prompter>>,

    /// Bounds concurrent task execution.
    pub(crate) limiter: ConcurrencyLimiter,
    /// Per-task call counters guarding against cyclic dependencies.
    pub(crate) task_call_count: RefCell<HashMap<String, i32>>,
    /// Serializes directory creation per task.
    pub(crate) mkdir_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Deduplicates concurrent executions of the same task hash.
    pub(crate) run_once: Mutex<HashMap<String, Arc<OnceCell<RunOnceResult>>>>,
}

impl Default for Executor {
    fn default() -> Self {
        Executor {
            dir: String::new(),
            entrypoint: String::new(),
            temp_dir: TempDir::default(),
            force: false,
            force_all: false,
            watch: false,
            verbose: false,
            silent: false,
            disable_fuzzy: false,
            assume_yes: false,
            assume_term: false,
            interactive: false,
            dry: false,
            summary: false,
            parallel: false,
            color: false,
            concurrency: 0,
            interval_ms: 0,
            failfast: false,
            enable_version_check: false,
            user_working_dir: String::new(),
            taskfile: None,
            output_style: ast::Output::default(),
            task_sorter: Sorter::AlphaNumericWithRootTasksFirst,
            logger: Rc::new(RefCell::new(Logger::default())),
            compiler: Rc::new(Compiler::new(
                String::new(),
                String::new(),
                String::new(),
                Vars::new(),
                Vars::new(),
                false,
            )),
            output: Rc::new(crate::output::Interleaved),
            env_precedence: false,
            prompted_vars: RefCell::new(None),
            prompter: None,
            limiter: ConcurrencyLimiter::unlimited(),
            task_call_count: RefCell::new(HashMap::new()),
            mkdir_locks: Mutex::new(HashMap::new()),
            run_once: Mutex::new(HashMap::new()),
        }
    }
}

impl Executor {
    /// Creates an executor with default options. Configure it with the `with_*`
    /// setters before calling [`Executor::setup`].
    pub fn new() -> Self {
        Executor::default()
    }

    /// Sets the working directory the Taskfile is resolved against.
    pub fn with_dir(mut self, dir: impl Into<String>) -> Self {
        self.dir = dir.into();
        self
    }

    /// Sets the entrypoint Taskfile path.
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self {
        self.entrypoint = entrypoint.into();
        self
    }

    /// Sets the temporary-directory locations.
    pub fn with_temp_dir(mut self, temp_dir: TempDir) -> Self {
        self.temp_dir = temp_dir;
        self
    }

    /// Forces a task to run even when fingerprinting would skip it.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Forces all tasks (including subtasks) to run.
    pub fn with_force_all(mut self, force_all: bool) -> Self {
        self.force_all = force_all;
        self
    }

    /// Enables watch mode.
    pub fn with_watch(mut self, watch: bool) -> Self {
        self.watch = watch;
        self
    }

    /// Enables verbose diagnostics.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Suppresses command echoing.
    pub fn with_silent(mut self, silent: bool) -> Self {
        self.silent = silent;
        self
    }

    /// Disables fuzzy task-name suggestions.
    pub fn with_disable_fuzzy(mut self, disable_fuzzy: bool) -> Self {
        self.disable_fuzzy = disable_fuzzy;
        self
    }

    /// Assumes "yes" for all confirmations.
    pub fn with_assume_yes(mut self, assume_yes: bool) -> Self {
        self.assume_yes = assume_yes;
        self
    }

    /// Pretends a terminal is attached (testing).
    pub fn with_assume_term(mut self, assume_term: bool) -> Self {
        self.assume_term = assume_term;
        self
    }

    /// Enables prompting for missing required variables.
    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Enables dry-run mode.
    pub fn with_dry(mut self, dry: bool) -> Self {
        self.dry = dry;
        self
    }

    /// Prints a task summary instead of running.
    pub fn with_summary(mut self, summary: bool) -> Self {
        self.summary = summary;
        self
    }

    /// Runs the tasks given on one call in parallel.
    pub fn with_parallel(mut self, parallel: bool) -> Self {
        self.parallel = parallel;
        self
    }

    /// Enables colorized output.
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Sets the maximum number of concurrent tasks (0 = unlimited).
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Sets the watch debounce interval in milliseconds.
    pub fn with_interval_ms(mut self, interval_ms: u64) -> Self {
        self.interval_ms = interval_ms;
        self
    }

    /// Cancels in-flight work as soon as one task fails.
    pub fn with_failfast(mut self, failfast: bool) -> Self {
        self.failfast = failfast;
        self
    }

    /// Enables Taskfile schema-version checking.
    pub fn with_version_check(mut self, enable: bool) -> Self {
        self.enable_version_check = enable;
        self
    }

    /// Overrides the output style (otherwise taken from the Taskfile).
    pub fn with_output_style(mut self, output_style: ast::Output) -> Self {
        self.output_style = output_style;
        self
    }

    /// Sets the sorter used when listing tasks.
    pub fn with_task_sorter(mut self, sorter: Sorter) -> Self {
        self.task_sorter = sorter;
        self
    }

    /// Sets the directory the user invoked `task` from (defaults to the current
    /// directory during setup).
    pub fn with_user_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.user_working_dir = dir.into();
        self
    }

    /// Sets the interactive prompter. Without one the engine runs
    /// non-interactively.
    pub fn with_prompter(mut self, prompter: Box<dyn Prompter>) -> Self {
        self.prompter = Some(prompter);
        self
    }

    /// The merged Taskfile, available after [`Executor::setup`].
    pub fn taskfile(&self) -> Option<&Taskfile> {
        self.taskfile.as_ref()
    }

    /// Returns the non-internal tasks for listing, sorted by the configured
    /// task sorter. When `all` is false, tasks without a description are
    /// excluded. Ports the filtering half of Go `GetTaskList`.
    pub fn list_tasks(&self, all: bool) -> Vec<TaskSummary> {
        let Some(tf) = self.taskfile.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for task in tf.tasks.values(self.task_sorter) {
            if task.internal {
                continue;
            }
            if !all && task.desc.is_empty() {
                continue;
            }
            out.push(TaskSummary {
                name: task.name().to_string(),
                task: task.task.clone(),
                desc: task.desc.clone(),
                summary: task.summary.clone(),
                aliases: task.aliases.clone(),
                location: task.location.clone(),
            });
        }
        out
    }

    /// Returns a shared handle to the logger.
    pub(crate) fn logger(&self) -> Rc<RefCell<Logger>> {
        Rc::clone(&self.logger)
    }

    /// Returns a shared handle to the variable compiler.
    pub(crate) fn compiler(&self) -> Rc<Compiler> {
        Rc::clone(&self.compiler)
    }

    /// Builds a throwaway [`Logger`] that captures output into shared byte
    /// buffers, carrying the executor's verbose/color flags. Async operations
    /// that need `&mut Logger` (the compiler, preconditions, cache) use this so
    /// no `RefCell` borrow of the shared logger is held across an `.await`;
    /// [`Executor::flush_scratch`] replays the captured bytes afterward.
    pub(crate) fn scratch_logger(&self) -> (Logger, ScratchSink) {
        let sink = ScratchSink::default();
        let logger = Logger {
            stdin: None,
            stdout: Box::new(sink.out.clone()),
            stderr: Box::new(sink.err.clone()),
            verbose: self.verbose,
            color: self.color,
            assume_yes: self.assume_yes,
            assume_term: self.assume_term,
        };
        (logger, sink)
    }

    /// Replays a scratch logger's captured output to the real logger's streams.
    pub(crate) fn flush_scratch(&self, sink: &ScratchSink) {
        use std::io::Write as _;
        let logger = self.logger();
        let mut logger = logger.borrow_mut();
        let out = sink.out.take();
        if !out.is_empty() {
            let _ = logger.stdout.write_all(&out);
        }
        let err = sink.err.take();
        if !err.is_empty() {
            let _ = logger.stderr.write_all(&err);
        }
    }

    /// Reports whether interactive prompting is possible: a prompter is set and
    /// a terminal is attached (or assumed). Ports Go `canPrompt`.
    pub(crate) fn can_prompt(&self) -> bool {
        self.interactive
            && self.prompter.is_some()
            && (self.assume_term || crate::term::is_terminal())
    }

    /// Injects any interactively-prompted variables into `call` (without
    /// overriding values already present). Ports the prompt-injection block at
    /// the top of Go `RunTask`.
    pub(crate) fn inject_prompted_vars(&self, call: &mut Call) {
        let prompted = self.prompted_vars.borrow();
        let Some(prompted) = prompted.as_ref() else {
            return;
        };
        for (name, v) in prompted.all() {
            if call.vars.get(name).is_none() {
                call.vars.set(name.clone(), v.clone());
            }
        }
    }
}

/// A thread-safe byte buffer backing a scratch logger stream.
#[derive(Clone, Default)]
pub(crate) struct ScratchBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl ScratchBuf {
    /// Removes and returns the buffered bytes.
    fn take(&self) -> Vec<u8> {
        match self.0.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(p) => std::mem::take(&mut *p.into_inner()),
        }
    }
}

impl std::io::Write for ScratchBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut g) = self.0.lock() {
            g.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The captured stdout/stderr of a scratch logger.
#[derive(Clone, Default)]
pub(crate) struct ScratchSink {
    out: ScratchBuf,
    err: ScratchBuf,
}

/// Reports whether a task's platform constraints allow it to run on the current
/// OS/arch. An empty list matches everything. Ports Go
/// `shouldRunOnCurrentPlatform`.
pub(crate) fn should_run_on_current_platform(platforms: &[ast::Platform]) -> bool {
    if platforms.is_empty() {
        return true;
    }
    let os = current_os();
    let arch = current_arch();
    platforms
        .iter()
        .any(|p| (p.os.is_empty() || p.os == os) && (p.arch.is_empty() || p.arch == arch))
}

/// The Go `runtime.GOOS` value for the target the binary was built for.
fn current_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// The Go `runtime.GOARCH` value for the target the binary was built for.
fn current_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_platforms_always_match() {
        assert!(should_run_on_current_platform(&[]));
    }

    #[test]
    fn matching_os_matches() {
        let p = ast::Platform {
            os: current_os().to_string(),
            arch: String::new(),
        };
        assert!(should_run_on_current_platform(&[p]));
    }

    #[test]
    fn non_matching_os_does_not_match() {
        let other_os = if current_os() == "linux" {
            "windows"
        } else {
            "linux"
        };
        let p = ast::Platform {
            os: other_os.to_string(),
            arch: String::new(),
        };
        assert!(!should_run_on_current_platform(&[p]));
    }
}
