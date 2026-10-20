//! What a `task:` or `from:` entry of `sources`, `generates` or
//! `sandbox.writes` reaches.
//!
//! A *state* is a call: the task it resolves to, its wildcard captures and the
//! call's vars. A `task:` entry reaches its state in *run* mode in `sources`
//! and `sandbox.writes`: the state's own entries, and those of every state its
//! `setup`, `deps` and `task:` commands (deferred ones included) reach in turn.
//! In `generates` it reaches it in *field* mode: the state's entries only,
//! with the references among them followed. `from: deps` and `from: cmds`
//! reach the direct targets in field mode.
//!
//! Each reached state is compiled once per full compile of the referrer, as a
//! run would compile it up to its `setup` and `deps`, its references left
//! unresolved ([`crate::variables::reached_task`]). The walk is a depth-first
//! preorder: entries in declaration order with a reference expanded in place,
//! then in run mode `setup`, `deps` and `cmds`. A state already expanded adds
//! nothing, so cycles end and the order is deterministic. Discovery is
//! conservative: `if:`, `requires`, prompts and command `platforms:` are
//! ignored; only a task-level `platforms:` excluding the host prunes a state
//! reached by running it.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{Cmd, Glob, Grant, GrantKind, Task, Var, Vars};
use crate::filepathext;
use crate::templater::{Cache, TemplaterError};
use crate::variables::{CompileError, DeferredCall, ReachedTask, ReferenceLimit, TaskResolver};
use crate::yaml::Value;

/// Maximum states of one task reached per walk.
pub const MAX_CALLS_PER_TASK: usize = 256;
/// Maximum states reached per walk, matching the runtime call limit
/// ([`crate::executor::MAXIMUM_TASK_CALL`]).
pub const MAX_CALLS: usize = 1000;

/// The list a walk builds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sources,
    Generates,
    Writes,
}

impl Kind {
    /// The field the errors name.
    fn label(self) -> &'static str {
        match self {
            Self::Sources => "sources",
            Self::Generates => "generates",
            Self::Writes => crate::variables::SANDBOX_WRITES,
        }
    }

    /// The glob field of each state the walk reads.
    fn field(self) -> &'static str {
        match self {
            Self::Sources => "sources",
            Self::Generates | Self::Writes => "generates",
        }
    }

    fn globs(self, t: &Task) -> &[Glob] {
        match self {
            Self::Sources => &t.sources,
            Self::Generates | Self::Writes => &t.generates,
        }
    }
}

/// How a state is reached.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Its entries and what running it runs.
    Run,
    /// Its entries only.
    Field,
}

/// One step from the referrer to a reached call, for messages.
struct Hop {
    /// How it was reached (`{task: x}`, `deps "y"`).
    label: String,
    /// The call, with its vars (`x N=3`).
    call: String,
    up: Chain,
}

type Chain = Option<Rc<Hop>>;

fn hop(up: &Chain, label: String, name: &str, vars: &Vars) -> Chain {
    Some(Rc::new(Hop {
        label,
        call: show_call(name, vars),
        up: up.clone(),
    }))
}

/// The hop labels, outermost first.
fn labels(chain: &Chain) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = chain.as_deref();
    while let Some(h) = at {
        out.push(h.label.clone());
        at = h.up.as_deref();
    }
    out.reverse();
    out
}

/// The calls, innermost first.
fn calls(chain: &Chain) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = chain.as_deref();
    while let Some(h) = at {
        out.push(h.call.clone());
        at = h.up.as_deref();
    }
    out
}

/// `name` with its static scalar vars (`x N=3`).
fn show_call(name: &str, vars: &Vars) -> String {
    let mut out = name.to_string();
    for (key, var) in vars.all() {
        let value = match &var.value {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            _ => "…".to_string(),
        };
        let value = match value.char_indices().nth(24) {
            Some((end, _)) => format!("{}…", value.get(..end).unwrap_or_default()),
            None => value,
        };
        out.push_str(&format!(" {key}={value}"));
    }
    out
}

