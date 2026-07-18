//! Compilation of a task into its fully-templated form.
//!
//! [`compiled_task`] takes a raw task and the variables resolved for it and
//! produces a copy with every templatable field (label, cmds, deps, sources,
//! generates, env, preconditions, cache, …) interpolated. It also evaluates the
//! task's dynamic env vars, reads `dotenv` files, computes the source checksum
//! (`CHECKSUM`), and expands `for` loops over cmds and deps.
//!
//! Ports the templating half of Go `variables.go`. The `resolveGlobsFrom`
//! expansion and the cached-generates-path validation depend on recursively
//! compiling *other* tasks (Executor territory) and are wired through the
//! optional [`TaskResolver`]; when none is provided those steps are skipped.

use std::future::Future;
use std::pin::Pin;

use indexmap::IndexMap;
use serde_yaml_ng::Value;

use crate::ast::{Caches, Dep, For, Glob, Location, Matrix, Precondition, Task, Var, Vars};
use crate::compiler::{Compiler, CompilerError};
use crate::env;
use crate::execext;
use crate::filepathext;
use crate::fingerprint::{self, ChecksumChecker};
use crate::logger::Logger;
use crate::templater::{Cache, TemplaterError};

/// Resolves `from:` entries in `sources`/`generates` by compiling the tasks
/// they reference. The Executor implements this; `compiled_task` calls it to
/// expand wrapper tasks that inherit their children's globs.
pub trait TaskResolver {
    /// Compiles the task named by a dep/cmd call, returning its fully-resolved
    /// form so its `sources`/`generates` can be read. Async, since compilation
    /// may evaluate dynamic (`sh:`) variables; the returned future is boxed so
    /// the trait stays object-safe and the recursion (a wrapper task's `from:`
    /// deps may themselves use `from:`) can be expressed.
    fn compiled_task_for_globs<'a>(
        &'a self,
        task: &'a str,
        vars: &'a Vars,
    ) -> Pin<Box<dyn Future<Output = Result<Task, CompileError>> + 'a>>;
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
    /// The directory used for fingerprint state (`TempDir.Fingerprint`).
    pub fingerprint_temp_dir: &'a str,
    /// Whether task env vars override the inherited process environment.
    pub env_precedence: bool,
    /// The taskfile-level named cache models (`caches:`), for `cache.inherit`.
    pub caches: &'a Caches,
}

