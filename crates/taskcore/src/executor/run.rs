//! Task resolution and the run loop: match calls to tasks, compile them,
//! short-circuit on fingerprint/cache, run dependencies concurrently, and
//! execute commands. Ports Go `task.go`.

use std::rc::Rc;
use std::sync::Arc;

use serde_yaml_ng::Value;
use tokio::sync::OnceCell;

use crate::ast::{Task, Var};
use crate::cache::{self, CacheLock, CacheUrl};
use crate::call::Call;
use crate::env;
use crate::execext::{self, RunCommandOptions, Stdio};
use crate::fingerprint::ChecksumChecker;
use crate::logger::Color;
use crate::output::{Output, SharedWriter};
use crate::precondition;
use crate::requires;
use crate::slicesext;
use crate::templater::Cache as TemplaterCache;
use crate::variables::{self, CompileContext};

use super::{
    Executor, ExecutorError, MAXIMUM_TASK_CALL, RunOnceResult, should_run_on_current_platform,
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
}

impl CallPath {
    /// The path with a level appended, for handing to that task's own callees.
    fn extend(path: &Ancestors, key: &str, name: &str) -> Rc<Self> {
        Rc::new(Self {
            key: key.to_string(),
            name: name.to_string(),
            parent: path.clone(),
        })
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
    pub async fn run(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
        let result = self.run_calls(calls).await;
        // However the run ended, nothing queued may outlive it.
        self.drain_queue().await;
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
        join_queued(tasks, self.failfast).await
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
        let resolver = GlobResolver {
            exec: self,
            // Seeded with this task, so a cycle reads from the task whose globs
            // started the expansion and a task whose `from:` reaches itself is
            // caught on the first hop.
            above: Some(CallPath::extend(&None, &call.task, &call.task)),
        };
        self.compiled_task_with(call, evaluate_sh_vars, &resolver)
            .await
    }

    /// The body of [`Self::compiled_task_inner`], taking the resolver that
    /// expands `from:` globs so a nested expansion can hand down its own path.
    async fn compiled_task_with(
        &self,
        call: &Call,
        evaluate_sh_vars: bool,
        resolver: &dyn crate::variables::TaskResolver,
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
        let result = variables::compiled_task(
            &orig,
            vars,
            evaluate_sh_vars,
            &ctx,
            &compiler,
            &mut scratch,
            Some(resolver),
        )
        .await;
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

    /// Cancels everything still queued and waits for those cancellations to
    /// run, so every abandoned task has released its lock and permit and will
    /// start no further command by the time the run returns. A process the
    /// shell has already started keeps running — nothing here kills it.
    pub(crate) async fn drain_queue(&self) {
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
        if self.prompt_task_vars(&t, &mut call)? {
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

    /// The core task execution body run once per dedup hash: fingerprint/cache
    /// short-circuit, deps, then commands. Ports the closure passed to Go
    /// `startExecution`.
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

        let mut checker = ChecksumChecker::new(&self.temp_dir.fingerprint, t.clone());
        let source_hash = t.source_hash.clone();

        let cache_url = self.cache_url(t);
        let cache_active = cache_url.is_some();

        // Acquire the build-once lock covering deps, fingerprint, execution and
        // the up-to-date write, for tasks with fingerprint state.
        let lock = self
            .acquire_task_lock(t, &source_hash, cache_url.as_ref())
            .await?;

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
                && let Some(url) = &cache_url
            {
                {
                    let (ok, meta) = {
                        let (mut scratch, sink) = self.scratch_logger();
                        let r = cache::cache_restore(
                            url,
                            t.name(),
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
                self.confirm_or_cancel(p, &call.task)?;
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
            self.run_deferred(t, call, i, deferred_exit_code, &ancestors)
                .await;
        }

        // A remote lock lost mid-run means a peer may have been running this
        // same task concurrently, so this run's fingerprint and cache entry
        // cannot be trusted as the build-once result. Read here, before the
        // early return and before anything that can fail, so the warning also
        // reaches an operator whose task errored out.
        let lock_lost = lock.as_ref().is_some_and(cache::Guard::is_lost);
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
                self.logger().borrow_mut().verbose_errf(
                    Color::Yellow,
                    &format!(
                        "task: sources changed during execution of {:?}, skipping fingerprint and cache update\n",
                        t.name()
                    ),
                );
                let _ = checker.on_error();
            } else {
                checker.set_up_to_date()?;
                if cache_active
                    && !source_hash.is_empty()
                    && let Some(url) = &cache_url
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
        join_queued(tasks, self.failfast || t.failfast).await
    }

    /// Runs a single deferred command. Deferred commands are left un-templated
    /// during compilation so they can be rendered here against the task's
    /// variables plus `EXIT_CODE` (the failing command's exit status). Errors are
    /// ignored. Ports Go `runDeferred`.
    async fn run_deferred(
        self: &Rc<Self>,
        t: &Task,
        call: &Call,
        i: usize,
        exit_code: u8,
        ancestors: &Ancestors,
    ) {
        let Some(cmd) = t.cmds.get(i) else {
            return;
        };
        let mut cache = TemplaterCache::new(t.vars.clone().unwrap_or_default());
        cache.set_dialect(t.dialect);
        let mut extra: indexmap::IndexMap<String, serde_yaml_ng::Value> = indexmap::IndexMap::new();
        if exit_code > 0 {
            extra.insert(
                "EXIT_CODE".to_string(),
                serde_yaml_ng::Value::String(exit_code.to_string()),
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
        if let Err(e) = self.run_command(&task, call, i, ancestors).await {
            self.logger().borrow_mut().verbose_errf(
                Color::Yellow,
                &format!("task: ignored error in deferred cmd: {e}\n"),
            );
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
            let opts = RunCommandOptions {
                command: cmd.if_.clone(),
                dir: Some(t.compute_dir()),
                env: split_env(env::get(t, self.env_precedence)),
                posix_opts: Vec::new(),
                bash_opts: Vec::new(),
                stdout: Stdio::Inherit,
                stderr: Stdio::Inherit,
            };
            if execext::run_command(opts).await.is_err() {
                self.logger().borrow_mut().verbose_outf(
                    Color::Yellow,
                    &format!("task: [{}] if condition not met - skipped\n", t.name()),
                );
                return Ok(());
            }
        }

        if !cmd.task.is_empty() {
            let sub = Call {
                task: cmd.task.clone(),
                vars: cmd.vars.clone().unwrap_or_default(),
                silent: cmd.silent,
                indirect: true,
            };
            let result = self.spawn_task(sub, ancestors.clone()).join().await;
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

            let result = self.exec_shell(t, call, &cmd.cmd, posix, bash).await;
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
    async fn exec_shell(
        &self,
        t: &Task,
        call: &Call,
        command: &str,
        posix: Vec<String>,
        bash: Vec<String>,
    ) -> Result<(), ExecutorError> {
        let env = split_env(env::get(t, self.env_precedence));

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
            };
            let _permit = self.limiter.acquire().await;
            let run_result = execext::run_command(opts).await;
            drop(_permit);
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
        };

        // Bound concurrent command execution by the configured limit.
        let _permit = self.limiter.acquire().await;
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
        run_result.map_err(ExecutorError::Exec)
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
    fn confirm_or_cancel(&self, message: &str, _task: &str) -> Result<(), ExecutorError> {
        if let Some(prompter) = &self.prompter {
            match prompter.confirm(message) {
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
        let url = t.cache.as_ref().map(|c| c.url.as_str()).unwrap_or("");
        CacheUrl::parse(url).unwrap_or_default()
    }

    /// Acquires the build-once lock for a task with both sources and generates,
    /// covering deps, the fingerprint check, execution, and the up-to-date
    /// write. Uses the configured `cache.lock` when set, otherwise a local
    /// filesystem lock under `<temp>/locks`, so concurrent invocations of the
    /// same fingerprinted task serialize. Ports the locking block of Go
    /// `RunTask` (`e.Locker` = a flock).
    async fn acquire_task_lock(
        &self,
        t: &Task,
        source_hash: &str,
        _cache_url: Option<&CacheUrl>,
    ) -> Result<Option<cache::Guard>, ExecutorError> {
        if self.dry || t.sources.is_empty() || t.generates.is_empty() {
            return Ok(None);
        }
        let lock_name = if source_hash.is_empty() {
            t.name().to_string()
        } else {
            format!("{}:{}", t.name(), source_hash)
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
        CacheLock::from_url(&c.lock, timeout).ok().flatten()
    }

    /// Validates cache metadata against the task's current state and records the
    /// fingerprint as up to date on success. Ports Go `cacheVerifyMeta`.
    fn cache_verify_meta(
        &self,
        t: &Task,
        checker: &mut ChecksumChecker,
        meta: &cache::CacheMeta,
    ) -> Result<(), ExecutorError> {
        if !meta.task.is_empty() && meta.task != t.name() {
            return Err(ExecutorError::Cache(Box::new(cache::CacheError::msg(
                format!(
                    "task name mismatch: cached {:?}, expected {:?}",
                    meta.task,
                    t.name()
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

/// Lets `variables::compiled_task` expand `from: deps`/`from: cmds` globs by
/// recursively compiling the referenced tasks. Async because compilation may
/// evaluate dynamic variables.
///
/// This recursion runs *during* compilation, before `run_task_on` can consult
/// the call path, so it needs its own guard: two tasks that list each other
/// under `deps:` and both take `sources: [{from: deps}]` would otherwise
/// compile each other until the stack overflowed.
///
/// The guard is the path carried by this value, not state on the executor.
/// Compiling a dynamic (`sh:`) variable yields, so two tasks expanding globs
/// over a shared dep interleave; a single stack on the executor would see the
/// other's entry and report a cycle that does not exist, pop the wrong entry,
/// and keep a stale one when `join_all_failfast` drops a suspended sibling.
struct GlobResolver<'a> {
    exec: &'a Executor,
    /// The tasks being compiled above this one, innermost first. Keyed by name
    /// alone: a repeat here is a task whose globs need its own globs.
    above: Ancestors,
}

impl crate::variables::TaskResolver for GlobResolver<'_> {
    fn compiled_task_for_globs<'a>(
        &'a self,
        task: &'a str,
        vars: &'a crate::ast::Vars,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Task, crate::variables::CompileError>> + 'a>,
    > {
        Box::pin(async move {
            if let Some(above) = &self.above
                && above.contains(task)
            {
                // Reported from the task whose globs started the expansion, the
                // same convention the call-path guard uses.
                return Err(crate::variables::CompileError::Cycle {
                    path: above.to_path_through(task),
                });
            }
            let call = Call {
                task: task.to_string(),
                vars: vars.clone(),
                silent: false,
                indirect: true,
            };
            let inner = GlobResolver {
                exec: self.exec,
                above: Some(CallPath::extend(&self.above, task, task)),
            };
            self.exec
                .compiled_task_with(&call, true, &inner)
                .await
                .map_err(|e| match e {
                    // Keep a cycle typed as it unwinds through the outer frames,
                    // so it still reaches the process as a cyclic-dependency
                    // error rather than as an opaque compile failure.
                    ExecutorError::CyclicDependency { path } => {
                        crate::variables::CompileError::Cycle { path }
                    }
                    other => crate::variables::CompileError::FromTask(other.to_string()),
                })
        })
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
async fn join_queued(tasks: Vec<QueuedTask>, failfast: bool) -> Result<(), ExecutorError> {
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