/// A call to reach.
struct Visit {
    name: String,
    vars: Vars,
    mode: Mode,
    /// Reached by running its caller: a task-level `platforms:` excluding
    /// the host prunes it.
    by_run: bool,
    /// The hops to it, it included.
    chain: Chain,
}

/// The group a reached state's literal entries share, numbered when the
/// first of them is emitted; 0 until then.
type Group = Rc<std::cell::Cell<u32>>;

enum Item {
    /// An entry, with the group to number it in (`None`: as is).
    Emit(Glob, Option<Group>),
    Grant(Grant),
    Visit(Visit),
}

/// The visited states and counts of one walk.
struct Walk {
    kind: Kind,
    run: HashSet<Vec<u8>>,
    field: HashSet<Vec<u8>>,
    per_task: HashMap<String, usize>,
    total: usize,
    next_group: u32,
}

impl Walk {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            run: HashSet::new(),
            field: HashSet::new(),
            per_task: HashMap::new(),
            total: 0,
            next_group: 1,
        }
    }
}

/// Resolves the references of one task's full compile, compiling each
/// reached state once for all of its walks.
pub(crate) struct Reach<'a> {
    resolver: &'a dyn TaskResolver,
    /// The referrer, as errors name it.
    referrer: String,
    memo: HashMap<Vec<u8>, Rc<ReachedTask>>,
    /// The referrer's own state, whose entries are its own.
    root: Option<Vec<u8>>,
    /// How many states were compiled, for tests.
    pub(crate) compiles: usize,
}

impl<'a> Reach<'a> {
    pub(crate) fn new(resolver: &'a dyn TaskResolver, referrer: &Task) -> Self {
        let root = resolver.root().and_then(|(name, vars)| {
            let resolved = resolver.resolve(name).ok()?;
            crate::hash::call_key(&resolved.task, &resolved.captures, vars).ok()
        });
        Self {
            resolver,
            referrer: referrer.name().to_string(),
            memo: HashMap::new(),
            root,
            compiles: 0,
        }
    }

    /// Resolves the `sources` (`generates` when `generates`) of `task`, the
    /// referrer: its own entries as declared (group 0), each reached state's
    /// made absolute, in a group of its own.
    pub(crate) async fn globs(
        &mut self,
        task: &Task,
        raw: &Task,
        generates: bool,
    ) -> Result<Vec<Glob>, CompileError> {
        let kind = if generates {
            Kind::Generates
        } else {
            Kind::Sources
        };
        let mut walk = Walk::new(kind);
        if let Some(root) = &self.root {
            walk.field.insert(root.clone());
        }
        let mut stack = Vec::new();
        self.push_entries(&mut walk, &mut stack, task, raw, &None, true)
            .map_err(|e| self.reached_error(&walk, &None, None, e))?;
        let mut out = Vec::new();
        self.drain(&mut walk, stack, |item| {
            if let Item::Emit(g, _) = item {
                out.push(g);
            }
        })
        .await?;
        Ok(crate::variables::finish_groups(out))
    }

    /// What each `sandbox.writes` task reference of `task` grants: the
    /// generates (field mode) of every state in its run closure, their
    /// `sandbox.writes` directories and dotenv files. One walk per reference.
    pub(crate) async fn writes(
        &mut self,
        task: &Task,
    ) -> Result<Vec<(String, Vec<Grant>)>, CompileError> {
        let refs = task
            .sandbox
            .as_ref()
            .map(|s| s.write_tasks.clone())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(refs.len());
        for r in refs {
            let vars = r.vars.clone().unwrap_or_default();
            let mut walk = Walk::new(Kind::Writes);
            let chain = hop(&None, format!("{{task: {}}}", r.task), &r.task, &vars);
            let stack = vec![Item::Visit(Visit {
                name: r.task.clone(),
                vars,
                mode: Mode::Run,
                by_run: false,
                chain,
            })];
            let mut grants = Vec::new();
            self.drain(&mut walk, stack, |item| {
                if let Item::Grant(g) = item {
                    grants.push(g);
                }
            })
            .await?;
            out.push((r.task.clone(), grants));
        }
        Ok(out)
    }