/// Compiles `orig_task` against `vars`, returning the fully-templated task.
///
/// When `evaluate_sh_vars` is true, dynamic env vars are resolved (running
/// shells via `compiler`); otherwise they are left for later. Ports Go
/// `compiledTask` (the field-templating body); the caller supplies `vars`
/// already resolved by [`Compiler::get_variables`].
pub async fn compiled_task(
    orig_task: &Task,
    mut vars: Vars,
    evaluate_sh_vars: bool,
    ctx: &CompileContext<'_>,
    compiler: &Compiler,
    logger: &mut Logger,
    resolver: Option<&dyn TaskResolver>,
) -> Result<Task, CompileError> {
    // Substitute captured wildcards from a `MATCH` var into the task name.
    let full_name = compute_full_name(orig_task, &vars);

    let mut cache = Cache::new(vars.clone());
    cache.set_dialect(orig_task.dialect);
    let mut new = Task {
        task: orig_task.task.clone(),
        label: cache.replace(&orig_task.label),
        desc: cache.replace(&orig_task.desc),
        prompt: orig_task.prompt.clone(),
        summary: cache.replace(&orig_task.summary),
        aliases: orig_task.aliases.clone(),
        sources: cache.replace_globs(&orig_task.sources),
        generates: cache.replace_globs(&orig_task.generates),
        dirs: cache.replace_vec(&orig_task.dirs),
        set: orig_task.set.clone(),
        shopt: orig_task.shopt.clone(),
        vars: Some(vars.clone()),
        env: None,
        dotenv: cache.replace_vec(&orig_task.dotenv),
        silent: orig_task.silent,
        interactive: orig_task.interactive,
        internal: orig_task.internal,
        prefix: cache.replace(&orig_task.prefix),
        ignore_error: orig_task.ignore_error,
        run: cache.replace(&orig_task.run),
        include_vars: orig_task.include_vars.clone(),
        included_taskfile_vars: orig_task.included_taskfile_vars.clone(),
        raw_cmds: orig_task.cmds.clone(),
        cache: None, // resolved below after CHECKSUM is available
        platforms: orig_task.platforms.clone(),
        if_: cache.replace(&orig_task.if_),
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
    if let Some(replaced) = cache.replace_vars(ctx.taskfile_env) {
        env_vars.merge(&replaced, None);
    }
    if let Some(replaced) = cache.replace_vars(&dotenv_envs) {
        env_vars.merge(&replaced, None);
    }
    if let Some(orig_env) = &orig_task.env
        && let Some(replaced) = cache.replace_vars(orig_env)
    {
        env_vars.merge(&replaced, None);
    }

    // Resolve dynamic env vars now if requested.
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

    // Compute the source checksum from the unresolved commands, so CHECKSUM is
    // available before cmd/cache resolution. Setting CHECKSUM changes the
    // variable set, so the cache is rebuilt afterwards.
    if !orig_task.sources.is_empty() {
        let mut checker = ChecksumChecker::new(ctx.fingerprint_temp_dir, new.clone());
        let source_hash = checker.source_value().to_string();
        new.source_hash = source_hash.clone();
        vars.set(
            "CHECKSUM".to_string(),
            Var {
                value: Some(Value::String(source_hash)),
                ..Default::default()
            },
        );
        new.vars = Some(vars.clone());
        cache = Cache::new(vars.clone());
        cache.set_dialect(orig_task.dialect);
    }

    // Resolve commands, expanding `for` loops and copying deferred commands.
    if !orig_task.cmds.is_empty() {
        let mut cmds = Vec::with_capacity(orig_task.cmds.len());
        for cmd in &orig_task.cmds {
            if let Some(for_) = &cmd.for_ {
                let (list, keys) = items_from_for(
                    for_,
                    &new.compute_dir().to_string_lossy(),
                    &new.sources,
                    &new.generates,
                    Some(&vars),
                    orig_task.location.as_ref(),
                    &mut cache,
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
                    new_cmd.cmd = cache.replace_with_extra(&cmd.cmd, &extra);
                    new_cmd.task = cache.replace_with_extra(&cmd.task, &extra);
                    new_cmd.if_ = cache.replace_with_extra(&cmd.if_, &extra);
                    new_cmd.vars = cache
                        .replace_vars_with_extra(cmd.vars.as_ref().unwrap_or(&Vars::new()), &extra);
                    cmds.push(new_cmd);
                }
                continue;
            }
            if cmd.defer {
                // Deferred commands are templated lazily so EXIT_CODE is known.
                cmds.push(cmd.clone());
                continue;
            }
            let mut new_cmd = cmd.clone();
            new_cmd.cmd = cache.replace(&cmd.cmd);
            new_cmd.task = cache.replace(&cmd.task);
            new_cmd.if_ = cache.replace(&cmd.if_);
            new_cmd.vars = cmd.vars.as_ref().and_then(|v| cache.replace_vars(v));
            cmds.push(new_cmd);
        }
        new.cmds = cmds;
    }

    new.setup = compile_deps(
        &orig_task.setup,
        &new,
        &vars,
        orig_task.location.as_ref(),
        &mut cache,
    )?;
    new.deps = compile_deps(
        &orig_task.deps,
        &new,
        &vars,
        orig_task.location.as_ref(),
        &mut cache,
    )?;

    // Expand `from:` entries in sources/generates via the resolver, recomputing
    // the checksum if sources were extended.
    if let Some(resolver) = resolver {
        resolve_globs_from(&mut new, GlobField::Sources, resolver).await?;
        resolve_globs_from(&mut new, GlobField::Generates, resolver).await?;
        if has_from_entries(&orig_task.sources) && !new.sources.is_empty() {
            let mut checker = ChecksumChecker::new(ctx.fingerprint_temp_dir, new.clone());
            let source_hash = checker.source_value().to_string();
            new.source_hash = source_hash.clone();
            vars.set(
                "CHECKSUM".to_string(),
                Var {
                    value: Some(Value::String(source_hash)),
                    ..Default::default()
                },
            );
            new.vars = Some(vars.clone());
            cache = Cache::new(vars.clone());
        }
    }
    // TODO(port): executor wiring — when no resolver is supplied, `from:`
    // entries in sources/generates are left unexpanded.

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
        let mut resolved = orig_cache.clone();
        if !resolved.inherit.is_empty()
            && let Some(model) = ctx.caches.0.get(&resolved.inherit)
        {
            let mut merged = model.clone();
            if !resolved.url.is_empty() {
                merged.url = resolved.url.clone();
            }
            if !resolved.lock.is_empty() {
                merged.lock = resolved.lock.clone();
            }
            if !resolved.if_.is_empty() {
                merged.if_ = resolved.if_.clone();
            }
            if resolved.enabled.is_some() {
                merged.enabled = resolved.enabled;
            }
            if !resolved.lock_timeout.is_empty() {
                merged.lock_timeout = resolved.lock_timeout.clone();
            }
            resolved = merged;
        }
        resolved.inherit = String::new();
        resolved.url = cache.replace(&resolved.url);
        resolved.lock = cache.replace(&resolved.lock);
        resolved.if_ = cache.replace(&resolved.if_);
        resolved.lock_timeout = cache.replace(&resolved.lock_timeout);
        new.cache = Some(resolved);
    }

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

/// Reports whether any glob in the slice carries a `from:` directive.
fn has_from_entries(globs: &[Glob]) -> bool {
    globs.iter().any(|g| !g.from.is_empty())
}

/// Selects which glob list of a task `resolve_globs_from` operates on.
#[derive(Clone, Copy)]
enum GlobField {
    Sources,
    Generates,
}

impl GlobField {
    fn name(self) -> &'static str {
        match self {
            Self::Sources => "sources",
            Self::Generates => "generates",
        }
    }
}

