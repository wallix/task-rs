//! Compilation of a task into its fully-templated form.
//!
//! [`compiled_task`] takes a raw task and the variables resolved for it and
//! produces a copy with every templatable field (label, cmds, deps, sources,
//! generates, env, preconditions, cache, …) interpolated. It also evaluates the
//! task's dynamic env vars, reads `dotenv` files, computes the source checksum
//! (`CHECKSUM`), and expands `for` loops over cmds and deps.
//!
//! Ports the templating half of Go `variables.go`. The `from:` and `task:`
//! entries of `sources`, `generates` and `sandbox.writes` reach *other* tasks
//! (Executor territory): they are resolved through the optional
//! [`TaskResolver`] (see [`crate::reach`]); when none is provided that step is
//! skipped.

use std::future::Future;
use std::pin::Pin;

use indexmap::IndexMap;

use crate::ast::{
    Caches, Cmd, Dep, Dialect, For, Glob, Location, Matrix, PendingCmd, Precondition, Task, Var,
    Vars,
};
use crate::compiler::{Compiler, CompilerError};
use crate::env;
use crate::execext;
use crate::filepathext;
use crate::fingerprint;
use crate::logger::Logger;
use crate::templater::{Cache, TemplaterError};
use crate::yaml::Value;

/// Finds and compiles the tasks `from:` and `task:` entries reach. The
/// executor's `GlobResolver` implements this; `compiled_task` calls it to
/// resolve those entries (see [`crate::reach`]).
pub trait TaskResolver {
    /// The task a call name runs; a [`CompileError::TaskNotFound`] when there
    /// is none.
    fn resolve(&self, name: &str) -> Result<ResolvedCall, CompileError>;

    /// Compiles a call as running it would, up to its `setup` and `deps`,
    /// leaving its references unresolved ([`reached_task`]). Async, since
    /// compilation may evaluate dynamic (`sh:`) variables; the returned
    /// future is boxed so the trait stays object-safe.
    fn compile_call<'a>(
        &'a self,
        name: &'a str,
        vars: &'a Vars,
    ) -> Pin<Box<dyn Future<Output = Result<ReachedTask, CompileError>> + 'a>>;

    /// The call the task being compiled was made with, when known: a
    /// reference reaching that call again adds only what it runs.
    fn root(&self) -> Option<(&str, &Vars)>;
}

/// The task a call name resolves to, before it is compiled.
#[derive(Clone, Debug)]
pub struct ResolvedCall {
    /// The task's key.
    pub task: String,
    /// The wildcard captures of the name.
    pub captures: Vec<String>,
    /// The task's `platforms:`, which a run checks before compiling it.
    pub platforms: Vec<crate::ast::Platform>,
}

/// A task compiled for a reference to reach it.
#[derive(Clone, Debug)]
pub struct ReachedTask {
    /// Compiled by [`reached_task`].
    pub task: Task,
    /// As declared, so that a `task:` entry whose name rendered empty is told
    /// from a glob.
    pub raw: Task,
}

/// An error raised while compiling a task.
#[derive(Debug)]
pub enum CompileError {
    /// A templater error surfaced during field interpolation.
    Template(TemplaterError),
    /// A dynamic env var (or other compiler operation) failed.
    Compiler(Box<CompilerError>),
    /// A `dotenv` file could not be read.
    Dotenv {
        /// The path that failed.
        path: String,
        /// The underlying I/O error message.
        message: String,
    },
    /// A `for.matrix` reference did not resolve to a list.
    MatrixRef(String),
    /// An unsupported `from:` value was used in sources/generates.
    UnsupportedFrom {
        /// The task being compiled.
        task: String,
        /// The field (`sources` or `generates`).
        field: String,
        /// The offending `from:` value.
        from: String,
    },
    /// Glob expansion for a `for.from` clause failed.
    Glob(String),
    /// Compiling a task referenced by a `from:` clause failed.
    FromTask(String),
    /// The resolver found no task by the name a `from:` or `task:` entry
    /// references. [`crate::reach`] replaces it with
    /// [`Self::ReferencedTaskNotFound`], which names the referrer.
    TaskNotFound {
        /// The referenced task name.
        name: String,
    },
    /// A `from:` or `task:` entry in sources/generates/sandbox.writes reaches
    /// a task that does not exist.
    ReferencedTaskNotFound {
        /// The task whose entry it is.
        task: String,
        /// The field (`sources`, `generates` or `sandbox.writes`).
        field: String,
        /// The missing task name.
        name: String,
        /// How the entry reached it, outermost first, the missing call last;
        /// one step or none for the entry itself.
        via: Vec<String>,
    },
    /// A reference reaches more calls than [`crate::reach`] follows.
    ReferenceLimit(Box<ReferenceLimit>),
    /// A deferred `task:` call a reference reaches depends on what only the
    /// run knows (`EXIT_CODE`), or on a `CHECKSUM` covering the reached
    /// task's own references.
    DeferredCallUnsupported(Box<DeferredCall>),
    /// A name a reference reaches matches the aliases of several tasks.
    TaskNameConflict {
        /// The task whose entry it is.
        task: String,
        /// The field.
        field: String,
        /// How the entry reached the name, outermost first, it last; one step
        /// for the entry itself.
        via: Vec<String>,
        /// The name.
        call: String,
        /// The tasks whose alias it is.
        task_names: Vec<String>,
    },
    /// A `task:` entry in sources/generates rendered to an empty name.
    EmptyTaskReference {
        /// The task whose entry it is.
        task: String,
        /// The field (`sources` or `generates`).
        field: String,
        /// The unrendered `task:` value.
        template: String,
    },
    /// A cache block names a `vk` registry and also spells out `url` or `lock`.
    CacheVkWithUrl {
        /// The task being compiled.
        task: String,
    },
    /// A cache block's `vk` rendered to something other than `host[:port]/repo`.
    CacheVkInvalid {
        /// The task being compiled.
        task: String,
        /// The rendered value.
        vk: String,
    },
    /// A cache block's entry key could not be derived from the task.
    CacheKey {
        /// The task being compiled.
        task: String,
        /// The underlying error message.
        message: String,
    },
    /// A cached task references a generates path outside the project root.
    CacheGeneratesOutsideRoot {
        /// The task being compiled.
        task: String,
        /// The offending generates glob.
        glob: String,
        /// The project root directory.
        root: String,
    },
}

/// The details of [`CompileError::ReferenceLimit`].
#[derive(Debug)]
pub struct ReferenceLimit {
    /// The task whose entry it is.
    pub task: String,
    /// The field.
    pub field: String,
    /// The entry (`{task: x}`).
    pub entry: String,
    /// The task reached too many times; `None` when the walk as a whole
    /// reached too many calls.
    pub name: Option<String>,
    /// The limit passed.
    pub limit: usize,
    /// The calls leading to the last one, innermost first.
    pub calls: Vec<String>,
}

/// The details of [`CompileError::DeferredCallUnsupported`].
#[derive(Debug)]
pub struct DeferredCall {
    /// The task whose entry it is.
    pub task: String,
    /// The field.
    pub field: String,
    /// How the entry reached the task holding the call, outermost first.
    pub via: Vec<String>,
    /// The task holding the call.
    pub owner: String,
    /// The call's unrendered task name.
    pub template: String,
    /// The variable it depends on.
    pub var: String,
}