    /// Processes `stack` to the end, handing each emitted item to `emit`.
    async fn drain(
        &mut self,
        walk: &mut Walk,
        mut stack: Vec<Item>,
        mut emit: impl FnMut(Item),
    ) -> Result<(), CompileError> {
        while let Some(item) = stack.pop() {
            let visit = match item {
                Item::Visit(v) => v,
                Item::Emit(mut g, Some(group)) => {
                    if group.get() == 0 {
                        group.set(walk.next_group);
                        walk.next_group = walk.next_group.saturating_add(1);
                    }
                    g.group = group.get();
                    emit(Item::Emit(g, None));
                    continue;
                }
                other => {
                    emit(other);
                    continue;
                }
            };
            self.visit(walk, &mut stack, visit).await?;
        }
        Ok(())
    }

    /// Reaches one call, pushing what it contributes.
    async fn visit(
        &mut self,
        walk: &mut Walk,
        stack: &mut Vec<Item>,
        v: Visit,
    ) -> Result<(), CompileError> {
        let resolved = self
            .resolver
            .resolve(&v.name)
            .map_err(|e| self.reached_error(walk, &v.chain, Some(&v.name), e))?;
        // Before anything else, as a run checks it before compiling: the
        // task is not compiled, nor counted, and an explicit reference to it
        // still includes it.
        if v.by_run && !crate::executor::should_run_on_current_platform(&resolved.platforms) {
            return Ok(());
        }
        let name = resolved.task;
        let key = crate::hash::call_key(&name, &resolved.captures, &v.vars).map_err(|e| {
            self.reached_error(walk, &v.chain, None, CompileError::FromTask(e.to_string()))
        })?;
        let entries = match v.mode {
            Mode::Field => {
                if walk.run.contains(&key) || walk.field.contains(&key) {
                    return Ok(());
                }
                true
            }
            Mode::Run => {
                if walk.run.contains(&key) {
                    return Ok(());
                }
                !walk.field.contains(&key)
            }
        };
        if !walk.field.contains(&key) && !walk.run.contains(&key) {
            self.count(walk, &name, &v.chain)?;
        }
        match v.mode {
            Mode::Field => walk.field.insert(key.clone()),
            Mode::Run => walk.run.insert(key.clone()),
        };
        let reached = self.compile(walk, &key, &v).await?;
        let t = &reached.task;

        // Its entries first, then in run mode (the writes walk's grants and)
        // its calls, in declaration order.
        let mut items = Vec::new();
        if v.mode == Mode::Run {
            if walk.kind == Kind::Writes {
                write_grants(&mut items, t, &reached.raw, &v.chain)
                    .map_err(|e| self.reached_error(walk, &v.chain, None, e))?;
            }
            self.run_edges(&mut items, t, &v.chain)
                .map_err(|e| self.reached_error(walk, &v.chain, None, e))?;
        }
        let mut own = Vec::new();
        if entries {
            self.push_entries(walk, &mut own, t, &reached.raw, &v.chain, false)
                .map_err(|e| self.reached_error(walk, &v.chain, None, e))?;
        }
        // `own` is in pop order already; `items` in declaration order.
        stack.extend(items.into_iter().rev());
        stack.extend(own);
        Ok(())
    }

