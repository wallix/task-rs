//! Task resolution and the run loop: match calls to tasks, compile them,
//! short-circuit on fingerprint/cache, run dependencies concurrently, and
//! execute commands. Ports Go `task.go`.

use std::rc::Rc;
use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::ast::{Task, Var};
use crate::cache::{self, CacheLock, CacheUrl};
use crate::call::Call;
use crate::env;
use crate::execext::{self, RunCommandOptions, Stdio};
use crate::filepathext;
use crate::fingerprint::{self, ChecksumChecker};
use crate::logger::Color;
use crate::output::{Output, SharedWriter};
use crate::precondition;
use crate::requires;
use crate::sandbox::{
    Backend, Condition, Denial, Layers, Policy, PolicyInput, Sandbox, remove_stale_tmp,
};
use crate::slicesext;
use crate::templater::Cache as TemplaterCache;
use crate::variables::{self, CompileContext, CompileError};
use crate::yaml::Value;

use super::{
    Denied, Executor, ExecutorError, MAXIMUM_TASK_CALL, RunOnceResult,
    should_run_on_current_platform,
};

/// A task matched by a call, with any wildcard captures.
pub struct MatchingTask<'a> {
    /// The matched task.
    pub task: &'a Task,
    /// Captured wildcard substrings.
    pub wildcards: Vec<String>,
}

/// The tasks running above a call on its dependency path, innermost first —
/// which is why [`CallPath::to_path_through`] reverses before reporting. `None`
/// is an invocation from the command line, which has none.
pub(crate) type Ancestors = Option<Rc<CallPath>>;

/// One task on a dependency path, linked to the task that depends on it. Levels
/// share their parents, so extending a path is a single allocation and a lookup
/// walks at most its length.
///
/// A level carries two strings because the thing that identifies a repeat is
/// not the thing worth reporting: `key` is the resolved name plus a digest of
/// the compiled task, `name` is what the cycle is printed as.
pub(crate) struct CallPath {
    key: String,
    name: String,
    parent: Ancestors,
    /// The sandbox policies the task at this level runs within, outermost
    /// first: those of the sandboxed tasks whose commands called it.
    layers: Option<Arc<Layers>>,
    /// Whether the caller ignores this task's failures, which then need no
    /// sandbox hint.
    ignored: bool,
    /// Which failures of the tasks this level calls are ignored.
    calls: Ignored,
}

/// Which failures of a called task its caller ignores, for the sandbox hint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ignored {
    /// None.
    #[default]
    No,
    /// Its own commands' (`ignore_error`): a failure of a task it calls
    /// still fails the caller.
    Callee,
    /// Every one beneath it (a deferred `task:` call).
    All,
}

impl CallPath {
    /// The path with a level appended, for handing to that task's own callees.
    /// The new level runs within the same sandbox layers as this one.
    fn extend(path: &Ancestors, key: &str, name: &str) -> Rc<Self> {
        let calls = path.as_ref().map(|level| level.calls).unwrap_or_default();
        Rc::new(Self {
            key: key.to_string(),
            name: name.to_string(),
            parent: path.clone(),
            layers: Self::layers(path),
            ignored: calls != Ignored::No,
            calls: if calls == Ignored::All {
                Ignored::All
            } else {
                Ignored::No
            },
        })
    }

    /// This level calling a task whose failures `how` are ignored.
    fn calling(path: &Ancestors, how: Ignored) -> Ancestors {
        path.as_ref().map(|level| {
            Rc::new(Self {
                key: level.key.clone(),
                name: level.name.clone(),
                parent: level.parent.clone(),
                layers: level.layers.clone(),
                ignored: level.ignored,
                calls: level.calls.max(how),
            })
        })
    }

    /// Whether the caller of the innermost level of `path` ignores its
    /// failures.
    fn ignored(path: &Ancestors) -> bool {
        path.as_ref().is_some_and(|level| level.ignored)
    }

    /// This level with `layers` in place of its own, for what a sandboxed
    /// task's commands call.
    fn with_layers(path: &Ancestors, layers: Arc<Layers>) -> Ancestors {
        path.as_ref().map(|level| {
            Rc::new(Self {
                key: level.key.clone(),
                name: level.name.clone(),
                parent: level.parent.clone(),
                layers: Some(layers),
                ignored: level.ignored,
                calls: level.calls,
            })
        })
    }

    /// The sandbox layers at the innermost level of `path`.
    fn layers(path: &Ancestors) -> Option<Arc<Layers>> {
        path.as_ref().and_then(|level| level.layers.clone())
    }

    /// Reports whether a task with this key is already running on this path.
    fn contains(&self, key: &str) -> bool {
        let mut node = Some(self);
        while let Some(cur) = node {
            if cur.key == key {
                return true;
            }
            node = cur.parent.as_deref();
        }
        false
    }

    /// This path with `task` appended, outermost task first — the cycle to
    /// report, ending with the repeat that closes it.
    fn to_path_through(&self, task: &str) -> Vec<String> {
        let mut names = vec![task.to_string()];
        let mut node = Some(self);
        while let Some(cur) = node {
            names.push(cur.name.clone());
            node = cur.parent.as_deref();
        }
        names.reverse();
        names
    }
}

/// Bookkeeping for the tasks this executor has queued on the runtime.
///
/// The runtime owns the queued futures, so abandoning one — a sibling dropped
/// after a failfast error, or a whole subtree whose parent was abandoned — only
/// *schedules* its cancellation. Counting live tasks lets
/// [`Executor::drain_queue`] wait for those cancellations to actually run,
/// while the runtime is still turning, so an abandoned task releases its lock
/// and concurrency permit and starts no further command before the run
/// returns. A command already handed to the shell is not killed: cancellation
/// has never done that.
#[derive(Default)]
pub(crate) struct TaskQueue {
    /// Tasks queued and not yet finished or cancelled.
    live: std::cell::Cell<usize>,
    /// Aborts for every task queued during this run.
    aborts: std::cell::RefCell<Vec<tokio::task::AbortHandle>>,
    /// Notified whenever a task finishes, so a drain can re-check `live`.
    idle: tokio::sync::Notify,
    /// Whether any task was abandoned part-way. Its command keeps running —
    /// nothing in the shell interpreter's API can stop it from here — so the
    /// drain sweeps for leftovers only when this is set.
    abandoned: std::cell::Cell<bool>,
}

/// Decrements the live count when a queued task ends, however it ends.
struct LiveTask {
    executor: Rc<Executor>,
}

impl Drop for LiveTask {
    fn drop(&mut self) {
        let live = &self.executor.queue.live;
        debug_assert!(live.get() > 0, "a queued task was counted down twice");
        live.set(live.get().saturating_sub(1));
        self.executor.queue.idle.notify_waiters();
    }
}

