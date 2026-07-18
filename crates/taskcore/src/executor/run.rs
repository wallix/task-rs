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

impl Executor {
    /// Runs the given calls. Existence and internal checks run first, then a
    /// dry summary if requested, otherwise the calls are executed (in parallel
    /// when `parallel` is set). Ports Go `Run`.
    pub async fn run(self: &Rc<Self>, calls: &[Call]) -> Result<(), ExecutorError> {
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
        let mut futures = Vec::with_capacity(calls.len());
        for call in calls {
            futures.push(self.run_task_boxed(call.clone()));
        }
        join_all_failfast(futures, self.failfast).await
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
            Some(self as &dyn crate::variables::TaskResolver),
        )
        .await;
        self.flush_scratch(&sink);
        Ok(result?)
    }

    /// Runs a task as a boxed future. Recursive call sites (deps, setup, cmd
    /// subtasks) use this to break the `async fn` recursion cycle, which the
    /// compiler cannot size otherwise.
    pub(crate) fn run_task_boxed(
        self: &Rc<Self>,
        call: Call,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ExecutorError>> + '_>> {
        let this = Rc::clone(self);
        Box::pin(async move { this.run_task(call).await })
    }

    /// Runs a task by resolving it, checking preconditions/fingerprint/cache,
    /// running its dependencies, and executing its commands. Ports Go `RunTask`.
    pub async fn run_task(self: &Rc<Self>, mut call: Call) -> Result<(), ExecutorError> {
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

        // Guard against cyclic dependencies via a per-task call counter.
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

        let result = self.start_execution(&t, &call).await;
        result.map_err(|source| ExecutorError::TaskRun {
            task_name: t.name().to_string(),
            source: Box::new(unwrap_arc(source)),
        })
    }

    /// Deduplicates concurrent executions of the same task hash: the first
    /// caller runs the task while later callers await its result. Ports Go
    /// `startExecution` + its `sync.Once`-style execution-hash map.
    async fn start_execution(self: &Rc<Self>, t: &Task, call: &Call) -> RunOnceResult {
        let h = self.task_hash(t);
        if h.is_empty() || t.watch {
            return self.execute(t, call).await.map_err(Arc::new);
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
            .get_or_init(|| async { self.execute(t, call).await.map_err(Arc::new) })
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
    async fn execute(self: &Rc<Self>, t: &Task, call: &Call) -> Result<(), ExecutorError> {
        self.logger()
            .borrow_mut()
            .verbose_errf(Color::Magenta, &format!("task: {:?} started\n", call.task));

        self.run_setup(t).await?;

        let mut checker = ChecksumChecker::new(&self.temp_dir.fingerprint, t.clone());
        let source_hash = t.source_hash.clone();

        let cache_url = self.cache_url(t);
        let cache_active = cache_url.is_some();

        // Acquire the build-once lock covering deps, fingerprint, execution and
        // the up-to-date write, for tasks with fingerprint state.
        let _lock = self
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

        self.run_deps(t).await?;

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
            if let Err(e) = self.run_command(t, call, i).await {
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
            self.run_deferred(t, call, i, deferred_exit_code).await;
        }

        if let Some(e) = run_err {
            return Err(e);
        }

        if !self.dry {
            let changed = checker.sources_changed()?;
            if changed {
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

    /// Runs setup tasks sequentially and unconditionally, releasing the
    /// concurrency slot while they run. Ports Go `runSetup`.
    async fn run_setup(self: &Rc<Self>, t: &Task) -> Result<(), ExecutorError> {
        for d in &t.setup {
            let call = Call {
                task: d.task.clone(),
                vars: d.vars.clone().unwrap_or_default(),
                silent: d.silent,
                indirect: true,
            };
            self.run_task_boxed(call).await?;
        }
        Ok(())
    }

    /// Runs a task's dependencies concurrently, honoring failfast. Ports Go
    /// `runDeps`.
    async fn run_deps(self: &Rc<Self>, t: &Task) -> Result<(), ExecutorError> {
        let mut futures = Vec::with_capacity(t.deps.len());
        for d in &t.deps {
            let call = Call {
                task: d.task.clone(),
                vars: d.vars.clone().unwrap_or_default(),
                silent: d.silent,
                indirect: true,
            };
            futures.push(self.run_task_boxed(call));
        }
        join_all_failfast(futures, self.failfast || t.failfast).await
    }

    /// Runs a single deferred command. Deferred commands are left un-templated
    /// during compilation so they can be rendered here against the task's
    /// variables plus `EXIT_CODE` (the failing command's exit status). Errors are
    /// ignored. Ports Go `runDeferred`.
    async fn run_deferred(self: &Rc<Self>, t: &Task, call: &Call, i: usize, exit_code: u8) {
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
        if let Err(e) = self.run_command(&task, call, i).await {
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
            let result = self.run_task_boxed(sub).await;
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

/// Reports whether the cache block is active for a task. Ports Go
/// `cacheEnabled`.
/// Lets `variables::compiled_task` expand `from: deps`/`from: cmds` globs by
/// recursively compiling the referenced tasks. Async because compilation may
/// evaluate dynamic variables; the recursion terminates on the dependency DAG.
impl crate::variables::TaskResolver for Executor {
    fn compiled_task_for_globs<'a>(
        &'a self,
        task: &'a str,
        vars: &'a crate::ast::Vars,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Task, crate::variables::CompileError>> + 'a>,
    > {
        Box::pin(async move {
            let call = Call {
                task: task.to_string(),
                vars: vars.clone(),
                silent: false,
                indirect: true,
            };
            self.compiled_task(&call)
                .await
                .map_err(|e| crate::variables::CompileError::FromTask(e.to_string()))
        })
    }
}

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

/// Drives a set of `!Send` futures concurrently on the current-thread runtime.
/// With failfast, returns as soon as one errors (dropping the rest); otherwise
/// runs all and returns the first error. Ports the `errgroup` used by Go
/// `runDeps`/`Run`.
async fn join_all_failfast<F>(futures: Vec<F>, failfast: bool) -> Result<(), ExecutorError>
where
    F: std::future::Future<Output = Result<(), ExecutorError>>,
{
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::task::Poll;

    let mut pending: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    if pending.is_empty() {
        return Ok(());
    }

    let mut first_err: Option<ExecutorError> = None;
    poll_fn(|cx| {
        let mut i = 0;
        while i < pending.len() {
            if let Some(fut) = pending.get_mut(i) {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        pending.remove(i);
                        if let Err(e) = res {
                            if failfast {
                                return Poll::Ready(Err(e));
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
            } else {
                break;
            }
        }
        if pending.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await?;

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