    /// Counts a newly reached state against the caps.
    fn count(&self, walk: &mut Walk, name: &str, chain: &Chain) -> Result<(), CompileError> {
        let per_task = walk.per_task.entry(name.to_string()).or_insert(0);
        *per_task = per_task.saturating_add(1);
        walk.total = walk.total.saturating_add(1);
        let over_task = *per_task > MAX_CALLS_PER_TASK;
        if over_task || walk.total > MAX_CALLS {
            return Err(CompileError::ReferenceLimit(Box::new(ReferenceLimit {
                task: self.referrer.clone(),
                field: walk.kind.label().to_string(),
                entry: labels(chain).into_iter().next().unwrap_or_default(),
                name: over_task.then(|| name.to_string()),
                limit: if over_task {
                    MAX_CALLS_PER_TASK
                } else {
                    MAX_CALLS
                },
                calls: calls(chain),
            })));
        }
        Ok(())
    }

    /// The phase-A compile of a state, memoized.
    async fn compile(
        &mut self,
        walk: &Walk,
        key: &[u8],
        v: &Visit,
    ) -> Result<Rc<ReachedTask>, CompileError> {
        if let Some(t) = self.memo.get(key) {
            return Ok(Rc::clone(t));
        }
        let reached = self
            .resolver
            .compile_call(&v.name, &v.vars)
            .await
            .map_err(|e| self.reached_error(walk, &v.chain, Some(&v.name), e))?;
        self.compiles = self.compiles.saturating_add(1);
        let reached = Rc::new(reached);
        self.memo.insert(key.to_vec(), Rc::clone(&reached));
        Ok(reached)
    }

    /// Pushes, in pop order onto `out`, the entries of `t`'s field: literal
    /// ones emitted (absolute unless `root`, in a group of their own), the
    /// references among them reached.
    fn push_entries(
        &self,
        walk: &mut Walk,
        out: &mut Vec<Item>,
        t: &Task,
        raw: &Task,
        chain: &Chain,
        root: bool,
    ) -> Result<(), CompileError> {
        let kind = walk.kind;
        let globs = kind.globs(t);
        let raw_globs = kind.globs(raw);
        let dir = t.compute_dir().to_string_lossy().into_owned();
        // Numbered when its first entry is emitted: after the states its
        // references before it emitted theirs.
        let group: Group = Rc::default();
        // A reached state's references are named by its field.
        let prefix = if root {
            String::new()
        } else {
            format!("{} ", kind.field())
        };
        let mut items = Vec::new();
        for (i, g) in globs.iter().enumerate() {
            if !g.is_reference() {
                // A `task:` entry whose name rendered empty reads as a glob.
                if let Some(r) = raw_globs.get(i).filter(|r| !r.task.is_empty()) {
                    return Err(CompileError::EmptyTaskReference {
                        task: t.name().to_string(),
                        field: kind.field().to_string(),
                        template: r.task.clone(),
                    });
                }
                if root {
                    items.push(Item::Emit(g.clone(), None));
                } else {
                    items.push(self.literal(kind, g, &dir, &group, chain));
                }
                continue;
            }
            if !g.task.is_empty() {
                let vars = g.vars.clone().unwrap_or_default();
                let mode = if kind == Kind::Sources {
                    Mode::Run
                } else {
                    Mode::Field
                };
                let label = format!("{prefix}{{task: {}}}", g.task);
                items.push(Item::Visit(Visit {
                    chain: hop(chain, label, &g.task, &vars),
                    name: g.task.clone(),
                    vars,
                    mode,
                    by_run: false,
                }));
                continue;
            }
            let targets = match g.from.as_str() {
                "deps" => t
                    .deps
                    .iter()
                    .map(|d| (d.task.clone(), d.vars.clone().unwrap_or_default()))
                    .collect(),
                "cmds" => cmd_calls(t)?,
                other => {
                    return Err(CompileError::UnsupportedFrom {
                        task: t.task.clone(),
                        field: kind.field().to_string(),
                        from: other.to_string(),
                    });
                }
            };
            for (name, vars) in targets {
                let label = format!("{prefix}from: {} {name:?}", g.from);
                items.push(Item::Visit(Visit {
                    chain: hop(chain, label, &name, &vars),
                    name,
                    vars,
                    mode: Mode::Field,
                    by_run: false,
                }));
            }
        }
        out.extend(items.into_iter().rev());
        Ok(())
    }