/// Unwraps a queued task's outcome, turning a cancellation into an error and
/// re-raising a panic the way an inline await did.
fn queued_result(
    res: Result<Result<(), ExecutorError>, tokio::task::JoinError>,
) -> Result<(), ExecutorError> {
    match res {
        Ok(result) => result,
        Err(e) if e.is_cancelled() => Err(ExecutorError::Cancelled),
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// A task queued on the runtime's local task set.
///
/// Dropping the handle aborts the task, so abandoning a sibling on the first
/// failure cancels it exactly as dropping an inline future used to.
#[must_use = "dropping a QueuedTask aborts the task it stands for"]
struct QueuedTask {
    handle: Option<tokio::task::JoinHandle<Result<(), ExecutorError>>>,
}

impl QueuedTask {
    /// Waits for the task to finish. Dropping this future before it resolves
    /// aborts the task.
    async fn join(mut self) -> Result<(), ExecutorError> {
        // Always `Some`: `spawn_task` is the only constructor, and the only
        // other consumer is `Drop`. Reported as a cancellation rather than
        // unwrapped, which the crate's lints forbid.
        debug_assert!(self.handle.is_some(), "a queued task was joined twice");
        let Some(handle) = self.handle.as_mut() else {
            return Err(ExecutorError::Cancelled);
        };
        let res = handle.await;
        // Finished, so there is nothing left for `Drop` to abort.
        self.handle = None;
        queued_result(res)
    }
}

impl Drop for QueuedTask {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Executor {
    /// Runs the given calls. Existence and internal checks run first, then a
    /// dry summary if requested, otherwise the calls are executed (in parallel
    /// when `parallel` is set). Ports Go `Run`.
    ///
    /// # Panics
    ///
    /// Must be awaited inside a [`tokio::task::LocalSet`]: dependencies, setup
    /// tasks and nested `task:` commands are queued with `spawn_local`, which
    /// panics outside one.
    ///
    /// One run at a time per executor: the queue is shared, so a concurrent run
    /// on the same executor would tear this one's tasks down with its own.
    ///
    /// A run that *fails* after abandoning a task stops every process below the
    /// host process, not only the ones it started — it cannot tell them apart.
    /// An embedder with children of its own should set `TASK_NO_REAP=1`.
    pub async fn run(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
        let result = self.run_calls(calls).await;
        // However the run ended, nothing queued may outlive it; the commands
        // behind it are only swept when the run is failing.
        self.drain_queue(result.is_err()).await;
        result
    }

    /// The body of [`Self::run`], wrapped so the queue is drained on both the
    /// success and the error exit. A panic unwinds past the drain, taking the
    /// process with it.
    async fn run_calls(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
        // Validate that every requested task exists and is not internal.
        for call in calls {
            let task = self.get_task(call)?;
            if task.internal {
                return Err(ExecutorError::TaskInternal {
                    task_name: call.task.clone(),
                });
            }
        }

        // Collect and prompt for missing required vars across the dependency
        // tree upfront, before any (possibly parallel) execution.
        self.prompt_deps_vars(calls).await?;

        if self.summary {
            let logger = self.logger();
            for (i, call) in calls.iter().enumerate() {
                let compiled = self.fast_compiled_task(call).await?;
                let mut logger = logger.borrow_mut();
                crate::summary::print_space_between_summaries(&mut logger, i);
                crate::summary::print_task(&mut logger, &compiled);
            }
            return Ok(());
        }

        let (regular, watch): (Vec<Call>, Vec<Call>) = self.split_regular_and_watch(calls)?;

        if self.parallel {
            self.run_parallel(&regular).await?;
        } else {
            for call in &regular {
                self.run_task(call.clone()).await?;
            }
        }

        if !watch.is_empty() {
            self.watch_tasks(&watch).await?;
        }
        Ok(())
    }

    /// Runs the given calls concurrently, honoring failfast. Concurrency is
    /// bounded by the run-once dedup and the concurrency limiter inside
    /// `run_task`.
    async fn run_parallel(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
        let mut tasks = Vec::with_capacity(calls.len());
        for call in calls {
            tasks.push(self.spawn_task(call.clone(), None));
        }
        join_queued(self, tasks, self.failfast).await
    }

    /// Whether any of these calls will be watched, so a caller can tell before
    /// the run starts. Watch mode is entered by the flag *or* by a task's own
    /// `watch: true`, and the two must be treated alike: the watch loop blocks
    /// the runtime thread, so anything that needs the runtime to keep turning —
    /// the interrupt handler — has to be skipped for both.
    pub fn will_watch(&self, calls: &[Call]) -> bool {
        if self.watch {
            return true;
        }
        calls
            .iter()
            .any(|call| self.get_task(call).is_ok_and(|task| task.watch))
    }

    fn split_regular_and_watch(
        &self,
        calls: &[Call],
    ) -> Result<(Vec<Call>, Vec<Call>), ExecutorError> {
        let mut regular = Vec::new();
        let mut watch = Vec::new();
        for call in calls {
            let task = self.get_task(call)?;
            if self.watch || task.watch {
                watch.push(call.clone());
            } else {
                regular.push(call.clone());
            }
        }
        Ok((regular, watch))
    }

    /// Finds every task matching a call: a direct name match, a unique alias
    /// match, or one or more wildcard matches. Ports Go `FindMatchingTasks`.
    pub fn find_matching_tasks(&self, call: &Call) -> Result<Vec<MatchingTask<'_>>, ExecutorError> {
        let tf = self
            .taskfile
            .as_ref()
            .ok_or_else(|| ExecutorError::Io("executor not set up".to_string()))?;
        if let Some(task) = tf.tasks.get(&call.task) {
            return Ok(vec![MatchingTask {
                task,
                wildcards: Vec::new(),
            }]);
        }

        let mut aliased = Vec::new();
        let mut matching = Vec::new();
        for task in tf.tasks.values(crate::sort::Sorter::None) {
            if task.aliases.iter().any(|a| a == &call.task) {
                aliased.push(task.task.clone());
                matching.push(MatchingTask {
                    task,
                    wildcards: Vec::new(),
                });
            }
        }
        if aliased.len() == 1 {
            return Ok(matching);
        }
        if aliased.len() > 1 {
            return Err(ExecutorError::TaskNameConflict {
                call: call.task.clone(),
                task_names: aliased,
            });
        }

        let mut wildcard_matches = Vec::new();
        for (_, task) in tf.tasks.all(crate::sort::Sorter::None) {
            let (matched, wildcards) = task.wildcard_match(&call.task);
            if matched {
                wildcard_matches.push(MatchingTask { task, wildcards });
            }
        }
        Ok(wildcard_matches)
    }

    /// Resolves the raw task for a call, injecting the captured wildcards into
    /// the call's `MATCH` variable. Ports Go `GetTask`.
    pub fn get_task(&self, call: &Call) -> Result<Task, ExecutorError> {
        let matching = self.find_matching_tasks(call)?;
        if let Some(first) = matching.first() {
            return Ok(first.task.clone());
        }
        Err(ExecutorError::TaskNotFound {
            task_name: call.task.clone(),
            did_you_mean: String::new(),
        })
    }

    /// Returns the raw task and its wildcard captures for a call.
    fn get_task_with_match(&self, call: &Call) -> Result<(Task, Vec<String>), ExecutorError> {
        let matching = self.find_matching_tasks(call)?;
        if let Some(first) = matching.first() {
            return Ok((first.task.clone(), first.wildcards.clone()));
        }
        Err(ExecutorError::TaskNotFound {
            task_name: call.task.clone(),
            did_you_mean: String::new(),
        })
    }

    /// Compiles a task for a call without evaluating dynamic (`sh:`) variables.
    /// Ports Go `FastCompiledTask`.
    pub async fn fast_compiled_task(&self, call: &Call) -> Result<Task, ExecutorError> {
        self.compiled_task_inner(call, false).await
    }

    /// Compiles a task for a call, evaluating dynamic variables. Ports Go
    /// `CompiledTask`.
    pub async fn compiled_task(&self, call: &Call) -> Result<Task, ExecutorError> {
        self.compiled_task_inner(call, true).await
    }

    async fn compiled_task_inner(
        &self,
        call: &Call,
        evaluate_sh_vars: bool,
    ) -> Result<Task, ExecutorError> {
        let resolver = GlobResolver { exec: self, call };
        self.compiled_task_with(call, evaluate_sh_vars, Some(&resolver))
            .await
    }

    /// The body of [`Self::compiled_task_inner`]: the full compile, resolving
    /// references through `resolver`, or without one the compile of a task a
    /// reference reaches ([`variables::reached_task`]).
    async fn compiled_task_with(
        &self,
        call: &Call,
        evaluate_sh_vars: bool,
        resolver: Option<&dyn crate::variables::TaskResolver>,
    ) -> Result<Task, ExecutorError> {
        let (orig, wildcards) = self.get_task_with_match(call)?;

        // Inject the captured wildcards as the MATCH variable.
        let mut call = call.clone();
        if !wildcards.is_empty() || call.vars.get("MATCH").is_none() {
            let seq = Value::Sequence(wildcards.iter().cloned().map(Value::String).collect());
            call.vars.set(
                "MATCH".to_string(),
                Var {
                    value: Some(seq),
                    ..Default::default()
                },
            );
        }

        let compiler = self.compiler();
        let (mut scratch, sink) = self.scratch_logger();
        let vars = compiler
            .get_variables(Some(&orig), Some(&call), evaluate_sh_vars, &mut scratch)
            .await?;

        let taskfile_env = self
            .taskfile
            .as_ref()
            .map(|tf| tf.env.clone())
            .unwrap_or_default();
        let empty_caches = crate::ast::Caches::default();
        let caches = self
            .taskfile
            .as_ref()
            .map(|tf| &tf.caches)
            .unwrap_or(&empty_caches);
        let ctx = CompileContext {
            dir: &self.dir,
            taskfile_env: &taskfile_env,
            fingerprint_temp_dir: &self.temp_dir.fingerprint,
            env_precedence: self.env_precedence,
            caches,
        };
        let result = match resolver {
            Some(resolver) => {
                variables::compiled_task(
                    &orig,
                    vars,
                    evaluate_sh_vars,
                    &ctx,
                    &compiler,
                    &mut scratch,
                    Some(resolver),
                )
                .await
            }
            None => variables::reached_task(&orig, vars, &ctx, &compiler, &mut scratch).await,
        };
        self.flush_scratch(&sink);
        Ok(result?)
    }

    /// Queues a task on the runtime's local task set and returns its handle.
    ///
    /// Recursive call sites (deps, setup, cmd subtasks) go through this instead
    /// of awaiting the child inline: the child is polled by the runtime, not
    /// from within its parent's poll, so the stack no longer grows with the
    /// depth of the dependency tree.
    fn spawn_task(self: &Rc<Self>, call: Call, ancestors: Ancestors) -> QueuedTask {
        let this = Rc::clone(self);
        // Counted up and handed to the task in one step: the guard is what
        // counts it back down, however the task ends.
        self.queue.live.set(self.queue.live.get().saturating_add(1));
        let live = LiveTask {
            executor: Rc::clone(self),
        };
        // Type-erased so the recursion — a task queueing its own dependencies —
        // stays a finite type for the compiler.
        let task: std::pin::Pin<Box<dyn std::future::Future<Output = _>>> = Box::pin(async move {
            // Held for the task's whole life, including a cancellation.
            let _live = live;
            this.run_task_on(call, ancestors).await
        });
        let handle = tokio::task::spawn_local(task);
        {
            let mut aborts = self.queue.aborts.borrow_mut();
            // A watch session queues tasks for as long as it runs, so drop the
            // finished handles once they outnumber the live ones.
            let live = self.queue.live.get();
            if aborts.len() > live.saturating_mul(2).saturating_add(64) {
                aborts.retain(|abort| !abort.is_finished());
            }
            aborts.push(handle.abort_handle());
        }
        QueuedTask {
            handle: Some(handle),
        }
    }

    /// Cancels everything still queued, waits for those cancellations to run,
    /// and stops the commands they leave behind, so that by the time the run
    /// returns every abandoned task has released its lock and permit, will start
    /// no further command, and has left no process behind.
    ///
    /// The sweep runs only when the run is *failing* and something was actually
    /// abandoned, because it cannot tell a command abandoned mid-flight from a
    /// process a task deliberately left running in the background. Both halves
    /// are needed: a deferred command's error is ignored, so a `defer:` onto a
    /// `failfast:` task abandons siblings and still returns `Ok`, and sweeping
    /// on that would stop a job a successful run meant to leave running.
    pub(crate) async fn drain_queue(&self, failed: bool) {
        if self.queue.live.get() > 0 {
            self.queue.abandoned.set(true);
        }
        // Taken rather than iterated under a shared borrow: `abort()` is
        // foreign code, and the other borrow of this cell is a `borrow_mut`.
        let aborts = std::mem::take(&mut *self.queue.aborts.borrow_mut());
        for abort in &aborts {
            abort.abort();
        }
        while self.queue.live.get() > 0 {
            // `notified()` only registers the waiter when it is first polled, so
            // enable it explicitly before the re-check: a task finishing in
            // between must not leave this waiting for a notification that has
            // already been sent.
            let mut idle = std::pin::pin!(self.queue.idle.notified());
            idle.as_mut().enable();
            if self.queue.live.get() == 0 {
                break;
            }
            idle.await;
        }
        if self.queue.abandoned.replace(false) && failed {
            self.report_swept(
                crate::reap::stop_commands().await,
                "left by the cancelled run",
            );
        }
    }

    /// Logs what a sweep did, `occasion` naming what prompted it. What it
    /// signalled is a detail of a run that was ending anyway, so it is
    /// verbose-only; what it *failed* to signal is a process still running that
    /// nobody is watching, so it warns.
    pub(super) fn report_swept(&self, swept: crate::reap::Swept, occasion: &str) {
        let logger = self.logger();
        let mut logger = logger.borrow_mut();
        if swept.asked > 0 {
            let killed = if swept.killed > 0 {
                format!("; {} still running afterwards and killed", swept.killed)
            } else {
                String::new()
            };
            logger.verbose_errf(
                Color::Yellow,
                &format!(
                    "task: signalled {} process(es) {occasion}{killed}\n",
                    swept.asked
                ),
            );
        }
        // Verbose rather than a warning, unlike the two below: a process that
        // adopts orphans is a container entrypoint or a supervised child, where
        // this holds for every failing run and a warning each time would be
        // noise about a decision the sweep will never make differently.
        if swept.declined {
            logger.verbose_errf(
                Color::Yellow,
                "task: this process adopts orphans, so no command was signalled\n",
            );
        }
        if let Some(found) = swept.truncated_at {
            logger.warnf(&format!(
                "task: stopped after {found} processes below this one; the rest were left \
                 running (the tree was too deep, or part of it could not be read)\n",
            ));
        }
        if !swept.unstoppable.is_empty() {
            let survivors: Vec<String> = swept
                .unstoppable
                .iter()
                .map(|(pid, reason)| format!("{pid} ({reason})"))
                .collect();
            logger.warnf(&format!(
                "task: could not stop {}, which may still be running\n",
                survivors.join(", ")
            ));
        }
    }

    /// Runs a task by resolving it, checking preconditions/fingerprint/cache,
    /// running its dependencies, and executing its commands. Ports Go `RunTask`.
    ///
    /// # Panics
    ///
    /// Must be awaited inside a [`tokio::task::LocalSet`]: dependencies, setup
    /// tasks and nested `task:` commands are queued with `spawn_local`, which
    /// panics outside one.
    ///
    /// Unlike [`Self::run`] this does not drain the queue, so a caller that
    /// keeps the executor alive across calls — the watch loop — has to call
    /// [`Self::drain_queue`] itself once the call returns.
    pub async fn run_task(self: &Rc<Self>, call: Call) -> Result<(), ExecutorError> {
        self.run_task_on(call, None).await
    }

    /// [`Self::run_task`] with the tasks already running above it on this
    /// dependency path, used to detect a cycle before it can recurse.
    pub(crate) async fn run_task_on(
        self: &Rc<Self>,
        mut call: Call,
        ancestors: Ancestors,
    ) -> Result<(), ExecutorError> {
        self.inject_prompted_vars(&mut call);

        let fast = self.fast_compiled_task(&call).await?;
        if !should_run_on_current_platform(&fast.platforms) {
            self.logger().borrow_mut().verbose_errf(
                Color::Yellow,
                &format!("task: {:?} not for current platform - ignored\n", call.task),
            );
            return Ok(());
        }

        // When we cannot prompt, check required vars early for a clear error.
        if !self.can_prompt() {
            requires::check_required_vars_set(&fast)?;
        }

        let mut t = self.compiled_task(&call).await?;

        // Evaluate the task-level `if:` after compilation so dynamic vars are
        // resolved; a non-zero exit skips the task.
        if !t.if_.trim().is_empty() {
            let opts = RunCommandOptions {
                command: t.if_.clone(),
                dir: Some(t.compute_dir()),
                env: split_env(env::get(&t, self.env_precedence)),
                posix_opts: Vec::new(),
                bash_opts: Vec::new(),
                stdout: Stdio::Inherit,
                stderr: Stdio::Inherit,
                sandbox: None,
            };
            if execext::run_command(opts).await.is_err() {
                self.logger().borrow_mut().verbose_outf(
                    Color::Yellow,
                    &format!("task: if condition not met - skipped: {:?}\n", call.task),
                );
                return Ok(());
            }
        }

        // Prompt for missing required vars after the if-check (so a task that
        // will not run does not prompt); recompile when a value was supplied.
        if self.prompt_task_vars(&t, &mut call).await? {
            t = self.compiled_task(&call).await?;
        }

        requires::check_required_vars_set(&t)?;
        requires::check_allowed_values(&t)?;

        // A task already running on this path with the same compiled body
        // depends on itself and will keep doing so. Reported here rather than
        // left to the call counter below, which only trips after a thousand
        // calls — a thousand pointless task compilations, and a cycle reported
        // as "called too many times".
        //
        // It has to run before `execute`, because `run_setup` runs before the
        // up-to-date check and a cycle closed through `setup:` would never
        // terminate otherwise. The cost is that nothing `execute` would have
        // short-circuited on can be seen from here — see CHANGELOG.md.
        //
        // The reported name is `full_name`, the name with wildcards
        // substituted; `t.task` for a wildcard task is the pattern, so `x-2`
        // calling `x-1` would read as `x-*` reaching itself. Compilation always
        // sets it — to the task key when nothing was substituted — so the
        // fallback only covers a task that never went through compilation.
        let name = if t.full_name.is_empty() {
            t.task.as_str()
        } else {
            t.full_name.as_str()
        };
        // Matched on the name *and* a digest of the compiled task, so a task
        // that calls itself with different vars — the countdown idiom, whose
        // commands render differently each turn — makes progress rather than
        // reading as a repeat. `crate::hash::hash` does not hash the vars field
        // itself, but the compiled `cmds`/`dirs`/`sources` it does hash have
        // the resolved values baked in.
        //
        // The digest only sees the compiled body, so recursion whose progress
        // lives outside it — a counter in a file, read by `if:` or by a command
        // — reads as a repeat and is rejected. Telling that apart from a real
        // loop would mean running it, which is the thing being avoided.
        //
        // Hashing is infallible today; the fallback is defensive. Note it
        // degrades towards *missing* a cycle, since a bare name never matches a
        // `name\0digest` key from another level.
        let key = match crate::hash::hash(&t) {
            Ok(digest) => format!("{name}\0{digest}"),
            Err(_) => name.to_string(),
        };
        if let Some(above) = &ancestors
            && above.contains(&key)
        {
            return Err(ExecutorError::CyclicDependency {
                path: above.to_path_through(name),
            });
        }
        // The ancestor chain this task hands to everything it calls.
        let callee_ancestors = Some(CallPath::extend(&ancestors, &key, name));

        // Guard against a task being called too many times without repeating on
        // any single path, via a per-task call counter. Keyed by `t.task`, the
        // pattern rather than the resolved name, on purpose: a thousand
        // instances of one wildcard pattern are the fan-out this guards.
        if !self.watch {
            let mut counts = self.task_call_count.borrow_mut();
            let count = counts.entry(t.task.clone()).or_insert(0);
            *count = count.saturating_add(1);
            if *count >= MAXIMUM_TASK_CALL {
                return Err(ExecutorError::TaskCalledTooManyTimes {
                    task_name: t.task.clone(),
                });
            }
        }

        let result = self.start_execution(&t, &call, callee_ancestors).await;
        result.map_err(|source| ExecutorError::TaskRun {
            task_name: t.name().to_string(),
            source: Box::new(unwrap_arc(source)),
        })
    }

    /// Deduplicates concurrent executions of the same task hash: the first
    /// caller runs the task while later callers await its result. Ports Go
    /// `startExecution` + its `sync.Once`-style execution-hash map.
    async fn start_execution(
        self: &Rc<Self>,
        t: &Task,
        call: &Call,
        ancestors: Ancestors,
    ) -> RunOnceResult {
        let h = self.task_hash(t);
        if h.is_empty() || t.watch {
            return self.execute(t, call, ancestors).await.map_err(Arc::new);
        }

        let cell = {
            let mut map = self.run_once.lock().await;
            map.entry(h)
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        // The first caller runs the task; concurrent callers await the same
        // result via the shared cell.
        let result = cell
            .get_or_init(|| async { self.execute(t, call, ancestors).await.map_err(Arc::new) })
            .await;
        result.clone()
    }

    /// Computes the run-once/dedup hash for a task from its `run:` mode.
    fn task_hash(&self, t: &Task) -> String {
        let run = if t.run.is_empty() {
            self.taskfile
                .as_ref()
                .map(|tf| tf.run.as_str())
                .unwrap_or("always")
        } else {
            t.run.as_str()
        };
        match run {
            "always" => String::new(),
            "once" => crate::hash::name(t).unwrap_or_default(),
            "when_changed" => crate::hash::hash(t).unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The core task execution body run once per dedup hash: setup, then the
    /// build-once lock around [`Self::execute_locked`], released before
    /// returning. Ports the closure passed to Go `startExecution`.
    async fn execute(
        self: &Rc<Self>,
        t: &Task,
        call: &Call,
        ancestors: Ancestors,
    ) -> Result<(), ExecutorError> {
        self.logger()
            .borrow_mut()
            .verbose_errf(Color::Magenta, &format!("task: {:?} started\n", call.task));

        self.run_setup(t, &ancestors).await?;

        let cache_url = self.cache_url(t);

        // Acquire the build-once lock covering deps, fingerprint, execution and
        // the up-to-date write, for tasks with fingerprint state.
        let lock = self
            .acquire_task_lock(t, &t.cache_checksum, cache_url.as_ref())
            .await?;

        let result = self
            .execute_locked(t, call, ancestors, cache_url.as_ref(), lock.as_ref())
            .await;
        // Release explicitly: a dropped vk lease only stops its heartbeat and
        // keeps peers waiting until it expires server-side.
        if let Some(guard) = lock
            && let Err(e) = guard.unlock().await
        {
            self.logger().borrow_mut().verbose_errf(
                Color::Yellow,
                &format!("task: failed to release the lock for {:?}: {e}\n", t.name()),
            );
        }
        result
    }

    /// The part of [`Self::execute`] that runs under the build-once lock: the
    /// fingerprint/cache short-circuit, deps, commands, then the up-to-date
    /// write and cache upload.
    async fn execute_locked(
        self: &Rc<Self>,
        t: &Task,
        call: &Call,
        ancestors: Ancestors,
        cache_url: Option<&CacheUrl>,
        lock: Option<&cache::Guard>,
    ) -> Result<(), ExecutorError> {
        let mut checker = ChecksumChecker::new(&self.temp_dir.fingerprint, t.clone());
        let source_hash = t.source_hash.as_str();
        let cache_active = cache_url.is_some();
        let skip_fingerprinting = self.force_all || (!call.indirect && self.force);
        if !skip_fingerprinting {
            let precond_met = {
                let (mut scratch, sink) = self.scratch_logger();
                let r = precondition::check(t, &mut scratch, self.env_precedence).await;
                self.flush_scratch(&sink);
                r
            };
            let precond_met = precond_met?;

            let up_to_date = checker.is_up_to_date()?;
            if up_to_date && precond_met {
                self.log_up_to_date(t, call);
                return Ok(());
            }

            // Try the remote cache before running deps.
            if cache_active
                && !self.dry
                && !source_hash.is_empty()
                && let Some(url) = cache_url
            {
                {
                    let source_value = checker.source_value().to_string();
                    let (ok, meta) = {
                        let (mut scratch, sink) = self.scratch_logger();
                        let r = cache::cache_restore(
                            url,
                            t,
                            &source_value,
                            std::path::Path::new(&self.dir),
                            &mut scratch,
                        )
                        .await;
                        self.flush_scratch(&sink);
                        r
                    };
                    if ok {
                        match self.cache_verify_meta(t, &mut checker, &meta) {
                            Ok(()) => return Ok(()),
                            Err(e) => {
                                self.logger().borrow_mut().errf(
                                    Color::Yellow,
                                    &format!(
                                        "task: WARNING: cache for {:?}: {e}, running task normally\n",
                                        t.name()
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        self.run_deps(t, &ancestors).await?;

        // Task-level prompts.
        for p in &t.prompt.0 {
            if !p.is_empty() && !self.dry {
                self.confirm_or_cancel(p, &call.task).await?;
            }
        }

        if let Err(e) = self.mkdir(t).await {
            self.logger().borrow_mut().errf(
                Color::Red,
                &format!(
                    "task: cannot make directory {:?}: {e}\n",
                    t.compute_dir().to_string_lossy()
                ),
            );
        }

        // Commands run within the sandbox layers above this task, plus its own.
        let mut sandbox_tmp = SandboxTmp(None);
        let ancestors = match self.sandbox_policy(t).await? {
            Some(policy) => {
                sandbox_tmp.0 = policy.tmpdir.clone();
                let layers = Layers::push(CallPath::layers(&ancestors).as_deref(), policy);
                CallPath::with_layers(&ancestors, layers)
            }
            None => ancestors,
        };

        // Run commands; deferred commands run after the rest, in reverse.
        let mut deferred = Vec::new();
        let mut run_err = None;
        // The exit code of the last failing command, exposed to deferred commands
        // as `.EXIT_CODE` (ports Go's `deferredExitCode`).
        let mut deferred_exit_code: u8 = 0;
        for (i, cmd) in t.cmds.iter().enumerate() {
            if cmd.defer {
                deferred.push(i);
                continue;
            }
            if let Err(e) = self.run_command(t, call, i, &ancestors).await {
                let _ = checker.on_error();
                let code = e.task_exit_code();
                if code > 0 {
                    deferred_exit_code = code.clamp(0, 255) as u8;
                }
                if let ExecutorError::Exec(execext::Error::NonZeroExit(_)) = &e
                    && (cmd.ignore_error || t.ignore_error)
                {
                    self.logger()
                        .borrow_mut()
                        .verbose_errf(Color::Yellow, &format!("task: task error ignored: {e}\n"));
                    continue;
                }
                run_err = Some(e);
                break;
            }
        }

        for &i in deferred.iter().rev() {
            let denied = self
                .run_deferred(t, call, i, deferred_exit_code, &ancestors)
                .await;
            if let Some(e) = denied
                && run_err.is_none()
            {
                // As for a failed command: the error being reported matters
                // more than a failure to mark the task out of date.
                let _ = checker.on_error();
                run_err = Some(e);
            }
        }
        drop(sandbox_tmp);

        // A remote lock lost mid-run means a peer may have been running this
        // same task concurrently, so this run's fingerprint and cache entry
        // cannot be trusted as the build-once result. Read here, before the
        // early return and before anything that can fail, so the warning also
        // reaches an operator whose task errored out.
        let lock_lost = lock.is_some_and(cache::Guard::is_lost);
        if lock_lost {
            self.logger().borrow_mut().errf(
                Color::Yellow,
                &format!(
                    "task: WARNING: lost the distributed lock during execution of {:?} \
                     (another run may have held it); not publishing its fingerprint or cache entry\n",
                    t.name()
                ),
            );
        }

        if let Some(e) = run_err {
            return Err(e);
        }

        if !self.dry {
            // Skip the fingerprint and the cache upload, exactly as for sources
            // that changed underneath us.
            if lock_lost {
                let _ = checker.on_error();
            } else if checker.sources_changed()? {
                let changed = checker.changed_sources().unwrap_or_default();
                let files = if changed.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", changed.join(", "))
                };
                self.logger().borrow_mut().errf(
                    Color::Yellow,
                    &format!(
                        "task: WARNING: sources for {:?} changed during execution{files}; \
                         not publishing its fingerprint or cache entry. If these are files the \
                         task generates, tighten its sources/exclude so they are not matched\n",
                        t.name()
                    ),
                );
                let _ = checker.on_error();
            } else {
                checker.set_up_to_date()?;
                if cache_active
                    && !source_hash.is_empty()
                    && let Some(url) = cache_url
                {
                    let (mut scratch, sink) = self.scratch_logger();
                    cache::cache_save(
                        url,
                        t,
                        std::path::Path::new(&self.dir),
                        &self.temp_dir.fingerprint,
                        &mut scratch,
                    )
                    .await;
                    self.flush_scratch(&sink);
                }
            }
        }

        self.logger()
            .borrow_mut()
            .verbose_errf(Color::Magenta, &format!("task: {:?} finished\n", call.task));
        Ok(())
    }

    fn log_up_to_date(&self, t: &Task, call: &Call) {
        let taskfile_silent = self.taskfile.as_ref().map(|tf| tf.silent).unwrap_or(false);
        let show =
            self.verbose || (!call.silent && !t.is_silent() && !taskfile_silent && !self.silent);
        if show {
            let name = if self.output_style.name == "prefixed" {
                t.prefix.clone()
            } else {
                t.name().to_string()
            };
            self.logger().borrow_mut().errf(
                Color::Magenta,
                &format!("task: Task {name:?} is up to date\n"),
            );
        }
    }

    /// Runs setup tasks sequentially and unconditionally. Ports Go `runSetup`.
    /// Holds no concurrency slot: one is taken only around a command.
    async fn run_setup(
        self: &Rc<Self>,
        t: &Task,
        ancestors: &Ancestors,
    ) -> Result<(), ExecutorError> {
        for d in &t.setup {
            let call = Call {
                task: d.task.clone(),
                vars: d.vars.clone().unwrap_or_default(),
                silent: d.silent,
                indirect: true,
            };
            self.spawn_task(call, ancestors.clone()).join().await?;
        }
        Ok(())
    }

    /// Runs a task's dependencies concurrently, honoring failfast. Ports Go
    /// `runDeps`.
    async fn run_deps(
        self: &Rc<Self>,
        t: &Task,
        ancestors: &Ancestors,
    ) -> Result<(), ExecutorError> {
        let mut tasks = Vec::with_capacity(t.deps.len());
        for d in &t.deps {
            let call = Call {
                task: d.task.clone(),
                vars: d.vars.clone().unwrap_or_default(),
                silent: d.silent,
                indirect: true,
            };
            tasks.push(self.spawn_task(call, ancestors.clone()));
        }
        join_queued(self, tasks, self.failfast || t.failfast).await
    }

    /// Runs a single deferred command. Deferred commands are left un-templated
    /// during compilation so they can be rendered here against the task's
    /// variables plus `EXIT_CODE` (the failing command's exit status). Errors are
    /// ignored except for sandbox denials and conditions that could not run
    /// sandboxed; these fail the task. Ports Go `runDeferred`.
    async fn run_deferred(
        self: &Rc<Self>,
        t: &Task,
        call: &Call,
        i: usize,
        exit_code: u8,
        ancestors: &Ancestors,
    ) -> Option<ExecutorError> {
        let cmd = t.cmds.get(i)?;
        let mut cache = TemplaterCache::new(t.vars.clone().unwrap_or_default());
        cache.set_dialect(t.dialect);
        let mut extra: indexmap::IndexMap<String, crate::yaml::Value> = indexmap::IndexMap::new();
        if exit_code > 0 {
            extra.insert(
                "EXIT_CODE".to_string(),
                crate::yaml::Value::String(exit_code.to_string()),
            );
        }
        let mut rendered = cmd.clone();
        rendered.cmd = cache.replace_with_extra(&cmd.cmd, &extra);
        rendered.task = cache.replace_with_extra(&cmd.task, &extra);
        rendered.if_ = cache.replace_with_extra(&cmd.if_, &extra);
        rendered.vars = cmd
            .vars
            .as_ref()
            .and_then(|v| cache.replace_vars_with_extra(v, &extra));

        let mut task = t.clone();
        if let Some(slot) = task.cmds.get_mut(i) {
            *slot = rendered;
        }
        match self.run_command(&task, call, i, ancestors).await {
            // Not ignored: the task's outputs may depend on an undeclared input,
            // or on a condition that could not be checked.
            Err(e) if e.is_sandbox_failure() => Some(e),
            Err(e) => {
                self.logger().borrow_mut().verbose_errf(
                    Color::Yellow,
                    &format!("task: ignored error in deferred cmd: {e}\n"),
                );
                None
            }
            Ok(()) => None,
        }
    }

    /// Executes command `i` of task `t`: a nested task call or a shell command,
    /// honoring `if:`, platform, silent, dry-run, and output wrapping. Ports Go
    /// `runCommand`.
    async fn run_command(
        self: &Rc<Self>,
        t: &Task,
        call: &Call,
        i: usize,
        ancestors: &Ancestors,
    ) -> Result<(), ExecutorError> {
        let Some(cmd) = t.cmds.get(i) else {
            return Ok(());
        };

        if !cmd.if_.trim().is_empty() {
            let (env, sandbox) = self.command_env(t, ancestors);
            let sandbox = self.condition_sandbox(t, sandbox.as_ref()).await;
            let opts = RunCommandOptions {
                command: cmd.if_.clone(),
                dir: Some(t.compute_dir()),
                env,
                posix_opts: Vec::new(),
                bash_opts: Vec::new(),
                stdout: Stdio::Inherit,
                stderr: Stdio::Inherit,
                sandbox: sandbox.clone(),
            };
            let before = sandbox.as_ref().map_or(0, Sandbox::denial_count);
            let result = execext::run_command(opts).await;
            let denied = self.report_sandbox(t, sandbox.as_ref(), false, before);
            // A sandboxed condition is false when it says so, not when the
            // sandbox refused it something or could not start; any other
            // error makes it false, as without a sandbox.
            if denied {
                return Err(ExecutorError::SandboxDenied {
                    task_name: t.name().to_string(),
                    denied: Denied::Condition,
                });
            }
            match result {
                Ok(()) => {}
                Err(execext::Error::Sandbox(source)) => {
                    return Err(ExecutorError::ConditionSandbox {
                        task_name: t.name().to_string(),
                        source,
                    });
                }
                Err(_) => {
                    self.logger().borrow_mut().verbose_outf(
                        Color::Yellow,
                        &format!("task: [{}] if condition not met - skipped\n", t.name()),
                    );
                    return Ok(());
                }
            }
        }

        if !cmd.task.is_empty() {
            let sub = Call {
                task: cmd.task.clone(),
                vars: cmd.vars.clone().unwrap_or_default(),
                silent: cmd.silent,
                indirect: true,
            };
            let how = if cmd.defer {
                Ignored::All
            } else if cmd.ignore_error || t.ignore_error {
                Ignored::Callee
            } else {
                Ignored::No
            };
            let path = CallPath::calling(ancestors, how);
            let result = self.spawn_task(sub, path).join().await;
            if let Err(ExecutorError::TaskRun { source, .. }) = &result
                && matches!(
                    &**source,
                    ExecutorError::Exec(execext::Error::NonZeroExit(_))
                )
                && (cmd.ignore_error || t.ignore_error)
            {
                self.logger().borrow_mut().verbose_errf(
                    Color::Yellow,
                    &format!("task: [{}] task error ignored\n", t.name()),
                );
                return Ok(());
            }
            return result;
        }

        if !cmd.cmd.is_empty() {
            if !should_run_on_current_platform(&cmd.platforms) {
                self.logger().borrow_mut().verbose_outf(
                    Color::Yellow,
                    &format!(
                        "task: [{}] {} not for current platform - ignored\n",
                        t.name(),
                        cmd.cmd
                    ),
                );
                return Ok(());
            }

            let taskfile_silent = self.taskfile.as_ref().map(|tf| tf.silent).unwrap_or(false);
            let echo = self.verbose
                || (!call.silent
                    && !cmd.silent
                    && !t.is_silent()
                    && !taskfile_silent
                    && !self.silent);
            if echo {
                self.logger()
                    .borrow_mut()
                    .errf(Color::Green, &format!("task: [{}] {}\n", t.name(), cmd.cmd));
            }

            if self.dry {
                return Ok(());
            }

            let tf_set = self
                .taskfile
                .as_ref()
                .map(|tf| tf.set.clone())
                .unwrap_or_default();
            let tf_shopt = self
                .taskfile
                .as_ref()
                .map(|tf| tf.shopt.clone())
                .unwrap_or_default();
            let posix =
                slicesext::unique_join(&[tf_set.as_slice(), t.set.as_slice(), cmd.set.as_slice()]);
            let bash = slicesext::unique_join(&[
                tf_shopt.as_slice(),
                t.shopt.as_slice(),
                cmd.shopt.as_slice(),
            ]);

            // Failures ignored by the command, task or caller, including deferred
            // commands whose errors are discarded.
            let ignored =
                cmd.ignore_error || t.ignore_error || cmd.defer || CallPath::ignored(ancestors);
            let result = self
                .exec_shell(t, call, &cmd.cmd, posix, bash, ancestors, ignored)
                .await;
            if let Err(ExecutorError::Exec(execext::Error::NonZeroExit(_))) = &result
                && cmd.ignore_error
            {
                self.logger().borrow_mut().verbose_errf(
                    Color::Yellow,
                    &format!("task: [{}] command error ignored\n", t.name()),
                );
                return Ok(());
            }
            return result;
        }

        Ok(())
    }

    /// Runs a shell command through the configured output style.
    ///
    /// Interactive tasks and the passthrough ([`Interleaved`](crate::output::Interleaved))
    /// style let the command inherit the process streams directly, so its output
    /// is seen live as it runs. The buffering styles ([`Group`](crate::output::Group),
    /// which must buffer, and [`Prefixed`](crate::output::Prefixed), which
    /// rewrites each line) capture the output into thread-safe buffers and replay
    /// it through the style on close — keeping the `!Send` style writers on the
    /// current thread while the shell's capture drain runs on a helper thread.
    /// A failure `ignore_error` lets pass is not pointed at the sandbox.
    #[allow(clippy::too_many_arguments)]
    async fn exec_shell(
        &self,
        t: &Task,
        call: &Call,
        command: &str,
        posix: Vec<String>,
        bash: Vec<String>,
        ancestors: &Ancestors,
        ignore_error: bool,
    ) -> Result<(), ExecutorError> {
        let (env, sandbox) = self.command_env(t, ancestors);

        // Stream directly to the process streams when nothing needs to intercept
        // the output: no capture, no replay, output seen live.
        if t.interactive || self.output.is_passthrough() {
            let opts = RunCommandOptions {
                command: command.to_string(),
                dir: Some(t.compute_dir()),
                env,
                posix_opts: posix,
                bash_opts: bash,
                stdout: Stdio::Inherit,
                stderr: Stdio::Inherit,
                sandbox: sandbox.clone(),
            };
            let _permit = self.limiter.acquire().await;
            let before = sandbox.as_ref().map_or(0, Sandbox::denial_count);
            let run_result = execext::run_command(opts).await;
            drop(_permit);
            let failed = !ignore_error && exited_nonzero(&run_result);
            let denied = self.report_sandbox(t, sandbox.as_ref(), failed, before);
            sandbox_verdict(t, denied)?;
            return run_result.map_err(ExecutorError::Exec);
        }

        let compiler = self.compiler();
        let vars = {
            let (mut scratch, sink) = self.scratch_logger();
            let r = compiler
                .fast_get_variables(Some(t), Some(call), &mut scratch)
                .await;
            self.flush_scratch(&sink);
            r?
        };

        let out_buf = SharedBytes::default();
        let err_buf = SharedBytes::default();
        let opts = RunCommandOptions {
            command: command.to_string(),
            dir: Some(t.compute_dir()),
            env,
            posix_opts: posix,
            bash_opts: bash,
            stdout: Stdio::Capture(Box::new(out_buf.clone())),
            stderr: Stdio::Capture(Box::new(err_buf.clone())),
            sandbox: sandbox.clone(),
        };

        // Bound concurrent command execution by the configured limit.
        let _permit = self.limiter.acquire().await;
        let before = sandbox.as_ref().map_or(0, Sandbox::denial_count);
        let run_result = execext::run_command(opts).await;
        drop(_permit);

        // Replay the captured output through the output style on the current
        // thread.
        let output: Rc<dyn Output> = Rc::clone(&self.output);
        let mut tcache = TemplaterCache::new(vars);
        let out_sink: SharedWriter = Rc::new(std::cell::RefCell::new(std::io::stdout()));
        let err_sink: SharedWriter = Rc::new(std::cell::RefCell::new(std::io::stderr()));
        let wrapped = output.wrap_writer(out_sink, err_sink, &t.prefix, Some(&mut tcache));
        {
            let _ = wrapped.stdout.borrow_mut().write_all(&out_buf.take());
            let _ = wrapped.stderr.borrow_mut().write_all(&err_buf.take());
        }
        let err_ref: Option<&dyn std::error::Error> = match &run_result {
            Ok(()) => None,
            Err(e) => Some(e),
        };
        if let Err(close_err) = (wrapped.close)(err_ref) {
            self.logger().borrow_mut().errf(
                Color::Red,
                &format!("task: unable to close writer: {close_err}\n"),
            );
        }
        // After the command's own output, which `group` only now flushed.
        let failed = !ignore_error && exited_nonzero(&run_result);
        let denied = self.report_sandbox(t, sandbox.as_ref(), failed, before);
        sandbox_verdict(t, denied)?;
        run_result.map_err(ExecutorError::Exec)
    }

    /// The sandbox backend, resolved from `TASK_SANDBOX` on first use; a
    /// fallback is warned about once.
    async fn sandbox_backend(&self) -> Result<Backend, ExecutorError> {
        let resolved = self
            .sandbox_backend
            .get_or_init(|| async {
                // Probing the seccomp supervisor blocks for a while.
                let mode = self.sandbox_mode;
                let resolved = tokio::task::spawn_blocking(move || Backend::resolve(mode))
                    .await
                    .unwrap_or_else(|e| Err(format!("resolving TASK_SANDBOX: {e}")));
                if let Ok((_, Some(warning))) = &resolved {
                    self.logger()
                        .borrow_mut()
                        .warnf(&format!("sandbox: {warning}\n"));
                }
                resolved.map(|(backend, _)| backend)
            })
            .await;
        resolved.clone().map_err(ExecutorError::Io)
    }

    /// Builds the policy of a task with `sandbox:`, after its deps have run so
    /// that the sources they generate exist; `None` when the task has none or
    /// sandboxing is off.
    async fn sandbox_policy(&self, t: &Task) -> Result<Option<Arc<Policy>>, ExecutorError> {
        let Some(sandbox) = &t.sandbox else {
            return Ok(None);
        };
        if self.dry || self.sandbox_backend().await? == Backend::None {
            return Ok(None);
        }
        let dir = t.compute_dir();
        let dir_str = dir.to_string_lossy();
        let root = std::path::Path::new(&self.dir);
        let fail = |e: std::io::Error| {
            ExecutorError::Io(format!("setting up the sandbox of {:?}: {e}", t.name()))
        };

        let mut readable: Vec<std::path::PathBuf> = declared_files(&dir_str, &t.sources)
            .map_err(fail)?
            .into_iter()
            .chain(declared_files(&dir_str, &sandbox.reads).map_err(fail)?)
            .chain(self.taskfile_paths.iter().cloned())
            .map(std::path::PathBuf::from)
            .collect();
        if let Some(tf) = &self.taskfile {
            readable.extend(
                tf.dotenv
                    .iter()
                    .map(|f| filepathext::smart_join(&self.dir, f)),
            );
        }
        readable.extend(
            t.dotenv
                .iter()
                .map(|f| filepathext::smart_join(&dir_str, f)),
        );
        // What the tasks `sandbox.writes` names declare, and those they run.
        let grants: Vec<&crate::ast::Grant> = sandbox
            .task_grants
            .iter()
            .flat_map(|(_, grants)| grants)
            .filter(|g| !g.path.is_empty())
            .collect();
        readable.extend(
            grants
                .iter()
                .filter(|g| g.kind == crate::ast::GrantKind::Dotenv)
                .map(|g| std::path::PathBuf::from(&g.path)),
        );
        let imported: Vec<(String, std::path::PathBuf)> = grants
            .iter()
            .filter(|g| g.kind == crate::ast::GrantKind::Writes)
            .map(|g| {
                let what = format!("{}: writes", g.what);
                (what, std::path::PathBuf::from(&g.path))
            })
            .collect();

        let writable: Vec<std::path::PathBuf> = sandbox
            .writes
            .iter()
            .map(|w| filepathext::smart_join(&dir_str, w))
            .collect();
        let temp_dir = std::path::PathBuf::from(&self.temp_dir.fingerprint);
        // The task's own generates, and those of the tasks `sandbox.writes`
        // names, the referenced ones absolute already. Those are shown
        // relative to the root, as the own ones are relative to the task dir.
        let own: Vec<_> = t
            .generates
            .iter()
            .map(|g| (format!("generates {:?}", g.glob), g))
            .collect();
        for (task, grants) in &sandbox.task_grants {
            let writing = grants.iter().any(|g| {
                !g.path.is_empty()
                    && matches!(
                        g.kind,
                        crate::ast::GrantKind::Generates | crate::ast::GrantKind::Writes
                    )
            });
            if !writing {
                self.logger().borrow_mut().warnf(&format!(
                    "sandbox: {:?}: sandbox.writes {{task: {task}}} grants nothing: {task:?} \
                     and the tasks it runs declare no generates or writes\n",
                    t.name()
                ));
            }
        }
        let referenced: Vec<(String, std::path::PathBuf)> = grants
            .iter()
            .filter(|g| g.kind == crate::ast::GrantKind::Generates)
            .map(|g| {
                let shown = std::path::Path::new(&g.path)
                    .strip_prefix(root)
                    .map_or_else(|_| g.path.clone(), |p| p.to_string_lossy().into_owned());
                let what = format!("{}: generates {shown:?}", g.what);
                (what, generated_dir(std::path::Path::new(&g.path)))
            })
            .collect();
        for g in &grants {
            let what = match g.kind {
                crate::ast::GrantKind::Generates => "generates",
                crate::ast::GrantKind::Writes => "writes",
                crate::ast::GrantKind::Dotenv => "dotenv",
            };
            self.logger().borrow_mut().verbose_errf(
                Color::Magenta,
                &format!(
                    "task: [{}] sandbox grants {}: {what} {:?}\n",
                    t.name(),
                    g.what,
                    g.declared
                ),
            );
        }
        let generated = own
            .into_iter()
            .filter(|(_, g)| !g.negate && !g.glob.is_empty())
            .map(|(what, g)| {
                let dir = generated_dir(&filepathext::smart_join(&dir_str, &g.glob));
                (what, dir)
            })
            .collect();

        // A directory per run, locked while it lives (see `Policy::build`), so
        // that concurrent ones never clear each other's; removed once the task
        // ran (see `SandboxTmp`). Random, as processes in other pid namespaces
        // may share `.task`.
        let tmp = std::path::Path::new(&self.temp_dir.fingerprint).join("tmp");
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let random = std::hash::BuildHasher::hash_one(
            &std::collections::hash_map::RandomState::new(),
            (std::process::id(), run, std::time::SystemTime::now()),
        );
        let private_tmp = tmp.join(format!("{}-{run}-{random:016x}", std::process::id()));

        // The walk of the root can be long: off the engine's thread.
        let (task, root) = (t.name().to_string(), root.to_path_buf());
        let policy = tokio::task::spawn_blocking(move || {
            remove_stale_tmp(&tmp);
            Policy::build(PolicyInput {
                task: &task,
                root: &root,
                readable,
                writable,
                temp_dir,
                generated,
                referenced,
                imported,
                system_tmp: std::env::temp_dir(),
                private_tmp,
            })
        })
        .await
        .map_err(|e| fail(std::io::Error::other(e)))?
        .map_err(fail)?;
        if let Some(tmp) = &policy.tmpdir
            && tmp.to_str().is_none()
        {
            return Err(ExecutorError::Io(format!(
                "sandbox of {:?}: TMPDIR {} is not UTF-8",
                t.name(),
                tmp.display()
            )));
        }
        for warning in &policy.warnings {
            self.logger()
                .borrow_mut()
                .warnf(&format!("sandbox: {:?}: {warning}\n", t.name()));
        }
        let backend = self.sandbox_backend().await?;
        self.logger().borrow_mut().verbose_errf(
            Color::Magenta,
            &format!(
                "task: [{}] sandboxed ({backend:?}, {} rules)\n",
                t.name(),
                policy.rule_count()
            ),
        );
        Ok(Some(Arc::new(policy)))
    }

    /// The sandbox a command at `ancestors` runs in, if any task above it has
    /// one.
    fn sandbox_for(&self, ancestors: &Ancestors) -> Option<Sandbox> {
        let layers = CallPath::layers(ancestors)?;
        let backend = self.sandbox_backend.get()?.as_ref().ok().copied()?;
        Some(Sandbox::new(backend, layers))
    }

    /// The environment a command of `t` at `ancestors` runs with, and the
    /// sandbox it runs in: a sandbox may need its own `TMPDIR`.
    fn command_env(
        &self,
        t: &Task,
        ancestors: &Ancestors,
    ) -> (Vec<(String, String)>, Option<Sandbox>) {
        let mut env = split_env(env::get(t, self.env_precedence));
        let sandbox = self.sandbox_for(ancestors);
        // UTF-8, checked when the policy was built.
        if let Some(tmp) = sandbox
            .as_ref()
            .and_then(Sandbox::tmpdir)
            .and_then(std::path::Path::to_str)
        {
            if env.is_empty() {
                // The shell inherits no environment once given one: pass the
                // process's on, as far as it fits the `String` pairs.
                env = std::env::vars_os()
                    .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                    .collect();
            }
            env.retain(|(k, _)| k != "TMPDIR");
            env.push(("TMPDIR".to_string(), tmp.to_string()));
        }
        (env, sandbox)
    }

    /// The sandbox a command's `if` runs under, given the command's own; warns
    /// once when Landlock enforces the command but its `if` goes unchecked,
    /// without the seccomp supervisor.
    async fn condition_sandbox(&self, t: &Task, sandbox: Option<&Sandbox>) -> Option<Sandbox> {
        let sandbox = sandbox?;
        // Probing blocks the first time: only Landlock asks for it.
        let probed = if matches!(sandbox.backend, Backend::Landlock(_)) {
            tokio::task::spawn_blocking(crate::sandbox::supervisor_available)
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        } else {
            Ok(())
        };
        match sandbox.for_condition(|| probed) {
            Condition::Sandboxed(s) => Some(s),
            Condition::Unsandboxed => None,
            Condition::Unchecked(why) => {
                if !self.condition_unchecked_warned.replace(true) {
                    self.logger().borrow_mut().warnf(&format!(
                        "task: [{}] sandbox: the seccomp supervisor is unavailable ({why}): the \
                         if conditions of this run's commands run unsandboxed; the commands \
                         themselves stay under Landlock\n",
                        t.name()
                    ));
                }
                None
            }
        }
    }

    /// Reports what a sandboxed command was denied, or would have been when
    /// auditing; returns whether anything was denied.
    ///
    /// `before` is [`Sandbox::denial_count`] from before the command ran: any
    /// denial since counts against it. A command running concurrently under
    /// the same layers (the parallel `deps` of a called task) may have made
    /// it, and may be the one to print it: both fail, whichever made it, as
    /// the supervisor cannot tell commands sharing its filter apart.
    fn report_sandbox(
        &self,
        t: &Task,
        sandbox: Option<&Sandbox>,
        failed: bool,
        before: u64,
    ) -> bool {
        let Some(sandbox) = sandbox else {
            return false;
        };
        let audit = sandbox.backend == (Backend::Seccomp { enforce: false });
        let logger = self.logger();
        let mut logger = logger.borrow_mut();
        let color = if audit { Color::Yellow } else { Color::Red };
        if sandbox.unenforced() && !self.sandbox_unenforced_warned.replace(true) {
            let what = if sandbox.landlock_condition {
                "the if conditions of this run's commands are checked only against the \
                 enclosing tasks' rules; the commands themselves stay under Landlock"
            } else {
                "this run's sandboxes are not enforced"
            };
            logger.warnf(&format!(
                "task: [{}] sandbox: this task runs under the seccomp supervisor of an enclosing \
                 sandboxed command, which checks only the enclosing tasks' rules: {what}\n",
                t.name()
            ));
        }
        let denied = !audit && sandbox.denial_count() > before;
        let taken = sandbox.take_denials();
        // Each distinct denial is shown once per supervisor: a repeat only
        // counts.
        let new_denial = taken.iter().any(|d| !matches!(d, Denial::Foreign { .. }));
        if denied && !new_denial {
            logger.errf(
                color,
                &format!(
                    "task: [{}] sandbox: denied again an access already reported above\n",
                    t.name()
                ),
            );
        }
        for d in taken {
            let what = match d {
                Denial::Refused { task, access, path } => {
                    let verb = if audit { "would deny" } else { "denied" };
                    let place = match access {
                        "create" | "remove" | "rename" => "in",
                        _ => "of",
                    };
                    format!(
                        "{verb} {access} {place} {} (not allowed by {task:?})",
                        path.display()
                    )
                }
                Denial::Unchecked { syscall, why } => {
                    let verb = if audit { "did not check" } else { "denied" };
                    format!("{verb} {syscall}: its paths cannot be checked ({why})")
                }
                Denial::Foreign { syscalls } if audit => {
                    format!("let {syscalls} through unchecked (shown once)")
                }
                Denial::Foreign { syscalls } => format!(
                    "failed {syscalls} with \"Permission denied\": their paths cannot be \
                     checked (shown once)"
                ),
            };
            logger.errf(color, &format!("task: [{}] sandbox: {what}\n", t.name()));
        }
        if failed && matches!(sandbox.backend, Backend::Landlock(_)) {
            logger.errf(
                Color::Yellow,
                &format!(
                    "task: [{}] ran sandboxed: a \"Permission denied\" above may be a file \
                     missing from sources; TASK_SANDBOX=audit lists every one\n",
                    t.name()
                ),
            );
        }
        denied
    }

    /// Creates the task's working directory if it does not exist, serialized per
    /// task name. Ports Go `mkdir`.
    async fn mkdir(&self, t: &Task) -> Result<(), ExecutorError> {
        let dir = t.compute_dir();
        if dir.as_os_str().is_empty() {
            return Ok(());
        }
        let lock = {
            let mut locks = self.mkdir_locks.lock().await;
            locks
                .entry(t.task.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Prompts for confirmation of a task-level `prompt:` string, mapping a
    /// decline or unavailable terminal to a cancellation error.
    async fn confirm_or_cancel(&self, message: &str, _task: &str) -> Result<(), ExecutorError> {
        if let Some(prompter) = &self.prompter {
            let prompter = Arc::clone(prompter);
            let message = message.to_string();
            // Mapped like every other prompter failure here, so an unreachable
            // terminal cancels rather than surfacing a different error, and a
            // different exit code, than a declined prompt.
            let answer = super::asked_off_thread(move || prompter.confirm(&message))
                .await
                .map_err(|_| ExecutorError::Cancelled)?;
            match answer {
                Ok(true) => Ok(()),
                Ok(false) => Err(ExecutorError::Cancelled),
                Err(super::PromptError::Cancelled) => Err(ExecutorError::Cancelled),
                Err(super::PromptError::Unavailable(_)) => Err(ExecutorError::Cancelled),
            }
        } else {
            // No prompter: fall back to the logger's yes/no prompt, matching the
            // Go behavior where a non-terminal session cancels.
            let logger = self.logger();
            let mut logger = logger.borrow_mut();
            match logger.prompt(Color::Yellow, message, "n", &["y", "yes"]) {
                Ok(()) => Ok(()),
                Err(_) => Err(ExecutorError::Cancelled),
            }
        }
    }

    // ---- cache helpers ----

    /// Parses the task's resolved `cache.url`, returning `None` when caching is
    /// disabled or unset. Ports Go `cacheEnabled` + `evalCacheURL`.
    fn cache_url(&self, t: &Task) -> Option<CacheUrl> {
        if !cache_enabled(t) {
            return None;
        }
        let c = t.cache.as_ref()?;
        let mut url = CacheUrl::parse(&c.url).unwrap_or_default();
        // A `cache.api_key` outranks every other credential, the URL's
        // included — the same order as the lock's.
        if let Some(CacheUrl::Oci { opts, .. }) = &mut url
            && !c.api_key.is_empty()
        {
            opts.token = c.api_key.clone();
        }
        url
    }

    /// Acquires the build-once lock for a task with both sources and generates,
    /// covering deps, the fingerprint check, execution, and the up-to-date
    /// write. Uses the configured `cache.lock` when set, otherwise a local
    /// filesystem lock under `<temp>/locks`, so concurrent invocations of the
    /// same fingerprinted task serialize. The key is the task's local name and
    /// `cache_checksum` — the identity its cache entry is keyed by — so every
    /// include path to a task takes the same lock. Ports the locking block of
    /// Go `RunTask` (`e.Locker` = a flock).
    async fn acquire_task_lock(
        &self,
        t: &Task,
        cache_checksum: &str,
        _cache_url: Option<&CacheUrl>,
    ) -> Result<Option<cache::Guard>, ExecutorError> {
        if self.dry || t.sources.is_empty() || t.generates.is_empty() {
            return Ok(None);
        }
        let lock_name = if cache_checksum.is_empty() {
            t.local_name()
        } else {
            format!("{}:{}", t.local_name(), cache_checksum)
        };

        let file_locker = CacheLock::File {
            dir: std::path::Path::new(&self.temp_dir.fingerprint).join("locks"),
            timeout: None,
        };

        // The contention callback would need a logger borrow across an await;
        // kept quiet to avoid a borrow conflict (a known minor gap vs Go).
        let guard = match self.cache_lock(t) {
            // A remote (redis) lock: if it cannot be acquired — e.g. Redis is
            // unreachable — fall back to the local file lock so a Redis outage
            // degrades to local locking instead of failing the build (Go does
            // the same).
            Some(remote) => match remote.lock(&lock_name, || {}).await {
                Ok(guard) => guard,
                Err(e) => {
                    self.logger().borrow_mut().verbose_errf(
                        Color::Yellow,
                        &format!(
                            "task: remote lock failed for {:?}: {e} (falling back to local)\n",
                            t.name()
                        ),
                    );
                    file_locker.lock(&lock_name, || {}).await?
                }
            },
            None => file_locker.lock(&lock_name, || {}).await?,
        };
        Ok(Some(guard))
    }

    /// Parses the task's resolved `cache.lock` into a distributed locker. Ports
    /// Go `evalCacheLocker`. A disabled cache has no locker — the lock only
    /// guards cache operations, so it must not be evaluated (or connected to)
    /// when the cache is off.
    fn cache_lock(&self, t: &Task) -> Option<CacheLock> {
        if !cache_enabled(t) {
            return None;
        }
        let c = t.cache.as_ref()?;
        if c.lock.trim().is_empty() {
            return None;
        }
        let timeout = if c.lock_timeout.is_empty() {
            None
        } else {
            crate::goext::parse_duration(&c.lock_timeout).ok()
        };
        // An unusable lock URL — an unreadable `?ca=` file, an unknown scheme
        // — must not read as "no lock configured": the run degrades to the
        // local file lock, so say why.
        match CacheLock::from_url(&c.lock, timeout, &c.api_key) {
            Ok(locker) => locker,
            Err(e) => {
                // A lock URL carries credentials (`vk://user:pass@host/...`).
                let shown = cache::CacheUri::parse(c.lock.trim())
                    .map_or_else(|| c.lock.trim().to_string(), |u| u.redacted());
                self.logger().borrow_mut().verbose_errf(
                    Color::Yellow,
                    &format!("task: cache lock {shown:?} unusable: {e} (falling back to local)\n"),
                );
                None
            }
        }
    }

    /// Validates cache metadata against the task's current state and records the
    /// fingerprint as up to date on success. The task annotation is the local
    /// name the entry is keyed by, so a copy reached through another include
    /// verifies against it. Ports Go `cacheVerifyMeta`.
    fn cache_verify_meta(
        &self,
        t: &Task,
        checker: &mut ChecksumChecker,
        meta: &cache::CacheMeta,
    ) -> Result<(), ExecutorError> {
        let local_name = t.local_name();
        if !meta.task.is_empty() && meta.task != local_name {
            return Err(ExecutorError::Cache(Box::new(cache::CacheError::msg(
                format!(
                    "task name mismatch: cached {:?}, expected {local_name:?}",
                    meta.task
                ),
            ))));
        }
        let source_value = checker.source_value().to_string();
        if !meta.sources.is_empty() && meta.sources != source_value {
            return Err(ExecutorError::Cache(Box::new(cache::CacheError::msg(
                format!(
                    "sources checksum mismatch: cached {}, got {}",
                    meta.sources, source_value
                ),
            ))));
        }
        let current = checker.generates_checksum()?;
        if current != meta.generates {
            return Err(ExecutorError::Cache(Box::new(cache::CacheError::msg(
                format!(
                    "generates checksum mismatch: cached {}, got {current}",
                    meta.generates
                ),
            ))));
        }
        checker.set_up_to_date()?;
        Ok(())
    }
}

/// Lets `variables::compiled_task` resolve `from:` and `task:` entries: finds
/// the tasks they reach and compiles them, without resolving theirs, so
/// compiles never recurse ([`crate::reach`] walks them).
struct GlobResolver<'a> {
    exec: &'a Executor,
    /// The call being compiled.
    call: &'a Call,
}

impl crate::variables::TaskResolver for GlobResolver<'_> {
    fn resolve(&self, name: &str) -> Result<crate::variables::ResolvedCall, CompileError> {
        let call = Call {
            task: name.to_string(),
            ..Default::default()
        };
        match self.exec.find_matching_tasks(&call) {
            Ok(matching) => match matching.first() {
                Some(m) => Ok(crate::variables::ResolvedCall {
                    task: m.task.task.clone(),
                    captures: m.wildcards.clone(),
                    platforms: m.task.platforms.clone(),
                }),
                None => Err(CompileError::TaskNotFound {
                    name: name.to_string(),
                }),
            },
            Err(e) => Err(to_compile_error(e)),
        }
    }

    fn compile_call<'a>(
        &'a self,
        name: &'a str,
        vars: &'a crate::ast::Vars,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::variables::ReachedTask, CompileError>>
                + 'a,
        >,
    > {
        Box::pin(async move {
            let call = Call {
                task: name.to_string(),
                vars: vars.clone(),
                silent: false,
                indirect: true,
            };
            let raw = self.exec.get_task(&call).map_err(to_compile_error)?;
            let task = self
                .exec
                .compiled_task_with(&call, true, None)
                .await
                .map_err(to_compile_error)?;
            Ok(crate::variables::ReachedTask { task, raw })
        })
    }

    fn root(&self) -> Option<(&str, &crate::ast::Vars)> {
        Some((&self.call.task, &self.call.vars))
    }
}

/// An error finding or compiling a task a reference reaches, kept typed where
/// a run would exit with its own code.
fn to_compile_error(e: ExecutorError) -> CompileError {
    match e {
        // So the referrer can name itself in the message, and the process
        // exits with the task-not-found code.
        ExecutorError::TaskNotFound { task_name, .. } => {
            CompileError::TaskNotFound { name: task_name }
        }
        // Named from the referrer by the walk.
        ExecutorError::TaskNameConflict { call, task_names } => CompileError::TaskNameConflict {
            task: String::new(),
            field: String::new(),
            via: Vec::new(),
            call,
            task_names,
        },
        ExecutorError::Compile(e) => *e,
        other => CompileError::FromTask(other.to_string()),
    }
}

/// Reports whether the cache block is active for a task. Ports Go
/// `cacheEnabled`.
fn cache_enabled(t: &Task) -> bool {
    let Some(c) = &t.cache else {
        return false;
    };
    if let Some(enabled) = c.enabled {
        return enabled;
    }
    if !c.if_.is_empty() {
        let v = c.if_.trim();
        return !v.is_empty() && v != "false" && v != "0";
    }
    true
}

/// Fails a command the sandbox denied something, whatever its exit status:
/// it may have worked around the denial (`cat secret || true`), and a failure
/// must not be ignored (`ignore_error`), as its outputs cannot be trusted to
/// depend only on the declared sources.
fn sandbox_verdict(t: &Task, denied: bool) -> Result<(), ExecutorError> {
    if denied {
        return Err(ExecutorError::SandboxDenied {
            task_name: t.name().to_string(),
            denied: Denied::Command,
        });
    }
    Ok(())
}

/// Whether a command ran and failed, the only failure a sandbox denial can
/// explain (not a shell syntax error, say).
fn exited_nonzero(result: &Result<(), execext::Error>) -> bool {
    matches!(result, Err(execext::Error::NonZeroExit(_)))
}

/// The `TMPDIR` of a sandboxed task's run, removed with what its commands left
/// there once they are done (or dropped).
struct SandboxTmp(Option<std::path::PathBuf>);

impl Drop for SandboxTmp {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            crate::sandbox::remove_tmp(dir);
        }
    }
}

/// The files a `sources`-style list lets a sandboxed command read: everything
/// its globs match, the files a `fingerprint` stands for included, plus the
/// fingerprints themselves.
fn declared_files(dir: &str, globs: &[crate::ast::Glob]) -> std::io::Result<Vec<String>> {
    let globs: Vec<_> = globs
        .iter()
        .filter(|g| !g.glob.starts_with("value:"))
        .cloned()
        .collect();
    let whole: Vec<_> = globs
        .iter()
        .map(|g| crate::ast::Glob {
            fingerprint: String::new(),
            ..g.clone()
        })
        .collect();
    let mut files = fingerprint::globs(dir, &whole)?;
    // The fingerprints, with the exclusions that may apply to them.
    let fingerprints: Vec<_> = globs
        .into_iter()
        .filter(|g| g.negate || !g.fingerprint.is_empty())
        .collect();
    if fingerprints.iter().any(|g| !g.negate) {
        files.extend(fingerprint::globs(dir, &fingerprints)?);
    }
    Ok(files)
}

/// The directory a `generates` pattern writes in: the part before its first
/// wildcard segment, or the parent of a plain file path.
fn generated_dir(pattern: &std::path::Path) -> std::path::PathBuf {
    let mut dir = std::path::PathBuf::new();
    for part in pattern.components() {
        if part
            .as_os_str()
            .to_string_lossy()
            .contains(['*', '?', '[', '{'])
        {
            return dir;
        }
        dir.push(part);
    }
    dir.parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or(dir)
}

/// Converts the `KEY=VALUE` list from [`env::get`] into `(name, value)` pairs.
fn split_env(list: Option<Vec<String>>) -> Vec<(String, String)> {
    let Some(list) = list else {
        return Vec::new();
    };
    list.into_iter()
        .map(|e| match e.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (e, String::new()),
        })
        .collect()
}

/// Unwraps an `Arc<ExecutorError>` into an owned error, cloning if shared.
fn unwrap_arc(err: Arc<ExecutorError>) -> ExecutorError {
    match Arc::try_unwrap(err) {
        Ok(e) => e,
        Err(shared) => ExecutorError::Io(shared.to_string()),
    }
}

/// Joins a set of queued tasks. With failfast, returns as soon as one errors,
/// aborting the rest and waiting for those aborts to run — for the siblings
/// themselves; their own subtrees are cancelled transitively, and only
/// [`Executor::drain_queue`] waits for those. Otherwise waits for all of them
/// and returns the first error. Ports the `errgroup` used by Go
/// `runDeps`/`Run`.
async fn join_queued(
    executor: &Executor,
    tasks: Vec<QueuedTask>,
    failfast: bool,
) -> Result<(), ExecutorError> {
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::task::Poll;

    let mut pending = tasks;
    if pending.is_empty() {
        return Ok(());
    }

    let mut first_err: Option<ExecutorError> = None;
    let mut fatal: Option<ExecutorError> = None;
    poll_fn(|cx| {
        let mut i = 0;
        while i < pending.len() {
            let Some(task) = pending.get_mut(i) else {
                break;
            };
            // Always `Some` here: a handle is only taken below, after which
            // the task has already left `pending`.
            let Some(handle) = task.handle.as_mut() else {
                debug_assert!(false, "a queued task lost its handle while pending");
                drop(pending.remove(i));
                continue;
            };
            match Pin::new(handle).poll(cx) {
                Poll::Ready(res) => {
                    // Dropped with its handle: aborting a task that has already
                    // finished is a no-op.
                    drop(pending.remove(i));
                    if let Err(e) = queued_result(res) {
                        if failfast {
                            fatal = Some(e);
                            return Poll::Ready(());
                        }
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    continue;
                }
                Poll::Pending => {
                    i = i.saturating_add(1);
                }
            }
        }
        if pending.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    if let Some(e) = fatal {
        // These siblings are abandoned part-way, so their commands can outlive
        // them; the drain sweeps for those once the run is over.
        if !pending.is_empty() {
            executor.queue.abandoned.set(true);
        }
        // Cancel the siblings and wait for it: aborting only schedules the
        // cancellation, and returning before it runs would let them start
        // further commands, which dropping an inline future never did. A
        // command already handed to the shell still runs to completion. Their
        // own subtrees are cancelled with them but drained by `drain_queue`,
        // not here.
        for task in &pending {
            if let Some(handle) = task.handle.as_ref() {
                handle.abort();
            }
        }
        for mut task in pending {
            if let Some(handle) = task.handle.as_mut() {
                // A cancelled sibling is expected, and its error is discarded in
                // favor of the one that cancelled it. A *panicking* sibling is a
                // bug, and must not be hidden behind that error.
                if let Err(join_err) = handle.await
                    && !join_err.is_cancelled()
                {
                    std::panic::resume_unwind(join_err.into_panic());
                }
            }
            task.handle = None;
        }
        return Err(e);
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A thread-safe, growable byte buffer used as a [`Stdio::Capture`] sink. The
/// shell's capture drain (a helper thread) writes into it; the current thread
/// drains it afterward via [`SharedBytes::take`].
#[derive(Clone, Default)]
struct SharedBytes(Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedBytes {
    /// Removes and returns the accumulated bytes.
    fn take(&self) -> Vec<u8> {
        match self.0.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl std::io::Write for SharedBytes {
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

#[cfg(test)]
mod tests {
    use super::CallPath;

    /// A level whose key and reported name are the same, for the cases where
    /// the digest half is not what is under test.
    fn level(path: &super::Ancestors, task: &str) -> std::rc::Rc<CallPath> {
        CallPath::extend(path, task, task)
    }

    #[test]
    fn detects_only_tasks_on_this_path() {
        let root = Some(level(&None, "a"));
        let branch = Some(level(&root, "b"));
        let sibling = level(&root, "c");
        let deep = level(&branch, "d");

        assert!(deep.contains("a"), "an ancestor is on the path");
        assert!(deep.contains("b"));
        assert!(deep.contains("d"), "the task itself closes a self-cycle");
        // `c` runs beside `b`, not above `d`: a task reached twice on separate
        // paths is a diamond, not a cycle.
        assert!(!deep.contains("c"));
        assert!(!sibling.contains("b"));
    }

    #[test]
    fn reports_the_cycle_outermost_first() {
        // Three deep, so the order is observable: a two-task cycle reads the
        // same in both directions and would pass without any reversal.
        let a = Some(level(&None, "a"));
        let b = Some(level(&a, "b"));
        let path = level(&b, "c");
        assert_eq!(path.to_path_through("a"), ["a", "b", "c", "a"]);
    }

    // A level matches on the key but reports the name, so the same task name
    // entered with a different compiled body is not a repeat — and the cycle
    // still reads in names when one is.
    #[test]
    fn a_different_body_under_the_same_name_is_not_a_repeat() {
        let first = Some(CallPath::extend(&None, "count\0aaa", "count"));
        let second = CallPath::extend(&first, "count\0bbb", "count");

        assert!(!second.contains("count\0ccc"), "a third turn progresses");
        assert!(second.contains("count\0aaa"), "the same body repeats");
        // The digests never reach the reported path.
        assert_eq!(second.to_path_through("count"), ["count", "count", "count"]);
    }
}