/// Expands `from: deps` / `from: cmds` entries of a task's sources or generates
/// into the same field of the referenced tasks, made absolute against each
/// child's directory. Ports Go `resolveGlobsFrom`.
async fn resolve_globs_from(
    task: &mut Task,
    field: GlobField,
    resolver: &dyn TaskResolver,
) -> Result<(), CompileError> {
    let globs = match field {
        GlobField::Sources => task.sources.clone(),
        GlobField::Generates => task.generates.clone(),
    };
    if !has_from_entries(&globs) {
        return Ok(());
    }

    let mut resolved: Vec<Glob> = Vec::with_capacity(globs.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for g in &globs {
        if g.from.is_empty() {
            dedup_add(g.clone(), &mut resolved, &mut seen);
            continue;
        }
        // The tasks whose same field is folded in, as (name, vars) pairs.
        let calls: Vec<(String, Vars)> = match g.from.as_str() {
            "deps" => task
                .deps
                .iter()
                .map(|d| (d.task.clone(), d.vars.clone().unwrap_or_default()))
                .collect(),
            "cmds" => task
                .cmds
                .iter()
                .filter(|c| !c.task.is_empty())
                .map(|c| (c.task.clone(), c.vars.clone().unwrap_or_default()))
                .collect(),
            other => {
                return Err(CompileError::UnsupportedFrom {
                    task: task.task.clone(),
                    field: field.name().to_string(),
                    from: other.to_string(),
                });
            }
        };
        for (name, vars) in calls {
            let ct = resolver.compiled_task_for_globs(&name, &vars).await?;
            // Make each child glob absolute against the child's dir, so it
            // resolves correctly when the parent (possibly a different dir)
            // computes checksums.
            let child_dir = ct.compute_dir().to_string_lossy().into_owned();
            let child_globs = match field {
                GlobField::Sources => &ct.sources,
                GlobField::Generates => &ct.generates,
            };
            for cg in child_globs {
                if !cg.from.is_empty() {
                    continue;
                }
                let mut abs = Glob {
                    glob: filepathext::smart_join(&child_dir, &cg.glob)
                        .to_string_lossy()
                        .into_owned(),
                    negate: cg.negate,
                    ..Default::default()
                };
                if !cg.fingerprint.is_empty() {
                    abs.fingerprint = filepathext::smart_join(&child_dir, &cg.fingerprint)
                        .to_string_lossy()
                        .into_owned();
                }
                dedup_add(abs, &mut resolved, &mut seen);
            }
        }
    }

    match field {
        GlobField::Sources => task.sources = resolved,
        GlobField::Generates => task.generates = resolved,
    }
    Ok(())
}

/// Appends `g` to `resolved` unless an equal glob (same pattern, negation, and
/// fingerprint) was already present.
fn dedup_add(g: Glob, resolved: &mut Vec<Glob>, seen: &mut std::collections::HashSet<String>) {
    let mut key = g.glob.clone();
    if g.negate {
        key = format!("!{key}");
    }
    if !g.fingerprint.is_empty() {
        key.push('\u{0}');
        key.push_str("fp:");
        key.push_str(&g.fingerprint);
    }
    if seen.insert(key) {
        resolved.push(g);
    }
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
                    other => serde_yaml_ng::to_string(other)
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
fn product(matrix: &Matrix) -> Vec<serde_yaml_ng::Mapping> {
    if matrix.is_empty() {
        return Vec::new();
    }
    let mut result: Vec<serde_yaml_ng::Mapping> = vec![serde_yaml_ng::Mapping::new()];
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
/// and `deps`). Ports Go `compileDeps`.
fn compile_deps(
    deps: &[Dep],
    task: &Task,
    vars: &Vars,
    location: Option<&Location>,
    cache: &mut Cache,
) -> Result<Vec<Dep>, CompileError> {
    if deps.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::with_capacity(deps.len());
    for dep in deps {
        if let Some(for_) = &dep.for_ {
            let (list, keys) = items_from_for(
                for_,
                &task.compute_dir().to_string_lossy(),
                &task.sources,
                &task.generates,
                Some(vars),
                location,
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
                new_dep.vars = cache
                    .replace_vars_with_extra(dep.vars.as_ref().unwrap_or(&Vars::new()), &extra);
                result.push(new_dep);
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
    use super::*;
    use crate::ast::{Cmd, For, Var, VarElement, Vars};
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

    fn ctx<'a>(taskfile_env: &'a Vars, tmp: &'a str) -> CompileContext<'a> {
        static EMPTY: std::sync::LazyLock<crate::ast::Caches> =
            std::sync::LazyLock::new(crate::ast::Caches::default);
        CompileContext {
            dir: "",
            taskfile_env,
            fingerprint_temp_dir: tmp,
            env_precedence: false,
            caches: &EMPTY,
        }
    }

    fn string_var(v: &str) -> Var {
        Var {
            value: Some(Value::String(v.to_string())),
            ..Default::default()
        }
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
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let out = compiled_task(&orig, vars, true, &ctx(&env, &tmp), &c, &mut logger, None)
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
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let out = compiled_task(
            &orig,
            Vars::new(),
            true,
            &ctx(&env, &tmp),
            &c,
            &mut logger,
            None,
        )
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
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let out = compiled_task(
            &orig,
            Vars::new(),
            true,
            &ctx(&taskfile_env, &tmp),
            &c,
            &mut logger,
            None,
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
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let out = compiled_task(
            &orig,
            Vars::new(),
            true,
            &ctx(&env, &tmp),
            &c,
            &mut logger,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            out.env.unwrap().get("DYN").and_then(|v| v.value.clone()),
            Some(Value::String("from-shell".to_string()))
        );
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
}