    /// A reached state's literal entry, absolute against its directory: a
    /// glob for the glob walks, a grant for the writes walk.
    fn literal(&self, kind: Kind, g: &Glob, dir: &str, group: &Group, chain: &Chain) -> Item {
        let join = |p: &str| {
            filepathext::smart_join(dir, p)
                .to_string_lossy()
                .into_owned()
        };
        let mut abs = Glob {
            glob: filepathext::join_glob(dir, &g.glob),
            negate: g.negate,
            ..Default::default()
        };
        if !g.fingerprint.is_empty() {
            abs.fingerprint = join(&g.fingerprint);
        }
        if kind != Kind::Writes {
            return Item::Emit(abs, Some(Rc::clone(group)));
        }
        // An exclusion, or an empty pattern, grants nothing.
        let grants = !g.negate && !g.glob.is_empty();
        Item::Grant(Grant {
            what: format!(
                "{} {}",
                crate::variables::SANDBOX_WRITES,
                labels(chain).join(" → ")
            ),
            declared: g.glob.clone(),
            path: if grants { join(&g.glob) } else { String::new() },
            kind: GrantKind::Generates,
        })
    }

    /// The calls running `t` makes, in declaration order: `setup`, `deps`,
    /// then `task:` commands (deferred ones at their position).
    fn run_edges(
        &self,
        items: &mut Vec<Item>,
        t: &Task,
        chain: &Chain,
    ) -> Result<(), CompileError> {
        let deps = t
            .setup
            .iter()
            .map(|d| ("setup", d))
            .chain(t.deps.iter().map(|d| ("deps", d)));
        let mut edges: Vec<(&str, String, Vars)> = deps
            .map(|(how, d)| (how, d.task.clone(), d.vars.clone().unwrap_or_default()))
            .collect();
        edges.extend(
            cmd_calls(t)?
                .into_iter()
                .map(|(name, vars)| ("cmds", name, vars)),
        );
        for (how, name, vars) in edges {
            items.push(Item::Visit(Visit {
                chain: hop(chain, format!("{how} {name:?}"), &name, &vars),
                name,
                vars,
                mode: Mode::Run,
                by_run: true,
            }));
        }
        Ok(())
    }

    /// Names the referrer, field and chain on an error found in a reached
    /// state (`chain` set) or in the referrer's own entries; a missing task,
    /// the caps and an unsupported deferred call stay typed.
    fn reached_error(
        &self,
        walk: &Walk,
        chain: &Chain,
        name: Option<&str>,
        err: CompileError,
    ) -> CompileError {
        let field = walk.kind.label().to_string();
        match err {
            CompileError::DeferredCallUnsupported(mut e) if e.task.is_empty() => {
                e.task = self.referrer.clone();
                e.field = field;
                e.via = labels(chain);
                CompileError::DeferredCallUnsupported(e)
            }
            CompileError::TaskNameConflict {
                call, task_names, ..
            } => CompileError::TaskNameConflict {
                task: self.referrer.clone(),
                field,
                via: labels(chain),
                call,
                task_names,
            },
            // The referrer's own entries: as found.
            other if chain.is_none() => other,
            CompileError::TaskNotFound { name: missing } => CompileError::ReferencedTaskNotFound {
                task: self.referrer.clone(),
                field,
                name: name.map_or(missing, str::to_string),
                via: labels(chain),
            },
            // Already named from the referrer.
            e @ (CompileError::ReferenceLimit(_)
            | CompileError::DeferredCallUnsupported(_)
            | CompileError::ReferencedTaskNotFound { .. }) => e,
            other => CompileError::FromTask(format!(
                "task: {}: {field}: {}: {other}",
                self.referrer,
                labels(chain).join(" → ")
            )),
        }
    }
}