impl CompileError {
    /// Names `task` in a refused `CHECKSUM` read (see
    /// [`TemplaterError::in_task`]).
    #[must_use]
    pub fn in_task(self, task: &str) -> Self {
        match self {
            Self::Template(err) => Self::Template(err.in_task(task)),
            Self::Compiler(err) => Self::Compiler(Box::new(err.in_task(task))),
            other => other,
        }
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Template(err) => write!(f, "{err}"),
            Self::Compiler(err) => write!(f, "{err}"),
            Self::Dotenv { path, message } => {
                write!(f, "task: failed to read dotenv {path:?}: {message}")
            }
            Self::MatrixRef(reference) => {
                write!(f, "matrix reference {reference:?} must resolve to a list")
            }
            Self::UnsupportedFrom { task, field, from } => write!(
                f,
                "task: {task}: {field}: unsupported from: {from:?} (expected \"deps\" or \"cmds\")"
            ),
            Self::Glob(message) => write!(f, "task: glob expansion failed: {message}"),
            Self::FromTask(message) => write!(f, "{message}"),
            Self::TaskNotFound { name } => write!(f, "task: Task {name:?} does not exist"),
            Self::ReferencedTaskNotFound {
                task,
                field,
                name,
                via,
            } => {
                if via.len() > 1 {
                    let via = via.join(" → ");
                    write!(
                        f,
                        "task: {task}: {field}: {via}: task {name:?} does not exist"
                    )
                } else {
                    write!(f, "task: {task}: {field}: task {name:?} does not exist")
                }
            }
            Self::ReferenceLimit(e) => {
                let ReferenceLimit {
                    task,
                    field,
                    entry,
                    name,
                    limit,
                    calls,
                } = &**e;
                let what = match name {
                    Some(name) => format!("more than {limit} calls of {name:?}"),
                    None => format!("more than {limit} task calls"),
                };
                // The innermost few and the outermost.
                let shown: Vec<&str> = match calls.split_last() {
                    Some((outer, inner)) if inner.len() > 3 => inner
                        .iter()
                        .take(3)
                        .map(String::as_str)
                        .chain(["…", outer.as_str()])
                        .collect(),
                    _ => calls.iter().map(String::as_str).collect(),
                };
                write!(
                    f,
                    "task: {task}: {field}: {entry}: {what} reached ({}): list the targets \
                     explicitly, or reference a task that does not recurse",
                    shown.join(" ← ")
                )
            }
            Self::DeferredCallUnsupported(e) => {
                let DeferredCall {
                    task,
                    field,
                    via,
                    owner,
                    template,
                    var,
                } = &**e;
                let via = if via.is_empty() {
                    String::new()
                } else {
                    format!("{}: ", via.join(" → "))
                };
                write!(
                    f,
                    "task: {task}: {field}: {via}{owner:?}'s deferred call {template:?} depends \
                     on {var}: list its targets with explicit `task:` entries"
                )
            }
            Self::TaskNameConflict {
                task,
                field,
                via,
                call,
                task_names,
            } => {
                let via = if via.len() > 1 {
                    format!("{}: ", via.join(" → "))
                } else {
                    String::new()
                };
                write!(
                    f,
                    "task: {task}: {field}: {via}multiple tasks ({}) match the alias {call:?}",
                    task_names.join(", ")
                )
            }
            Self::EmptyTaskReference {
                task,
                field,
                template,
            } => write!(
                f,
                "task: {task}: {field}: `task: {template}` rendered to an empty task name"
            ),
            Self::CacheVkWithUrl { task } => write!(
                f,
                "task: {task}: cache: `vk` derives the cache and lock URLs; drop `url` and `lock`"
            ),
            Self::CacheVkInvalid { task, vk } => write!(
                f,
                "task: {task}: cache: `vk` must be a bare host[:port]/repo, got {vk:?}"
            ),
            Self::CacheKey { task, message } => {
                write!(
                    f,
                    "task: {task}: cache: cannot derive the entry key: {message}"
                )
            }
            Self::CacheGeneratesOutsideRoot { task, glob, root } => write!(
                f,
                "task: {task}: generates path {glob:?} is outside project root {root:?}; caching requires all outputs to be within the project directory"
            ),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<CompilerError> for CompileError {
    fn from(err: CompilerError) -> Self {
        Self::Compiler(Box::new(err))
    }
}

/// The inputs `compiled_task` needs beyond the raw task and its variables.
///
/// These are the Executor fields the Go `compiledTask` reads directly. Bundling
/// them keeps the function signature focused rather than requiring a full
/// Executor.
pub struct CompileContext<'a> {
    /// The executor's root directory (prepended to the task's `dirs`).
    pub dir: &'a str,
    /// The taskfile-level environment variables (merged into the task env).
    pub taskfile_env: &'a Vars,
    /// Whether task env vars override the inherited process environment.
    pub env_precedence: bool,
    /// The taskfile-level named cache models (`caches:`), for `cache.inherit`.
    pub caches: &'a Caches,
}

/// Compiles `orig_task` against `vars`, returning the fully-templated task,
/// with `CHECKSUM` still [`PENDING_CHECKSUM`](crate::templater::PENDING_CHECKSUM):
/// [`finalize`] sets it once the run has hashed the sources.
///
/// When `evaluate_sh_vars` is true, dynamic env vars are resolved (running
/// shells via `compiler`); otherwise they are left for later. Ports Go
/// `compiledTask` (the field-templating body); the caller supplies `vars`
/// already resolved by [`Compiler::get_variables`].
pub async fn compiled_task(
    orig_task: &Task,
    vars: Vars,
    evaluate_sh_vars: bool,
    ctx: &CompileContext<'_>,
    compiler: &Compiler,
    logger: &mut Logger,
) -> Result<Task, CompileError> {
    let (new, cache) = compile_run(
        orig_task,
        vars,
        evaluate_sh_vars,
        ctx,
        compiler,
        logger,
        true,
    )
    .await?;
    // A `task:` name that renders empty would otherwise read as a plain glob
    // matching the task dir.
    if evaluate_sh_vars {
        check_task_references(&new, "sources", &orig_task.sources, &new.sources)?;
        check_task_references(&new, "generates", &orig_task.generates, &new.generates)?;
        if let (Some(orig), Some(rendered)) = (&orig_task.sandbox, &new.sandbox) {
            check_task_references(
                &new,
                SANDBOX_WRITES,
                &orig.write_tasks,
                &rendered.write_tasks,
            )?;
        }
    }
    finish_compile(orig_task, new, evaluate_sh_vars, ctx, cache)
}

/// Completes `t`, compiled from `orig_task` by [`compiled_task`], against the
/// tree as it is — for a run, the one setup left: expands the `for:` loops
/// over `sources` or `generates` it left, then resolves `from:` and `task:`
/// entries in sources/generates/sandbox.writes through `resolver`. Not without
/// `sh:` vars: a name templated from one renders empty, and no fast-compile
/// consumer reads the globs.
pub async fn resolve(
    orig_task: &Task,
    mut t: Task,
    evaluate_sh_vars: bool,
    resolver: &dyn TaskResolver,
) -> Result<Task, CompileError> {
    expand_tree_loops(&mut t, true, evaluate_sh_vars)?;
    let writes = t
        .sandbox
        .as_ref()
        .is_some_and(|s| !s.write_tasks.is_empty());
    let references = has_from_entries(&t.sources) || has_from_entries(&t.generates);
    if !evaluate_sh_vars || !(references || writes) {
        return Ok(t);
    }
    let mut reach = crate::reach::Reach::new(resolver, &t);
    if has_from_entries(&t.sources) {
        t.sources = reach.globs(&t, orig_task, false).await?;
    }
    if has_from_entries(&t.generates) {
        t.generates = reach.globs(&t, orig_task, true).await?;
    }
    // Only a task about to run needs these grants, but resolved on every full
    // compile, sandboxing on or off, so bad references fail alike.
    if writes {
        let grants = reach.writes(&t).await?;
        if let Some(s) = &mut t.sandbox {
            s.task_grants = grants;
        }
    }
    Ok(t)
}

/// Whether a `for:` loop iterates files: `sources` or `generates`, which only
/// the tree a run hashes can tell.
fn reads_tree(f: &For) -> bool {
    f.matrix.as_ref().is_none_or(Matrix::is_empty)
        && f.list.is_empty()
        && (f.from == "sources" || f.from == "generates")
}

/// Expands the `for:` loops [`compile_run`] left in `t`'s commands and deps,
/// those over files (see [`reads_tree`]), against the tree as it is. With
/// `late`, the bodies and conditions of the commands they make render too.
fn expand_tree_loops(t: &mut Task, late: bool, evaluate_sh_vars: bool) -> Result<(), CompileError> {
    let cmd_loops = t.cmds.iter().any(|c| c.for_.is_some());
    let dep_loops = t.deps.iter().any(|d| d.for_.is_some());
    if !cmd_loops && !dep_loops {
        return Ok(());
    }
    let vars = t.vars.clone().unwrap_or_default();
    let mut cache = Cache::new(vars.clone());
    cache.set_dialect(t.dialect);
    cache.set_lenient(!evaluate_sh_vars);
    if cmd_loops {
        cache.field("cmds");
        let old = std::mem::take(&mut t.cmds);
        let mut moved = Vec::with_capacity(old.len());
        let mut cmds = Vec::with_capacity(old.len());
        let mut pending = Vec::new();
        for cmd in &old {
            moved.push(cmds.len());
            match &cmd.for_ {
                Some(for_) => expand_cmd_loop(
                    cmd,
                    for_,
                    t,
                    &vars,
                    &mut cache,
                    late,
                    &mut cmds,
                    &mut pending,
                )?,
                None => cmds.push(cmd.clone()),
            }
        }
        for p in &mut t.pending_cmds {
            p.index = moved.get(p.index).copied().unwrap_or(p.index);
        }
        t.reads_checksum |= !pending.is_empty();
        t.pending_cmds.extend(pending);
        t.cmds = cmds;
    }
    if dep_loops {
        cache.field("deps");
        let old = std::mem::take(&mut t.deps);
        let mut deps = Vec::with_capacity(old.len());
        for dep in &old {
            match &dep.for_ {
                Some(for_) => expand_dep_loop(dep, for_, t, &vars, &mut cache, &mut deps)?,
                None => deps.push(dep.clone()),
            }
        }
        t.deps = deps;
    }
    match cache.err() {
        Some(err) if evaluate_sh_vars => Err(CompileError::Template(err.clone())),
        _ => Ok(()),
    }
}

/// Sets `t`'s `CHECKSUM` and cache key from `source_hash`, read after setup.
/// Re-renders command bodies and conditions that read the pending value,
/// preconditions and the cache block, whose `vk:` URLs carry the key.
/// An empty `source_hash` (no sources or no reader) leaves `CHECKSUM` unset.
/// `t` is `orig_task` compiled by [`compiled_task`].
pub fn finalize(
    orig_task: &Task,
    mut t: Task,
    source_hash: &str,
    evaluate_sh_vars: bool,
    ctx: &CompileContext<'_>,
) -> Result<Task, CompileError> {
    t.source_hash = source_hash.to_string();
    t.cache_checksum = crate::hash::join_cache_checksum(source_hash, &t.cache_vars_digest);
    let mut vars = t.vars.take().unwrap_or_default();
    if source_hash.is_empty() {
        vars.remove("CHECKSUM");
    } else {
        vars.set(
            "CHECKSUM".to_string(),
            Var {
                value: Some(Value::String(source_hash.to_string())),
                ..Default::default()
            },
        );
    }
    t.vars = Some(vars.clone());
    let mut cache = Cache::new(vars);
    cache.set_dialect(orig_task.dialect);
    for p in std::mem::take(&mut t.pending_cmds) {
        if let Some(cmd) = t.cmds.get_mut(p.index) {
            cmd.cmd = cache.replace_with_extra(&p.cmd, &p.extra);
            cmd.if_ = cache.replace_with_extra(&p.if_, &p.extra);
        }
    }
    finish_compile(orig_task, t, evaluate_sh_vars, ctx, cache)
}

/// Compiles a task a `from:` or `task:` entry reaches, as running it would up
/// to its `setup` and `deps`, the steps that decide what it runs: variables
/// (`sh:` ones included), dotenv, env, and the calls of its commands and
/// `for:` loops, those over files expanded against the tree as it is. Its
/// command bodies and conditions, references, preconditions and cache block
/// are left alone, so template errors there do not fail it.
pub async fn reached_task(
    orig_task: &Task,
    vars: Vars,
    ctx: &CompileContext<'_>,
    compiler: &Compiler,
    logger: &mut Logger,
) -> Result<Task, CompileError> {
    let (mut new, cache) = compile_run(orig_task, vars, true, ctx, compiler, logger, false).await?;
    if let Some(err) = cache.err() {
        return Err(CompileError::Template(err.clone()));
    }
    expand_tree_loops(&mut new, false, true)?;
    Ok(new)
}

/// The part of [`compiled_task`] a run needs to know what it runs: everything
/// up to `setup` and `deps`. Returns the task and the templater holding its
/// vars. With `late`, command bodies and conditions are rendered too, those
/// reading the pending `CHECKSUM` noted in `pending_cmds`; without, they are
/// left as written.
async fn compile_run(
    orig_task: &Task,
    vars: Vars,
    evaluate_sh_vars: bool,
    ctx: &CompileContext<'_>,
    compiler: &Compiler,
    logger: &mut Logger,
    late: bool,
) -> Result<(Task, Cache), CompileError> {
    // Substitute captured wildcards from a `MATCH` var into the task name.
    let full_name = compute_full_name(orig_task, &vars);

    let mut cache = Cache::new(vars.clone());
    cache.set_dialect(orig_task.dialect);
    cache.set_lenient(!evaluate_sh_vars);
    let mut new = Task {
        task: orig_task.task.clone(),
        label: cache.field("label").replace(&orig_task.label),
        desc: cache.field("desc").replace(&orig_task.desc),
        prompt: orig_task.prompt.clone(),
        summary: cache.field("summary").replace(&orig_task.summary),
        aliases: orig_task.aliases.clone(),
        sources: cache.field("sources").replace_globs(&orig_task.sources),
        generates: cache.field("generates").replace_globs(&orig_task.generates),
        sandbox: orig_task.sandbox.as_ref().map(|s| crate::ast::Sandbox {
            reads: cache.field("sandbox.reads").replace_globs(&s.reads),
            writes: cache.field(SANDBOX_WRITES).replace_vec(&s.writes),
            write_tasks: cache.replace_globs(&s.write_tasks),
            task_grants: Vec::new(),
        }),
        dirs: cache.field("dir").replace_vec(&orig_task.dirs),
        set: orig_task.set.clone(),
        shopt: orig_task.shopt.clone(),
        vars: Some(vars.clone()),
        env: None,
        dotenv: cache.field("dotenv").replace_vec(&orig_task.dotenv),
        silent: orig_task.silent,
        interactive: orig_task.interactive,
        internal: orig_task.internal,
        prefix: cache.field("prefix").replace(&orig_task.prefix),
        ignore_error: orig_task.ignore_error,
        run: cache.field("run").replace(&orig_task.run),
        include_vars: orig_task.include_vars.clone(),
        included_taskfile_vars: orig_task.included_taskfile_vars.clone(),
        raw_cmds: orig_task.cmds.clone(),
        cache: None, // resolved below after CHECKSUM is available
        platforms: orig_task.platforms.clone(),
        if_: cache.field("if").replace(&orig_task.if_),
        location: orig_task.location.clone(),
        requires: orig_task.requires.clone(),
        watch: orig_task.watch,
        failfast: orig_task.failfast,
        namespace: orig_task.namespace.clone(),
        dialect: orig_task.dialect,
        full_name,
        ..Default::default()
    };

    // Expand shell symbols (`~`, `$VAR`) in each dir, then prepend the root dir.
    new.dirs = new
        .dirs
        .iter()
        .map(|d| execext::expand_literal(d))
        .collect();
    if !ctx.dir.is_empty() {
        let mut stack = Vec::with_capacity(new.dirs.len().saturating_add(1));
        stack.push(ctx.dir.to_string());
        stack.append(&mut new.dirs);
        new.dirs = stack;
    }
    if new.prefix.is_empty() {
        new.prefix = new.task.clone();
    }

    // Read dotenv files (relative to the task dir), keeping the first value for
    // each key.
    let mut dotenv_envs = Vars::new();
    for dot_env in &new.dotenv {
        let mut path_parts: Vec<String> = new.dirs.clone();
        path_parts.push(dot_env.clone());
        let dot_env_path = filepathext::join_dirs(&path_parts)
            .to_string_lossy()
            .into_owned();
        if !std::path::Path::new(&dot_env_path).exists() {
            continue;
        }
        let contents =
            std::fs::read_to_string(&dot_env_path).map_err(|e| CompileError::Dotenv {
                path: dot_env_path.clone(),
                message: e.to_string(),
            })?;
        for (key, value) in parse_dotenv(&contents) {
            if dotenv_envs.get(&key).is_none() {
                dotenv_envs.set(
                    key,
                    Var {
                        value: Some(Value::String(value)),
                        ..Default::default()
                    },
                );
            }
        }
    }

    // Build the task env: taskfile env, then dotenv, then the task's own env.
    let mut env_vars = Vars::new();
    if let Some(replaced) = cache.field("env").replace_vars(ctx.taskfile_env) {
        env_vars.merge(&replaced, None);
    }
    if let Some(replaced) = cache.field("dotenv").replace_vars(&dotenv_envs) {
        env_vars.merge(&replaced, None);
    }
    if let Some(orig_env) = &orig_task.env
        && let Some(replaced) = cache.field("env").replace_vars(orig_env)
    {
        env_vars.merge(&replaced, None);
    }

    // Resolve dynamic env vars now if requested; not a command that reads
    // CHECKSUM, rendered as written.
    if evaluate_sh_vars && let Some(err @ TemplaterError::ChecksumUnavailable { .. }) = cache.err()
    {
        return Err(CompileError::Template(err.clone()));
    }
    if evaluate_sh_vars {
        let dir = new.compute_dir().to_string_lossy().into_owned();
        let snapshot = env_vars.clone();
        for (k, v) in snapshot.all() {
            if v.value.is_some() || v.sh.is_none() {
                env_vars.set(
                    k.clone(),
                    Var {
                        value: v.value.clone(),
                        ..Default::default()
                    },
                );
                continue;
            }
            let env_list = env::get_from_vars(&env_vars, ctx.env_precedence);
            let static_value = compiler
                .handle_dynamic_var(v, &dir, env_list, logger)
                .await?;
            env_vars.set(
                k.clone(),
                Var {
                    value: Some(Value::String(static_value)),
                    ..Default::default()
                },
            );
        }
    }
    new.env = Some(env_vars);

    if !orig_task.sources.is_empty() {
        new.cache_vars_digest =
            crate::hash::cache_vars_digest(orig_task).map_err(|e| CompileError::CacheKey {
                task: orig_task.name().to_string(),
                message: e.to_string(),
            })?;
    }

    // Resolve commands, expanding `for` loops and copying deferred commands.
    // What a command calls renders now; its body and condition may read
    // CHECKSUM, rendered pending and noted for `finalize`.
    cache.field("cmds");
    if !orig_task.cmds.is_empty() {
        let mut cmds = Vec::with_capacity(orig_task.cmds.len());
        let mut pending = Vec::new();
        for cmd in &orig_task.cmds {
            if let Some(for_) = &cmd.for_ {
                if reads_tree(for_) {
                    // Expanded against the tree a run hashes, by `resolve`.
                    cmds.push(cmd.clone());
                } else {
                    expand_cmd_loop(
                        cmd,
                        for_,
                        &new,
                        &vars,
                        &mut cache,
                        late,
                        &mut cmds,
                        &mut pending,
                    )?;
                }
                continue;
            }
            if cmd.defer {
                // Deferred commands are templated lazily so EXIT_CODE is known.
                if late && evaluate_sh_vars {
                    new.reads_checksum |= check_deferred(cmd, &vars, orig_task.dialect)
                        .map_err(CompileError::Template)?;
                }
                cmds.push(cmd.clone());
                continue;
            }
            let mut new_cmd = cmd.clone();
            new_cmd.task = cache.replace(&cmd.task);
            new_cmd.vars = cmd.vars.as_ref().and_then(|v| cache.replace_vars(v));
            if late {
                render_late_cmd(
                    &mut cache,
                    cmd,
                    &mut new_cmd,
                    &IndexMap::new(),
                    cmds.len(),
                    &mut pending,
                );
            }
            cmds.push(new_cmd);
        }
        new.cmds = cmds;
        new.reads_checksum |= !pending.is_empty();
        new.pending_cmds = pending;
    }

    // Setup runs before the tree is final: its loops expand against the tree
    // as it is. A dep's loop over files waits for `resolve`.
    cache.field("setup");
    new.setup = compile_deps(&orig_task.setup, &new, &vars, &mut cache, false)?;
    cache.field("deps");
    new.deps = compile_deps(&orig_task.deps, &new, &vars, &mut cache, true)?;
    if evaluate_sh_vars {
        let cmd_loops = orig_task.cmds.iter().filter_map(|c| {
            let f = c.for_.as_ref().filter(|f| reads_tree(f))?;
            Some(("cmds", f, &c.task, c.vars.as_ref()))
        });
        let dep_loops = orig_task.deps.iter().filter_map(|d| {
            let f = d.for_.as_ref().filter(|f| reads_tree(f))?;
            Some(("deps", f, &d.task, d.vars.as_ref()))
        });
        for (field, for_, task, call_vars) in cmd_loops.chain(dep_loops) {
            check_tree_loop(field, for_, task, call_vars, &vars, orig_task.dialect)
                .map_err(CompileError::Template)?;
        }
    }
    Ok((new, cache))
}

/// Checks what a loop over files calls, which renders only once `resolve`
/// expands it after setup: it cannot read `CHECKSUM`, refused before setup.
/// Its item and `KEY` render empty; other errors wait for the expansion. Each
/// string renders on its own, so one failing on the empty item hides no read.
fn check_tree_loop(
    field: &str,
    for_: &For,
    task: &str,
    call_vars: Option<&Vars>,
    vars: &Vars,
    dialect: Dialect,
) -> Result<(), TemplaterError> {
    let as_ = if for_.as_.is_empty() {
        "ITEM"
    } else {
        &for_.as_
    };
    let extra = for_extra(as_, &Value::String(String::new()), &[String::new()], 0);
    let check = |render: &dyn Fn(&mut Cache)| {
        let mut cache = Cache::new(vars.clone());
        cache.set_dialect(dialect);
        cache.field(field);
        render(&mut cache);
        match cache.err() {
            Some(err @ TemplaterError::ChecksumUnavailable { .. }) => Err(err.clone()),
            _ => Ok(()),
        }
    };
    check(&|c| {
        c.replace_with_extra(task, &extra);
    })?;
    for (k, v) in call_vars.into_iter().flat_map(Vars::all) {
        let mut one = Vars::new();
        one.set(k.clone(), v.clone());
        check(&|c| {
            c.replace_vars_with_extra(&one, &extra);
        })?;
    }
    Ok(())
}

/// Expands the `for:` loop of `cmd` into `cmds`: what each iteration calls
/// renders now, and with `late` its body and condition, those reading the
/// pending `CHECKSUM` noted in `pending`. `task` gives the directory and the
/// task's own `sources` and `generates`.
#[allow(clippy::too_many_arguments)]
fn expand_cmd_loop(
    cmd: &Cmd,
    for_: &For,
    task: &Task,
    vars: &Vars,
    cache: &mut Cache,
    late: bool,
    cmds: &mut Vec<Cmd>,
    pending: &mut Vec<PendingCmd>,
) -> Result<(), CompileError> {
    let (list, keys) = items_from_for(
        for_,
        &task.compute_dir().to_string_lossy(),
        &task.sources,
        &task.generates,
        Some(vars),
        task.location.as_ref(),
        cache,
    )?;
    let as_ = if for_.as_.is_empty() {
        "ITEM"
    } else {
        &for_.as_
    };
    for (i, loop_value) in list.iter().enumerate() {
        let extra = for_extra(as_, loop_value, &keys, i);
        let mut new_cmd = cmd.clone();
        new_cmd.for_ = None;
        new_cmd.task = cache.replace_with_extra(&cmd.task, &extra);
        new_cmd.vars =
            cache.replace_vars_with_extra(cmd.vars.as_ref().unwrap_or(&Vars::new()), &extra);
        if late {
            render_late_cmd(cache, cmd, &mut new_cmd, &extra, cmds.len(), pending);
        }
        cmds.push(new_cmd);
    }
    Ok(())
}

/// Expands the `for:` loop of `dep` into `deps`, as [`expand_cmd_loop`] does
/// for a command.
fn expand_dep_loop(
    dep: &Dep,
    for_: &For,
    task: &Task,
    vars: &Vars,
    cache: &mut Cache,
    deps: &mut Vec<Dep>,
) -> Result<(), CompileError> {
    let (list, keys) = items_from_for(
        for_,
        &task.compute_dir().to_string_lossy(),
        &task.sources,
        &task.generates,
        Some(vars),
        task.location.as_ref(),
        cache,
    )?;
    let as_ = if for_.as_.is_empty() {
        "ITEM"
    } else {
        &for_.as_
    };
    for (i, loop_value) in list.iter().enumerate() {
        let extra = for_extra(as_, loop_value, &keys, i);
        let mut new_dep = dep.clone();
        new_dep.for_ = None;
        new_dep.task = cache.replace_with_extra(&dep.task, &extra);
        new_dep.vars =
            cache.replace_vars_with_extra(dep.vars.as_ref().unwrap_or(&Vars::new()), &extra);
        deps.push(new_dep);
    }
    Ok(())
}

/// Renders a command's body and condition from `raw`, the late fields, which
/// may read the pending `CHECKSUM`: a command that does is noted in `pending`
/// at `index`, for [`finalize`].
fn render_late_cmd(
    cache: &mut Cache,
    raw: &Cmd,
    rendered: &mut Cmd,
    extra: &IndexMap<String, Value>,
    index: usize,
    pending: &mut Vec<PendingCmd>,
) {
    cache.set_late(true);
    rendered.cmd = cache.replace_with_extra(&raw.cmd, extra);
    rendered.if_ = cache.replace_with_extra(&raw.if_, extra);
    cache.set_late(false);
    if cache.take_read_checksum() {
        pending.push(PendingCmd {
            index,
            cmd: raw.cmd.clone(),
            if_: raw.if_.clone(),
            extra: extra.clone(),
        });
    }
}

/// Checks a deferred command, which renders only when it runs: what it calls
/// cannot read `CHECKSUM`. Returns whether its body or condition reads it.
/// Other errors wait for the run, which knows `EXIT_CODE`.
fn check_deferred(cmd: &Cmd, vars: &Vars, dialect: Dialect) -> Result<bool, TemplaterError> {
    let mut cache = Cache::new(vars.clone());
    cache.set_dialect(dialect);
    cache.field("cmds").replace(&cmd.task);
    if let Some(v) = &cmd.vars {
        cache.replace_vars(v);
    }
    if let Some(err @ TemplaterError::ChecksumUnavailable { .. }) = cache.err() {
        return Err(err.clone());
    }
    let mut cache = Cache::new(vars.clone());
    cache.set_dialect(dialect);
    cache.set_late(true);
    cache.replace(&cmd.cmd);
    cache.replace(&cmd.if_);
    Ok(cache.take_read_checksum())
}

/// The rest of [`compiled_task`], once references are resolved:
/// preconditions, the cache block and the checks on it. Both are late fields,
/// rendered again by [`finalize`].
fn finish_compile(
    orig_task: &Task,
    mut new: Task,
    evaluate_sh_vars: bool,
    ctx: &CompileContext<'_>,
    mut cache: Cache,
) -> Result<Task, CompileError> {
    cache.set_late(true);
    // Resolve preconditions.
    if !orig_task.preconditions.is_empty() {
        let mut preconditions = Vec::with_capacity(orig_task.preconditions.len());
        for precondition in &orig_task.preconditions {
            preconditions.push(Precondition {
                sh: cache.replace(&precondition.sh),
                msg: cache.replace(&precondition.msg),
            });
        }
        new.preconditions = preconditions;
    }

    // Resolve the cache block: merge a named model from the taskfile `caches:`
    // map (task-level fields override), then template the URL/lock/if fields.
    if let Some(orig_cache) = &orig_task.cache {
        let model = if orig_cache.inherit.is_empty() {
            None
        } else {
            ctx.caches.get(&orig_cache.inherit)
        };
        let mut resolved = orig_cache.clone();
        if let Some(model) = model {
            let mut merged = model.clone();
            if !resolved.url.is_empty() {
                merged.url = resolved.url.clone();
            }
            if !resolved.lock.is_empty() {
                merged.lock = resolved.lock.clone();
            }
            if !resolved.vk.is_empty() {
                merged.vk = resolved.vk.clone();
            }
            if !resolved.api_key.is_empty() {
                merged.api_key = resolved.api_key.clone();
            }
            if !resolved.namespace.is_empty() {
                merged.namespace = resolved.namespace.clone();
            }
            if !resolved.if_.is_empty() {
                merged.if_ = resolved.if_.clone();
            }
            if resolved.enabled.is_some() {
                merged.enabled = resolved.enabled;
            }
            if !resolved.ttl.is_empty() {
                merged.ttl = resolved.ttl.clone();
            }
            if !resolved.lock_timeout.is_empty() {
                merged.lock_timeout = resolved.lock_timeout.clone();
            }
            if resolved.outside_links.is_some() {
                merged.outside_links = resolved.outside_links;
            }
            resolved = merged;
        }
        resolved.inherit = String::new();
        let vk_configured = !resolved.vk.is_empty();
        // Each field renders in the dialect of the block it was written in: the
        // task's own file for a task-level value, the defining file for one
        // inherited from `caches:`. The two differ while a tree is partway
        // through `--migrate`, so a single dialect for the merged block would
        // render the inherited half through the wrong templater. Without a
        // model the two are the same file, so the task's dialect applies.
        // `dialect_of` mirrors the merge above — a non-empty task value
        // overrides — and has to stay in step with it.
        let model_dialect = model.map_or(orig_task.dialect, |m| m.dialect);
        let dialect_of = |task_value: &str| {
            if task_value.is_empty() {
                model_dialect
            } else {
                orig_task.dialect
            }
        };
        cache.set_dialect(dialect_of(&orig_cache.url));
        resolved.url = cache.replace(&resolved.url);
        cache.set_dialect(dialect_of(&orig_cache.lock));
        resolved.lock = cache.replace(&resolved.lock);
        cache.set_dialect(dialect_of(&orig_cache.if_));
        resolved.if_ = cache.replace(&resolved.if_);
        cache.set_dialect(dialect_of(&orig_cache.lock_timeout));
        resolved.lock_timeout = cache.replace(&resolved.lock_timeout);
        cache.set_dialect(dialect_of(&orig_cache.vk));
        resolved.vk = cache.replace(&resolved.vk).trim().to_string();
        cache.set_dialect(dialect_of(&orig_cache.api_key));
        resolved.api_key = cache.replace(&resolved.api_key).trim().to_string();
        cache.set_dialect(dialect_of(&orig_cache.namespace));
        resolved.namespace = cache.replace(&resolved.namespace).trim().to_string();
        // Leave the templater on the task's dialect for anything rendered after
        // this block, and record it on the compiled block, whose strings are all
        // resolved by now.
        cache.set_dialect(orig_task.dialect);
        resolved.dialect = orig_task.dialect;

        // A `vk` model derives its URLs. Rendering empty — the registry comes
        // from a CI variable that is unset locally — is "no registry", and
        // there is nothing to cache to or lock on.
        if vk_configured {
            if !resolved.url.is_empty() || !resolved.lock.is_empty() {
                return Err(CompileError::CacheVkWithUrl {
                    task: orig_task.name().to_string(),
                });
            }
            if resolved.vk.is_empty() {
                resolved.enabled = Some(false);
            } else if !crate::cache::vk::is_valid(&resolved.vk) {
                return Err(CompileError::CacheVkInvalid {
                    task: orig_task.name().to_string(),
                    vk: resolved.vk.clone(),
                });
            } else {
                // Key the entry by the task's local name, not its qualified
                // one, so every include path to a task shares one entry; the
                // checksum carries the declared `vars`, which the source hash
                // does not, so tasks that render different commands stay apart.
                resolved.url = crate::cache::vk::cache_url(
                    &resolved.vk,
                    &resolved.namespace,
                    &new.local_name(),
                    &new.cache_checksum,
                );
                resolved.lock = crate::cache::vk::lock_url(&resolved.vk, &resolved.namespace);
            }
        }
        new.cache = Some(resolved);
    }
    new.reads_checksum |= cache.take_read_checksum();

    // Reject cached tasks whose generates escape the project root — such a path
    // would write outside the archive on extraction.
    if !ctx.dir.is_empty() && cache_is_enabled(&new) {
        let task_dir = new.compute_dir();
        let task_dir = task_dir.to_string_lossy();
        for g in &new.generates {
            let resolved = filepathext::smart_join(&task_dir, &g.glob);
            let outside = match filepathext::rel_str(ctx.dir, &resolved.to_string_lossy()) {
                Some(rel) => rel.starts_with(".."),
                None => true,
            };
            if outside {
                return Err(CompileError::CacheGeneratesOutsideRoot {
                    task: new.name().to_string(),
                    glob: g.glob.clone(),
                    root: ctx.dir.to_string(),
                });
            }
        }
    }

    // Surface templater errors only when evaluating shell variables, matching Go.
    if evaluate_sh_vars && let Some(err) = cache.err() {
        return Err(CompileError::Template(err.clone()));
    }

    Ok(new)
}

/// Substitutes each captured wildcard from a `MATCH` var into the task name,
/// replacing successive `*` characters. Ports the `MATCH` handling in Go
/// `compiledTask`.
fn compute_full_name(orig_task: &Task, vars: &Vars) -> String {
    let mut full_name = orig_task.task.clone();
    if let Some(matches) = vars.get("MATCH")
        && let Some(Value::Sequence(seq)) = &matches.value
    {
        for m in seq {
            if let Value::String(s) = m {
                full_name = full_name.replacen('*', s, 1);
            }
        }
    }
    full_name
}

/// Reports whether any glob in the slice carries a `from:` or `task:`
/// reference.
fn has_from_entries(globs: &[Glob]) -> bool {
    globs.iter().any(Glob::is_reference)
}

/// The field name `sandbox.writes` errors carry.
pub(crate) const SANDBOX_WRITES: &str = "sandbox.writes";

/// Rejects task names that render empty. `replace_globs` preserves entry
/// order, so `orig` and `rendered` correspond entry for entry.
fn check_task_references(
    task: &Task,
    field: &str,
    orig: &[Glob],
    rendered: &[Glob],
) -> Result<(), CompileError> {
    match orig
        .iter()
        .zip(rendered)
        .find(|(o, r)| !o.task.is_empty() && r.task.is_empty())
    {
        Some((o, _)) => Err(CompileError::EmptyTaskReference {
            task: task.name().to_string(),
            field: field.to_string(),
            template: o.task.clone(),
        }),
        None => Ok(()),
    }
}

/// Settles the groups of a resolved list. They matter only when a folded
/// task's entries exclude something: otherwise every entry joins group 0, which
/// evaluates, deduplicates and hashes exactly as one flat list, its folded
/// entries marked [`Glob::folded`].
pub(crate) fn finish_groups(mut globs: Vec<Glob>) -> Vec<Glob> {
    let grouped = globs.iter().any(|g| g.group != 0 && g.negate);
    if !grouped {
        for g in &mut globs {
            g.folded = g.group != 0;
            g.group = 0;
        }
    }
    dedup(globs, grouped)
}

/// Keeps the first of equal entries (same pattern, negation and fingerprint,
/// and the same group when `grouped`), in order; folded only if each is.
fn dedup(globs: Vec<Glob>, grouped: bool) -> Vec<Glob> {
    use std::collections::{HashMap, hash_map::Entry};
    let mut seen: HashMap<(u32, String), usize> = HashMap::new();
    let mut out: Vec<Glob> = Vec::new();
    for g in globs {
        let mut key = g.glob.clone();
        if g.negate {
            key = format!("!{key}");
        }
        if !g.fingerprint.is_empty() {
            key.push('\u{0}');
            key.push_str("fp:");
            key.push_str(&g.fingerprint);
        }
        let group = if grouped { g.group } else { 0 };
        match seen.entry((group, key)) {
            Entry::Occupied(at) => {
                if let Some(kept) = out.get_mut(*at.get()) {
                    kept.folded &= g.folded;
                }
            }
            Entry::Vacant(at) => {
                at.insert(out.len());
                out.push(g);
            }
        }
    }
    out
}

/// Whether caching is active for a compiled task: a cache block is present and
/// not disabled by an explicit flag or a resolved `if:` condition. Mirrors the
/// executor's `cache_enabled`.
fn cache_is_enabled(t: &Task) -> bool {
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

/// Builds the `extra` template map for a `for` iteration: the iterator variable
/// (named `as_`) plus `KEY` when looping over a map.
fn for_extra(
    as_: &str,
    loop_value: &Value,
    keys: &[String],
    index: usize,
) -> IndexMap<String, Value> {
    let mut extra = IndexMap::new();
    extra.insert(as_.to_string(), loop_value.clone());
    if let Some(key) = keys.get(index) {
        extra.insert("KEY".to_string(), Value::String(key.clone()));
    }
    extra
}

/// Resolves the list (and optional keys) a `for` clause iterates over: a
/// matrix product, an explicit list, a sources/generates glob expansion, or a
/// delimiter-split variable. Ports Go `itemsFromFor`.
fn items_from_for(
    f: &For,
    dir: &str,
    sources: &[Glob],
    generates: &[Glob],
    vars: Option<&Vars>,
    location: Option<&Location>,
    cache: &mut Cache,
) -> Result<(Vec<Value>, Vec<String>), CompileError> {
    let _ = location;
    // A matrix produces the cartesian product of its rows.
    if let Some(matrix) = &f.matrix
        && !matrix.is_empty()
    {
        let mut matrix = matrix.clone();
        resolve_matrix_refs(&mut matrix, cache)?;
        let product = product(&matrix);
        let values = product.into_iter().map(Value::Mapping).collect();
        return Ok((values, Vec::new()));
    }
    // An explicit list is used verbatim.
    if !f.list.is_empty() {
        return Ok((f.list.clone(), Vec::new()));
    }
    // `from: sources` / `from: generates` expand the task globs, relativized.
    if f.from == "sources" || f.from == "generates" {
        let globs = if f.from == "sources" {
            sources
        } else {
            generates
        };
        let mut glist =
            fingerprint::globs(dir, globs).map_err(|e| CompileError::Glob(e.to_string()))?;
        for path in &mut glist {
            if let Some(rel) = filepathext::rel_str(dir, path) {
                *path = rel;
            }
        }
        let values = glist.into_iter().map(Value::String).collect();
        return Ok((values, Vec::new()));
    }
    // A variable is split into a list (or iterated as a map).
    if !f.var.is_empty()
        && let Some(vars) = vars
        && let Some(v) = vars.get(&f.var)
    {
        // A dynamic var is not yet resolved, so it cannot be a list.
        if v.value.is_some() && v.sh.is_none() {
            return split_for_var(v, f);
        }
    }
    Ok((Vec::new(), Vec::new()))
}

/// Splits a resolved `for.var` value into iteration values (and keys for maps).
fn split_for_var(v: &Var, f: &For) -> Result<(Vec<Value>, Vec<String>), CompileError> {
    match &v.value {
        Some(Value::String(s)) => {
            let parts: Vec<Value> = if f.split.is_empty() {
                s.split_whitespace()
                    .map(|p| Value::String(p.to_string()))
                    .collect()
            } else {
                s.split(&f.split)
                    .map(|p| Value::String(p.to_string()))
                    .collect()
            };
            Ok((parts, Vec::new()))
        }
        Some(Value::Sequence(seq)) => Ok((seq.clone(), Vec::new())),
        Some(Value::Mapping(map)) => {
            let mut keys = Vec::with_capacity(map.len());
            let mut values = Vec::with_capacity(map.len());
            for (k, val) in map {
                let key = match k {
                    Value::String(s) => s.clone(),
                    other => crate::yaml::to_string(other)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                };
                keys.push(key);
                values.push(val.clone());
            }
            Ok((values, keys))
        }
        _ => Ok((Vec::new(), Vec::new())),
    }
}

/// Resolves `ref:` rows in a matrix to their referenced list values. Ports Go
/// `resolveMatrixRefs`.
fn resolve_matrix_refs(matrix: &mut Matrix, cache: &mut Cache) -> Result<(), CompileError> {
    if matrix.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = matrix.keys().cloned().collect();
    for key in keys {
        let Some(row) = matrix.get(&key) else {
            continue;
        };
        if row.ref_.is_empty() {
            continue;
        }
        let reference = row.ref_.clone();
        let resolved = cache.resolve_ref(&reference);
        match resolved {
            Value::Sequence(seq) => {
                if let Some(row) = matrix.get(&key).cloned() {
                    matrix.set(
                        key.clone(),
                        crate::ast::MatrixRow {
                            ref_: row.ref_,
                            value: seq,
                        },
                    );
                }
            }
            _ => return Err(CompileError::MatrixRef(reference)),
        }
    }
    Ok(())
}

/// Produces the cartesian product of a matrix's rows as a list of maps. Ports
/// Go `product`.
fn product(matrix: &Matrix) -> Vec<crate::yaml::Mapping> {
    if matrix.is_empty() {
        return Vec::new();
    }
    let mut result: Vec<crate::yaml::Mapping> = vec![crate::yaml::Mapping::new()];
    for (key, row) in matrix.all() {
        let mut new_result = Vec::new();
        for combination in &result {
            for item in &row.value {
                let mut comb = combination.clone();
                comb.insert(Value::String(key.clone()), item.clone());
                new_result.push(comb);
            }
        }
        result = new_result;
    }
    result
}

/// Resolves templates and `for` loops in a list of deps (used for both `setup`
/// and `deps`). With `tree_loops`, a loop over files (see [`reads_tree`]) is
/// left for [`resolve`]. Ports Go `compileDeps`.
fn compile_deps(
    deps: &[Dep],
    task: &Task,
    vars: &Vars,
    cache: &mut Cache,
    tree_loops: bool,
) -> Result<Vec<Dep>, CompileError> {
    if deps.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::with_capacity(deps.len());
    for dep in deps {
        if let Some(for_) = &dep.for_ {
            if tree_loops && reads_tree(for_) {
                result.push(dep.clone());
            } else {
                expand_dep_loop(dep, for_, task, vars, cache, &mut result)?;
            }
            continue;
        }
        let mut new_dep = dep.clone();
        new_dep.task = cache.replace(&dep.task);
        new_dep.vars = dep.vars.as_ref().and_then(|v| cache.replace_vars(v));
        result.push(new_dep);
    }
    Ok(result)
}

/// Parses a minimal `.env` file into key/value pairs, matching the common
/// subset of the Go `godotenv.Read` behavior. Blank lines and `#` comments are
/// skipped; values may be single- or double-quoted; an `export ` prefix is
/// stripped.
fn parse_dotenv(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let value = unquote_dotenv(value.trim());
        out.push((key, value));
    }
    out
}

/// Removes matching surrounding quotes from a dotenv value, leaving unquoted
/// values as-is (with a trailing inline comment stripped).
fn unquote_dotenv(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes.first().copied();
        let last = bytes.last().copied();
        if (first == Some(b'"') && last == Some(b'"'))
            || (first == Some(b'\'') && last == Some(b'\''))
        {
            return value
                .get(1..value.len().saturating_sub(1))
                .unwrap_or("")
                .to_string();
        }
    }
    // Strip an inline comment from an unquoted value.
    match value.split_once(" #") {
        Some((v, _)) => v.trim().to_string(),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ast::{Cmd, Dialect, For, Var, VarElement, Vars};
    use crate::compiler::Compiler;

    fn silent_logger() -> Logger {
        Logger {
            stdout: Box::new(Vec::new()),
            stderr: Box::new(Vec::new()),
            ..Default::default()
        }
    }

    fn compiler() -> Compiler {
        Compiler::new(
            String::new(),
            "Taskfile.yml".to_string(),
            String::new(),
            Vars::new(),
            Vars::new(),
            false,
        )
    }

    fn ctx(taskfile_env: &Vars) -> CompileContext<'_> {
        static EMPTY: std::sync::LazyLock<Caches> = std::sync::LazyLock::new(Caches::default);
        ctx_with_caches(taskfile_env, &EMPTY)
    }