/// The `sandbox.writes` directories and dotenv files of a state reached in
/// run mode by the writes walk, then its own `sandbox.writes` references, in
/// declaration order.
fn write_grants(
    items: &mut Vec<Item>,
    t: &Task,
    raw: &Task,
    chain: &Chain,
) -> Result<(), CompileError> {
    let what = format!(
        "{} {}",
        crate::variables::SANDBOX_WRITES,
        labels(chain).join(" → ")
    );
    let dir = t.compute_dir().to_string_lossy().into_owned();
    let join = |p: &str| {
        filepathext::smart_join(&dir, p)
            .to_string_lossy()
            .into_owned()
    };
    if let Some(s) = &t.sandbox {
        for w in &s.writes {
            items.push(Item::Grant(Grant {
                what: what.clone(),
                declared: w.clone(),
                path: join(w),
                kind: GrantKind::Writes,
            }));
        }
    }
    for f in &t.dotenv {
        items.push(Item::Grant(Grant {
            what: what.clone(),
            declared: f.clone(),
            path: join(f),
            kind: GrantKind::Dotenv,
        }));
    }
    let raw_refs = raw.sandbox.as_ref().map_or(&[][..], |s| &s.write_tasks);
    for (i, r) in t.sandbox.iter().flat_map(|s| &s.write_tasks).enumerate() {
        if r.task.is_empty() {
            return Err(CompileError::EmptyTaskReference {
                task: t.name().to_string(),
                field: crate::variables::SANDBOX_WRITES.to_string(),
                template: raw_refs.get(i).map(|r| r.task.clone()).unwrap_or_default(),
            });
        }
        let vars = r.vars.clone().unwrap_or_default();
        let label = format!("{} {{task: {}}}", crate::variables::SANDBOX_WRITES, r.task);
        items.push(Item::Visit(Visit {
            chain: hop(chain, label, &r.task, &vars),
            name: r.task.clone(),
            vars,
            mode: Mode::Run,
            by_run: false,
        }));
    }
    Ok(())
}

/// The `task:` commands of `t`, as `from: cmds` and a run see them: deferred
/// ones rendered as on success; one whose name renders empty runs nothing.
/// The error leaves the referrer and field for the caller to fill in.
pub(crate) fn cmd_calls(t: &Task) -> Result<Vec<(String, Vars)>, CompileError> {
    let mut out = Vec::new();
    for c in &t.cmds {
        let (name, vars) = if c.defer {
            if mentions(c, "EXIT_CODE") {
                return Err(CompileError::DeferredCallUnsupported(Box::new(
                    DeferredCall {
                        task: String::new(),
                        field: String::new(),
                        via: Vec::new(),
                        owner: t.name().to_string(),
                        template: c.task.clone(),
                        var: "EXIT_CODE".to_string(),
                    },
                )));
            }
            deferred_call(t, c).map_err(CompileError::Template)?
        } else {
            (c.task.clone(), c.vars.clone())
        };
        if !name.is_empty() {
            out.push((name, vars.unwrap_or_default()));
        }
    }
    Ok(out)
}

/// Renders a deferred command's `task:` name and vars as `run_deferred` does
/// when the task succeeded: from the task's vars, without `EXIT_CODE`, and
/// with `CHECKSUM` pending, so reading it is an error.
pub(crate) fn deferred_call(t: &Task, c: &Cmd) -> Result<(String, Option<Vars>), TemplaterError> {
    let mut cache = Cache::new(t.vars.clone().unwrap_or_default());
    cache.set_dialect(t.dialect);
    let name = cache.field("cmds").replace(&c.task);
    let vars = c.vars.as_ref().and_then(|v| cache.replace_vars(v));
    match cache.err() {
        Some(e) => Err(e.clone()),
        None => Ok((name, vars)),
    }
}

/// Whether a raw deferred call's name or vars mention `word`.
/// Uses substring matching, so false positives are possible.
fn mentions(c: &Cmd, word: &str) -> bool {
    fn value(v: &Value, word: &str) -> bool {
        match v {
            Value::String(s) => s.contains(word),
            Value::Sequence(seq) => seq.iter().any(|v| value(v, word)),
            Value::Mapping(map) => map.iter().any(|(k, v)| value(k, word) || value(v, word)),
            _ => false,
        }
    }
    let var = |v: &Var| {
        v.sh.as_deref().is_some_and(|s| s.contains(word))
            || v.ref_.contains(word)
            || v.value.as_ref().is_some_and(|v| value(v, word))
    };
    c.task.contains(word) || c.vars.iter().flat_map(Vars::all).any(|(_, v)| var(v))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::pin::Pin;

    use super::*;
    use crate::ast::Dep;
    use crate::variables::ResolvedCall;

    /// Tasks compiled already, counting the compiles asked for.
    struct Tasks {
        tasks: HashMap<String, Task>,
        compiles: Cell<usize>,
        compiled: RefCell<Vec<String>>,
    }

    impl Tasks {
        fn new(tasks: impl IntoIterator<Item = Task>) -> Self {
            Self {
                tasks: tasks.into_iter().map(|t| (t.task.clone(), t)).collect(),
                compiles: Cell::new(0),
                compiled: RefCell::new(Vec::new()),
            }
        }
    }

    impl TaskResolver for Tasks {
        fn resolve(&self, name: &str) -> Result<ResolvedCall, CompileError> {
            match self.tasks.get(name) {
                Some(t) => Ok(ResolvedCall {
                    task: name.to_string(),
                    captures: Vec::new(),
                    platforms: t.platforms.clone(),
                }),
                None => Err(CompileError::TaskNotFound {
                    name: name.to_string(),
                }),
            }
        }

        fn compile_call<'a>(
            &'a self,
            name: &'a str,
            _vars: &'a Vars,
        ) -> Pin<Box<dyn Future<Output = Result<ReachedTask, CompileError>> + 'a>> {
            Box::pin(async move {
                self.compiles.set(self.compiles.get() + 1);
                self.compiled.borrow_mut().push(name.to_string());
                let task = self.tasks.get(name).cloned().unwrap();
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

    fn glob(p: &str) -> Glob {
        Glob {
            glob: p.to_string(),
            ..Default::default()
        }
    }

    fn reference(task: &str) -> Glob {
        Glob {
            task: task.to_string(),
            ..Default::default()
        }
    }

    fn from(what: &str) -> Glob {
        Glob {
            from: what.to_string(),
            ..Default::default()
        }
    }

    fn task(name: &str, sources: &[Glob], deps: &[&str]) -> Task {
        Task {
            task: name.to_string(),
            dirs: vec!["/r".to_string()],
            sources: sources.to_vec(),
            deps: deps
                .iter()
                .map(|d| Dep {
                    task: d.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn globs(list: &[Glob]) -> Vec<&str> {
        list.iter().map(|g| g.glob.as_str()).collect()
    }

    async fn sources(tasks: &Tasks, root: &Task) -> Result<Vec<Glob>, CompileError> {
        Reach::new(tasks, root).globs(root, root, false).await
    }

    /// A task two reached tasks run is compiled and counted once, in
    /// depth-first order.
    #[tokio::test]
    async fn a_diamond_is_compiled_once() {
        let tasks = Tasks::new([
            task("l", &[glob("l")], &["d"]),
            task("r", &[glob("r")], &["d"]),
            task("d", &[glob("d")], &[]),
        ]);
        let root = task("root", &[reference("l"), reference("r")], &[]);
        let out = sources(&tasks, &root).await.unwrap();
        assert_eq!(globs(&out), ["/r/l", "/r/d", "/r/r"]);
        assert_eq!(*tasks.compiled.borrow(), ["l", "d", "r"]);
    }

    /// A task reached for its entries, then for what it runs, or the other
    /// way round: its entries once, what it runs too.
    #[tokio::test]
    async fn field_and_run_mode_combine_in_either_order() {
        let tasks = Tasks::new([
            task("x", &[glob("x")], &["y"]),
            task("y", &[glob("y")], &[]),
        ]);
        for order in [
            vec![from("deps"), reference("x")],
            vec![reference("x"), from("deps")],
        ] {
            let root = task("root", &order, &["x"]);
            let out = sources(&tasks, &root).await.unwrap();
            assert_eq!(globs(&out), ["/r/x", "/r/y"]);
        }
    }

    /// The walks of one compile share compiles, not results: what a task runs
    /// counts in sources, not in generates.
    #[tokio::test]
    async fn walks_share_compiles_only() {
        let mut x = task("x", &[glob("x")], &["y"]);
        x.generates = vec![glob("gx")];
        let mut y = task("y", &[glob("y")], &[]);
        y.generates = vec![glob("gy")];
        let tasks = Tasks::new([x, y]);
        let mut root = task("root", &[reference("x")], &[]);
        root.generates = vec![reference("x")];

        let mut reach = Reach::new(&tasks, &root);
        let generates = reach.globs(&root, &root, true).await.unwrap();
        let shared = reach.globs(&root, &root, false).await.unwrap();
        assert_eq!(globs(&generates), ["/r/gx"]);
        assert_eq!(globs(&shared), ["/r/x", "/r/y"]);
        assert_eq!(tasks.compiles.get(), 2);
        assert_eq!(reach.compiles, 2);

        let alone = Tasks::new(tasks.tasks.values().cloned());
        let separate = sources(&alone, &root).await.unwrap();
        assert_eq!(separate, shared);
    }

    /// A walk reaching more than [`MAX_CALLS`] calls in all fails.
    #[tokio::test]
    async fn too_many_calls_in_all_fail() {
        let names: Vec<String> = (0..=MAX_CALLS).map(|i| format!("t{i}")).collect();
        let tasks = Tasks::new(names.iter().enumerate().map(|(i, n)| {
            let next: Vec<&str> = names.get(i + 1).map(String::as_str).into_iter().collect();
            task(n, &[], &next)
        }));
        let root = task("root", &[reference("t0")], &[]);
        let Err(err) = sources(&tasks, &root).await else {
            panic!("expected the cap");
        };
        assert!(
            matches!(&err, CompileError::ReferenceLimit(e) if e.name.is_none() && e.limit == MAX_CALLS),
            "{err}"
        );
        assert_eq!(tasks.compiles.get(), MAX_CALLS);
        // One fewer is fine.
        let root = task("root", &[reference("t1")], &[]);
        assert!(sources(&tasks, &root).await.is_ok());
    }

    /// A deferred call is rendered as on success; one that needs EXIT_CODE
    /// is refused, and one that reads the pending CHECKSUM fails.
    #[test]
    fn deferred_calls_render_without_the_exit_code() {
        let mut t = Task {
            task: "x".to_string(),
            vars: Some(Vars::from_elements([
                crate::ast::VarElement {
                    key: "T".to_string(),
                    value: Var::from_string("y"),
                },
                crate::ast::VarElement {
                    key: "CHECKSUM".to_string(),
                    value: Var::from_string(crate::templater::PENDING_CHECKSUM),
                },
            ])),
            ..Default::default()
        };
        let call = |name: &str| Cmd {
            task: name.to_string(),
            defer: true,
            ..Default::default()
        };
        t.cmds = vec![call("{{.T}}"), call("{{.NONE}}")];
        assert_eq!(cmd_calls(&t).unwrap(), [("y".to_string(), Vars::new())]);
        t.cmds = vec![call("{{if .EXIT_CODE}}a{{else}}b{{end}}")];
        assert!(matches!(
            cmd_calls(&t),
            Err(CompileError::DeferredCallUnsupported(_))
        ));
        t.cmds = vec![call("c-{{.CHECKSUM}}")];
        assert!(matches!(
            cmd_calls(&t),
            Err(CompileError::Template(
                TemplaterError::ChecksumUnavailable { .. }
            ))
        ));
    }
}