    fn ctx_with_caches<'a>(taskfile_env: &'a Vars, caches: &'a Caches) -> CompileContext<'a> {
        CompileContext {
            dir: "",
            taskfile_env,
            env_precedence: false,
            caches,
        }
    }

    /// `t`, compiled from `orig`, finalized as a run would once it hashed the
    /// sources to `HASH`.
    const HASH: &str = "0123456789abcdef0123456789abcdef";
    fn finalized(orig: &Task, t: Task, ctx: &CompileContext<'_>) -> Task {
        finalize(orig, t, HASH, true, ctx).unwrap()
    }

    fn string_var(v: &str) -> Var {
        Var {
            value: Some(Value::String(v.to_string())),
            ..Default::default()
        }
    }

    /// Every field a task sets overrides the model it inherits, and every field
    /// it leaves out comes from the model.
    #[tokio::test]
    async fn task_cache_fields_override_the_inherited_model() {
        let caches = Caches(HashMap::from([(
            "default".to_string(),
            crate::ast::Cache {
                url: "file:///model.zip".to_string(),
                lock: "redis://model/lock".to_string(),
                if_: "false".to_string(),
                enabled: Some(false),
                ttl: "72h".to_string(),
                lock_timeout: "5m".to_string(),
                ..Default::default()
            },
        )]));
        let orig = Task {
            task: "build".to_string(),
            cache: Some(crate::ast::Cache {
                inherit: "default".to_string(),
                url: "file:///task.zip".to_string(),
                lock: "redis://task/lock".to_string(),
                if_: "true".to_string(),
                enabled: Some(true),
                ttl: "1h".to_string(),
                // Left out, so it comes from the model.
                lock_timeout: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let out = compiled_task(&orig, Vars::new(), true, &ctx, &c, &mut logger)
            .await
            .unwrap();
        let cache = out.cache.unwrap();
        assert_eq!(cache.url, "file:///task.zip");
        assert_eq!(cache.lock, "redis://task/lock");
        assert_eq!(cache.if_, "true");
        assert_eq!(cache.enabled, Some(true));
        assert_eq!(cache.ttl, "1h");
        assert_eq!(cache.lock_timeout, "5m");
        // The name is consumed by the merge, not carried into the compiled task.
        assert!(cache.inherit.is_empty());
    }

    /// A `caches:` model keeps the dialect of the file that defined it, so a Go
    /// task inheriting a Jinja model — a tree partway through `--migrate` —
    /// still renders the model as Jinja, while a field the task overrides
    /// renders in the task's own dialect.
    #[tokio::test]
    async fn cache_fields_render_in_the_dialect_of_their_origin_block() {
        let caches = Caches(HashMap::from([(
            "default".to_string(),
            crate::ast::Cache {
                url: "oci://{{ REPO }}/task".to_string(),
                lock: "redis://{{ REPO }}/lock".to_string(),
                if_: "{{ REPO != \"\" }}".to_string(),
                lock_timeout: "{{ LOCK_WAIT }}".to_string(),
                dialect: Dialect::Jinja,
                ..Default::default()
            },
        )]));
        let orig = Task {
            task: "build".to_string(),
            dialect: Dialect::Go,
            cache: Some(crate::ast::Cache {
                inherit: "default".to_string(),
                lock: "redis://{{.REPO}}/task-lock".to_string(),
                lock_timeout: "{{.LOCK_WAIT}}".to_string(),
                dialect: Dialect::Go,
                ..Default::default()
            }),
            ..Default::default()
        };
        let vars = Vars::from_elements([
            VarElement {
                key: "REPO".to_string(),
                value: string_var("reg.example/cache"),
            },
            VarElement {
                key: "LOCK_WAIT".to_string(),
                value: string_var("5m"),
            },
        ]);
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let out = compiled_task(&orig, vars, true, &ctx, &c, &mut logger)
            .await
            .unwrap();
        let cache = out.cache.unwrap();
        // Inherited from the Jinja model, so rendered as Jinja.
        assert_eq!(cache.url, "oci://reg.example/cache/task");
        assert_eq!(cache.if_, "true");
        // Overridden by the Go task, so rendered as Go.
        assert_eq!(cache.lock, "redis://reg.example/cache/task-lock");
        assert_eq!(cache.lock_timeout, "5m");
        // The merged block is normalised onto the task's dialect.
        assert_eq!(cache.dialect, Dialect::Go);
    }

    fn vk_model(vk: &str) -> Caches {
        Caches(HashMap::from([(
            "default".to_string(),
            crate::ast::Cache {
                vk: vk.to_string(),
                namespace: "{{ NS }}".to_string(),
                api_key: "{{ KEY }}".to_string(),
                dialect: Dialect::Jinja,
                ..Default::default()
            },
        )]))
    }

    fn vk_task(cache: crate::ast::Cache) -> Task {
        Task {
            task: "build".to_string(),
            dialect: Dialect::Jinja,
            sources: vec![crate::ast::Glob {
                glob: "Cargo.toml".to_string(),
                ..Default::default()
            }],
            cache: Some(cache),
            ..Default::default()
        }
    }

    fn vk_vars() -> Vars {
        Vars::from_elements([
            VarElement {
                key: "NS".to_string(),
                value: string_var("gcc 13"),
            },
            VarElement {
                key: "KEY".to_string(),
                value: string_var("vkr_k"),
            },
        ])
    }

    /// A `vk` model spells out the registry once; the cache and lock URLs
    /// follow from it, the task name and the checksum.
    #[tokio::test]
    async fn vk_model_derives_the_cache_and_lock_urls() {
        let caches = vk_model("reg.example/task-cache");
        let orig = vk_task(crate::ast::Cache {
            inherit: "default".to_string(),
            ..Default::default()
        });
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let out = compiled_task(&orig, vk_vars(), true, &ctx, &c, &mut logger)
            .await
            .unwrap();
        let out = finalized(&orig, out, &ctx);
        let key = out.cache_checksum.clone();
        assert_eq!(out.source_hash, HASH);
        assert!(key.starts_with(&format!("{HASH}-")), "{key}");
        let cache = out.cache.unwrap();
        assert_eq!(cache.vk, "reg.example/task-cache");
        assert_eq!(cache.api_key, "vkr_k");
        assert_eq!(
            cache.url,
            format!("oci://reg.example/task-cache:gcc-13-build-{key}")
        );
        assert_eq!(cache.lock, "vks://reg.example/task-cache/gcc-13");
        assert!(cache.enabled.is_none());
    }

    /// The directly included copy (`sub:build`) and the nested one
    /// (`outer:sub:build`) key their entries by the task's local name, so
    /// neither include path appears in the tag and both share one entry.
    #[tokio::test]
    async fn vk_cache_url_is_independent_of_include_depth() {
        let caches = vk_model("reg.example/task-cache");
        let included = |name: &str, namespace: &str| {
            let mut t = vk_task(crate::ast::Cache {
                inherit: "default".to_string(),
                ..Default::default()
            });
            t.task = name.to_string();
            t.namespace = namespace.to_string();
            t
        };
        let direct = included("sub:build", "sub");
        let nested = included("outer:sub:build", "outer:sub");
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let a = compiled_task(&direct, vk_vars(), true, &ctx, &c, &mut logger)
            .await
            .unwrap();
        let a = finalized(&direct, a, &ctx);
        let b = compiled_task(&nested, vk_vars(), true, &ctx, &c, &mut logger)
            .await
            .unwrap();
        let b = finalized(&nested, b, &ctx);
        let key = a.cache_checksum.clone();
        assert!(!key.is_empty());
        assert_eq!(a.cache_checksum, b.cache_checksum);
        let (a, b) = (a.cache.unwrap(), b.cache.unwrap());
        assert_eq!(
            a.url,
            format!("oci://reg.example/task-cache:gcc-13-build-{key}")
        );
        assert_eq!(a.url, b.url);
    }

    /// Two includes of one Taskfile that pass different `vars` share a local
    /// name and a source hash but render different commands, so their entries
    /// carry a digest of those vars and stay apart; the same key is stored on
    /// the task for the build-once lock.
    #[tokio::test]
    async fn vk_cache_url_keeps_includes_with_different_vars_apart() {
        let caches = vk_model("reg.example/task-cache");
        let included = |namespace: &str, cc: &str| {
            let mut t = vk_task(crate::ast::Cache {
                inherit: "default".to_string(),
                ..Default::default()
            });
            t.task = format!("{namespace}:build");
            t.namespace = namespace.to_string();
            t.include_vars = Some(Vars::from_elements([VarElement {
                key: "CC".to_string(),
                value: string_var(cc),
            }]));
            t
        };
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let mut urls = Vec::new();
        for t in [
            included("gcc", "gcc"),
            included("clang", "clang"),
            included("gcc2", "gcc"),
        ] {
            let out = compiled_task(&t, vk_vars(), true, &ctx, &c, &mut logger)
                .await
                .unwrap();
            let out = finalized(&t, out, &ctx);
            let key = out.cache_checksum.clone();
            let url = out.cache.unwrap().url;
            assert!(key.starts_with(&format!("{HASH}-")), "{key}");
            assert_eq!(
                url,
                format!("oci://reg.example/task-cache:gcc-13-build-{key}")
            );
            urls.push(url);
        }
        assert_ne!(urls[0], urls[1]);
        assert_eq!(urls[0], urls[2]);
    }

    /// A registry that renders empty — its CI variable is unset locally — is
    /// no registry: the block is disabled rather than pointed at nothing.
    #[tokio::test]
    async fn vk_rendering_empty_disables_the_cache() {
        let caches = vk_model("{{ REGISTRY }}");
        let orig = vk_task(crate::ast::Cache {
            inherit: "default".to_string(),
            enabled: Some(true),
            ..Default::default()
        });
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let out = compiled_task(&orig, vk_vars(), true, &ctx, &c, &mut logger)
            .await
            .unwrap();
        let cache = out.cache.unwrap();
        assert_eq!(cache.enabled, Some(false));
        assert!(cache.url.is_empty());
        assert!(cache.lock.is_empty());
    }

    /// `vk` owns the URLs; a task that also spells one out is refused rather
    /// than silently overridden either way.
    #[tokio::test]
    async fn vk_with_an_explicit_url_is_an_error() {
        let caches = vk_model("reg.example/task-cache");
        let orig = vk_task(crate::ast::Cache {
            inherit: "default".to_string(),
            url: "file:///tmp/x.zip".to_string(),
            ..Default::default()
        });
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let Err(err) = compiled_task(&orig, vk_vars(), true, &ctx, &c, &mut logger).await else {
            panic!("vk with url must fail");
        };
        assert!(
            matches!(err, CompileError::CacheVkWithUrl { .. }),
            "unexpected error: {err}"
        );
    }

    /// A `vk` carrying a scheme (or anything else beyond `host[:port]/repo`)
    /// would be copied verbatim into both derived URLs, so it is refused.
    #[tokio::test]
    async fn vk_with_a_scheme_is_an_error() {
        let caches = vk_model("https://reg.example/task-cache");
        let orig = vk_task(crate::ast::Cache {
            inherit: "default".to_string(),
            ..Default::default()
        });
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let ctx = ctx_with_caches(&env, &caches);
        let Err(err) = compiled_task(&orig, vk_vars(), true, &ctx, &c, &mut logger).await else {
            panic!("vk with a scheme must fail");
        };
        assert!(
            matches!(err, CompileError::CacheVkInvalid { .. }),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn templates_cmds_and_label() {
        let orig = Task {
            task: "build".to_string(),
            label: "Build {{.NAME}}".to_string(),
            cmds: vec![Cmd {
                cmd: "echo {{.NAME}}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let vars = Vars::from_elements([VarElement {
            key: "NAME".to_string(),
            value: string_var("app"),
        }]);
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let out = compiled_task(&orig, vars, true, &ctx(&env), &c, &mut logger)
            .await
            .unwrap();
        assert_eq!(out.label, "Build app");
        assert_eq!(out.cmds[0].cmd, "echo app");
        assert_eq!(out.prefix, "build");
    }

    #[tokio::test]
    async fn for_list_expands_cmds() {
        let orig = Task {
            task: "loop".to_string(),
            cmds: vec![Cmd {
                cmd: "echo {{.ITEM}}".to_string(),
                for_: Some(For {
                    list: vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let out = compiled_task(&orig, Vars::new(), true, &ctx(&env), &c, &mut logger)
            .await
            .unwrap();
        assert_eq!(out.cmds.len(), 2);
        assert_eq!(out.cmds[0].cmd, "echo a");
        assert_eq!(out.cmds[1].cmd, "echo b");
        assert!(out.cmds[0].for_.is_none());
    }

    #[tokio::test]
    async fn env_merges_taskfile_and_task() {
        let orig = Task {
            task: "t".to_string(),
            env: Some(Vars::from_elements([VarElement {
                key: "TASK_LEVEL".to_string(),
                value: string_var("task"),
            }])),
            ..Default::default()
        };
        let taskfile_env = Vars::from_elements([VarElement {
            key: "FILE_LEVEL".to_string(),
            value: string_var("file"),
        }]);
        let c = compiler();
        let mut logger = silent_logger();
        let out = compiled_task(
            &orig,
            Vars::new(),
            true,
            &ctx(&taskfile_env),
            &c,
            &mut logger,
        )
        .await
        .unwrap();
        let env = out.env.unwrap();
        assert_eq!(
            env.get("FILE_LEVEL").and_then(|v| v.value.clone()),
            Some(Value::String("file".to_string()))
        );
        assert_eq!(
            env.get("TASK_LEVEL").and_then(|v| v.value.clone()),
            Some(Value::String("task".to_string()))
        );
    }

    #[tokio::test]
    async fn dynamic_env_var_is_resolved() {
        let orig = Task {
            task: "t".to_string(),
            env: Some(Vars::from_elements([VarElement {
                key: "DYN".to_string(),
                value: Var {
                    sh: Some("echo from-shell".to_string()),
                    ..Default::default()
                },
            }])),
            ..Default::default()
        };
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let out = compiled_task(&orig, Vars::new(), true, &ctx(&env), &c, &mut logger)
            .await
            .unwrap();
        assert_eq!(
            out.env.unwrap().get("DYN").and_then(|v| v.value.clone()),
            Some(Value::String("from-shell".to_string()))
        );
    }

    fn glob(pattern: &str, negate: bool, group: u32) -> Glob {
        Glob {
            glob: pattern.to_string(),
            negate,
            group,
            ..Default::default()
        }
    }

    /// Without a folded exclusion, groups collapse into one deduplicated list,
    /// as before they existed, an entry folded only if no copy is the task's
    /// own; with one, equal entries of different groups both stay.
    #[test]
    fn finish_groups_collapses_unless_a_folded_task_excludes() {
        let flat = finish_groups(vec![
            glob("/r/a", false, 0),
            glob("/r/x", true, 0),
            glob("/r/a", false, 1),
            glob("/r/b", false, 2),
            glob("/r/b", false, 1),
        ]);
        let folded = |mut g: Glob| {
            g.folded = true;
            g
        };
        assert_eq!(
            flat,
            vec![
                glob("/r/a", false, 0),
                glob("/r/x", true, 0),
                folded(glob("/r/b", false, 0))
            ]
        );
        let own_later = finish_groups(vec![glob("/r/a", false, 1), glob("/r/a", false, 0)]);
        assert_eq!(own_later, vec![glob("/r/a", false, 0)]);
        let grouped = finish_groups(vec![
            glob("/r/a", false, 0),
            glob("/r/a", false, 1),
            glob("/r/x", true, 1),
            glob("/r/a", false, 2),
            glob("/r/a", false, 2),
        ]);
        assert_eq!(
            grouped,
            vec![
                glob("/r/a", false, 0),
                glob("/r/a", false, 1),
                glob("/r/x", true, 1),
                glob("/r/a", false, 2)
            ]
        );
    }

    /// Counts the compiles a full compile asks for.
    struct Counting(std::cell::Cell<usize>);

    impl TaskResolver for Counting {
        fn resolve(&self, name: &str) -> Result<ResolvedCall, CompileError> {
            Ok(ResolvedCall {
                task: name.to_string(),
                captures: Vec::new(),
                platforms: Vec::new(),
            })
        }

        fn compile_call<'a>(
            &'a self,
            name: &'a str,
            _vars: &'a Vars,
        ) -> Pin<Box<dyn Future<Output = Result<ReachedTask, CompileError>> + 'a>> {
            self.0.set(self.0.get() + 1);
            let task = Task {
                task: name.to_string(),
                ..Default::default()
            };
            Box::pin(async move {
                Ok(ReachedTask {
                    raw: task.clone(),
                    task,
                })
            })
        }

        fn root(&self) -> Option<(&str, &Vars)> {
            None
        }
    }

    /// A task without references compiles no other task, sandboxed or not.
    #[tokio::test]
    async fn no_reference_no_compile() {
        let orig = Task {
            task: "build".to_string(),
            sources: vec![glob("src/**", false, 0)],
            generates: vec![glob("out/*", false, 0)],
            deps: vec![Dep {
                task: "dep".to_string(),
                ..Default::default()
            }],
            sandbox: Some(crate::ast::Sandbox {
                writes: vec!["cache".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let env = Vars::new();
        let c = compiler();
        let mut logger = silent_logger();
        let counting = Counting(std::cell::Cell::new(0));
        let t = compiled_task(&orig, Vars::new(), true, &ctx(&env), &c, &mut logger)
            .await
            .unwrap();
        resolve(&orig, t, true, &counting).await.unwrap();
        assert_eq!(counting.0.get(), 0);
    }

    #[test]
    fn parse_dotenv_handles_quotes_and_comments() {
        let contents = "# comment\nexport A=1\nB=\"two\"\nC='three'\nD=four # inline\n\n";
        let parsed = parse_dotenv(contents);
        assert_eq!(parsed[0], ("A".to_string(), "1".to_string()));
        assert_eq!(parsed[1], ("B".to_string(), "two".to_string()));
        assert_eq!(parsed[2], ("C".to_string(), "three".to_string()));
        assert_eq!(parsed[3], ("D".to_string(), "four".to_string()));
    }

    #[test]
    fn product_is_cartesian() {
        let matrix = Matrix::from_elements([
            crate::ast::MatrixElement {
                key: "OS".to_string(),
                value: crate::ast::MatrixRow {
                    ref_: String::new(),
                    value: vec![
                        Value::String("linux".to_string()),
                        Value::String("mac".to_string()),
                    ],
                },
            },
            crate::ast::MatrixElement {
                key: "ARCH".to_string(),
                value: crate::ast::MatrixRow {
                    ref_: String::new(),
                    value: vec![Value::String("amd64".to_string())],
                },
            },
        ]);
        let product = product(&matrix);
        assert_eq!(product.len(), 2);
    }

    /// A directory holding one source, and a task over it running `cmds`.
    fn sourced(cmds: Vec<Cmd>) -> (String, Task) {
        let dir = std::env::temp_dir().join(format!(
            "taskcore-lazy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.txt"), "s").unwrap();
        let dir = dir.to_string_lossy().into_owned();
        let task = Task {
            task: "build".to_string(),
            dirs: vec![dir.clone()],
            sources: vec![Glob {
                glob: "s.txt".to_string(),
                ..Default::default()
            }],
            cmds,
            ..Default::default()
        };
        (dir, task)
    }

    fn cmd(text: &str) -> Cmd {
        Cmd {
            cmd: text.to_string(),
            ..Default::default()
        }
    }

    /// `vars` as the compiler hands them to a task: `CHECKSUM` pending.
    fn pending_vars() -> Vars {
        let mut vars = Vars::new();
        crate::compiler::set_pending_checksum(&mut vars);
        vars
    }

    async fn compile(t: &Task) -> Result<Task, CompileError> {
        let env = Vars::new();
        compiled_task(
            t,
            pending_vars(),
            true,
            &ctx(&env),
            &compiler(),
            &mut silent_logger(),
        )
        .await
    }

    /// A compile never hashes the sources: a command reading CHECKSUM renders
    /// pending, noted for `finalize`, which renders it with the run's value.
    #[tokio::test]
    async fn compile_leaves_checksum_to_the_run() {
        let (_dir, t) = sourced(vec![Cmd {
            cmd: "echo {{.CHECKSUM}}".to_string(),
            if_: "test -n '{{.CHECKSUM}}'".to_string(),
            ..Default::default()
        }]);
        let out = compile(&t).await.unwrap();
        assert!(out.source_hash.is_empty());
        assert!(out.cache_checksum.is_empty());
        assert!(!out.cache_vars_digest.is_empty());
        assert!(out.reads_checksum);
        assert_eq!(out.pending_cmds.len(), 1);
        let env = Vars::new();
        let out = finalized(&t, out, &ctx(&env));
        assert_eq!(out.cmds[0].cmd, format!("echo {HASH}"));
        assert_eq!(out.cmds[0].if_, format!("test -n '{HASH}'"));
        assert!(out.pending_cmds.is_empty());
        assert_eq!(
            out.cache_checksum,
            crate::hash::join_cache_checksum(HASH, &out.cache_vars_digest)
        );

        let (_dir, t) = sourced(vec![cmd("echo hi")]);
        let out = compile(&t).await.unwrap();
        assert!(!out.reads_checksum);
        assert!(out.source_hash.is_empty());
    }

    /// Command bodies and conditions, deferred ones, preconditions and the
    /// cache block may read CHECKSUM; everything deciding what the task runs
    /// or reads is refused it.
    #[tokio::test]
    async fn checksum_is_only_for_late_fields() {
        let with = |f: &dyn Fn(&mut Task)| {
            let (_dir, mut t) = sourced(vec![cmd("echo hi")]);
            f(&mut t);
            t
        };
        let var = |v: Var| {
            Some(Vars::from_elements([VarElement {
                key: "C".to_string(),
                value: v,
            }]))
        };
        let late: Vec<(&str, Task)> = vec![
            ("cmd", with(&|t| t.cmds = vec![cmd("echo {{.CHECKSUM}}")])),
            (
                "jinja debug",
                with(&|t| t.cmds = vec![cmd("echo '{{ debug() }}'")]),
            ),
            (
                "deferred",
                with(&|t| {
                    t.cmds.push(Cmd {
                        cmd: "echo {{.CHECKSUM}}".to_string(),
                        defer: true,
                        ..Default::default()
                    })
                }),
            ),
            (
                "precondition",
                with(&|t| {
                    t.preconditions = vec![Precondition {
                        sh: "test {{.CHECKSUM}}".to_string(),
                        msg: String::new(),
                    }]
                }),
            ),
            (
                "cache url",
                with(&|t| {
                    t.cache = Some(crate::ast::Cache {
                        url: "file:///c-{{.CHECKSUM}}.zip".to_string(),
                        ..Default::default()
                    })
                }),
            ),
        ];
        for (what, t) in late {
            let out = compile(&t).await.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(out.reads_checksum, "{what}");
        }
        let structural: Vec<(&str, Task)> = vec![
            (
                "loop over files task",
                with(&|t| {
                    t.cmds = vec![Cmd {
                        task: "x-{{.CHECKSUM}}".to_string(),
                        for_: Some(For {
                            from: "sources".to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]
                }),
            ),
            (
                "dep loop over files vars",
                with(&|t| {
                    t.deps = vec![Dep {
                        task: "x".to_string(),
                        vars: var(string_var("{{.ITEM}}{{.CHECKSUM}}")),
                        for_: Some(For {
                            from: "sources".to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]
                }),
            ),
            (
                "cmd var",
                with(&|t| t.cmds[0].vars = var(string_var("{{.CHECKSUM}}"))),
            ),
            (
                "cmd task",
                with(&|t| {
                    t.cmds = vec![Cmd {
                        task: "x-{{.CHECKSUM}}".to_string(),
                        ..Default::default()
                    }]
                }),
            ),
            (
                "deferred call",
                with(&|t| {
                    t.cmds = vec![Cmd {
                        task: "x-{{.CHECKSUM}}".to_string(),
                        defer: true,
                        ..Default::default()
                    }]
                }),
            ),
            (
                "dep vars",
                with(&|t| {
                    t.deps = vec![Dep {
                        task: "x".to_string(),
                        vars: var(string_var("{{.CHECKSUM}}")),
                        ..Default::default()
                    }]
                }),
            ),
            (
                "setup task",
                with(&|t| {
                    t.setup = vec![Dep {
                        task: "x-{{.CHECKSUM}}".to_string(),
                        ..Default::default()
                    }]
                }),
            ),
            ("label", with(&|t| t.label = "{{.CHECKSUM}}".to_string())),
            // The commands reading it hash the sources while compiling.
            (
                "label beside a reader",
                with(&|t| {
                    t.label = "{{.CHECKSUM}}".to_string();
                    t.cmds = vec![cmd("echo {{.CHECKSUM}}")];
                }),
            ),
            (
                "task if beside a reader",
                with(&|t| {
                    t.if_ = "{{.CHECKSUM}}".to_string();
                    t.cmds = vec![cmd("echo {{.CHECKSUM}}")];
                }),
            ),
            (
                "label debug",
                with(&|t| t.label = "{{ debug() }}".to_string()),
            ),
            (
                "source",
                with(&|t| t.sources[0].glob = "{{.CHECKSUM}}.txt".to_string()),
            ),
            (
                "cmd ref",
                with(&|t| {
                    t.cmds[0].vars = var(Var {
                        ref_: ".CHECKSUM".to_string(),
                        ..Default::default()
                    })
                }),
            ),
        ];
        for (what, mut t) in structural {
            if what == "label debug" {
                t.dialect = Dialect::Jinja;
            }
            let err = compile(&t).await.expect_err(what);
            assert!(
                matches!(
                    err,
                    CompileError::Template(TemplaterError::ChecksumUnavailable { .. })
                ),
                "{what}: {err}"
            );
            assert!(
                err.to_string().contains(
                    "CHECKSUM is only available in commands, preconditions and the cache block, \
                     after setup"
                ),
                "{what}: {err}"
            );
        }

        // A captured context leaves the pending value out.
        let t = with(&|t| {
            t.cmds[0].vars = var(Var {
                ref_: ".".to_string(),
                ..Default::default()
            })
        });
        let out = compile(&t).await.unwrap();
        let Some(Value::Mapping(captured)) = out.cmds[0]
            .vars
            .as_ref()
            .and_then(|v| v.get("C"))
            .and_then(|v| v.value.clone())
        else {
            panic!("{:?}", out.cmds[0].vars);
        };
        assert!(
            captured
                .get(Value::String("CHECKSUM".to_string()))
                .is_none()
        );

        // Nothing computes it without sources: a late field renders it empty,
        // and the task's vars leave it unset.
        let t = with(&|t| {
            t.sources.clear();
            t.cmds = vec![cmd("echo [{{.CHECKSUM}}]")];
        });
        let env = Vars::new();
        let out = finalize(&t, compile(&t).await.unwrap(), "", true, &ctx(&env)).unwrap();
        assert_eq!(out.cmds[0].cmd, "echo []");
        assert!(out.vars.unwrap().get("CHECKSUM").is_none());
    }

    /// A fast compile never hashes either.
    #[tokio::test]
    async fn fast_compile_does_not_hash() {
        let (_dir, t) = sourced(vec![cmd("echo {{.CHECKSUM}}")]);
        let env = Vars::new();
        let fast = compiled_task(
            &t,
            pending_vars(),
            false,
            &ctx(&env),
            &compiler(),
            &mut silent_logger(),
        )
        .await
        .unwrap();
        assert!(fast.source_hash.is_empty());
    }
}
