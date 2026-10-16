//! Stable content hashing of a task, used as its cache identity.
//!
//! A task's hash combines the originating Taskfile and local name with a
//! deterministic digest of the task's meaningful fields. Fields populated
//! during merging or compilation (`task`, `prefix`, `namespace`, `full_name`,
//! `raw_cmds`, `source_hash`) are excluded, so the include path alone does not
//! change the hash. Namespaced aliases and `task:` references still do. The
//! digest is an xxHash3 over a canonical byte encoding: it need not match any
//! external tool, only be stable for a given task and change when a meaningful
//! field changes.

use twox_hash::XxHash3_64;

use crate::ast;

/// An error raised when a task cannot be hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HashError {
    /// A variable value could not be canonically serialized.
    Serialize(String),
}

impl std::fmt::Display for HashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HashError::Serialize(msg) => write!(f, "hash: failed to serialize value: {msg}"),
        }
    }
}

impl std::error::Error for HashError {}

/// Returns an empty hash, used when hashing is disabled for a task.
pub fn empty(_task: &ast::Task) -> Result<String, HashError> {
    Ok(String::new())
}

/// Returns the task's identity as `<taskfile>:<local name>`.
pub fn name(task: &ast::Task) -> Result<String, HashError> {
    let taskfile = task
        .location
        .as_ref()
        .map(|l| l.taskfile.as_str())
        .unwrap_or_default();
    Ok(format!("{}:{}", taskfile, task.local_name()))
}

/// Returns the cache entry and build-once lock checksum: `source_hash`, `-`,
/// then a digest of raw task and include `vars`. The source hash uses raw
/// commands, so the digest separates tasks whose commands render differently:
/// one Taskfile included twice with different `vars`, or two Taskfiles whose
/// `build` tasks differ only in `vars`. `task` must be uncompiled so its `vars`
/// are still declarations. Returns empty when `source_hash` is empty: a task
/// without sources has no cache identity.
pub fn cache_checksum(task: &ast::Task, source_hash: &str) -> Result<String, HashError> {
    if source_hash.is_empty() {
        return Ok(String::new());
    }
    let mut enc = Encoder::new();
    encode_vars_flat(&mut enc, task.vars.as_ref())?;
    encode_vars_flat(&mut enc, task.include_vars.as_ref())?;
    let hex = format!("{:016x}", XxHash3_64::oneshot(&enc.finish()));
    Ok(format!("{source_hash}-{hex}"))
}

/// The key of a call a `task:` or `from:` reference reaches: the task it
/// resolves to, its wildcard captures, then the call's vars in declaration
/// order, each with its dialect, which the callee renders it in. No vars and
/// an empty map are the same call. Never persisted.
pub(crate) fn call_key(
    task: &str,
    captures: &[String],
    vars: &ast::Vars,
) -> Result<Vec<u8>, HashError> {
    let mut enc = Encoder::new();
    enc.str(task);
    encode_str_slice(&mut enc, captures);
    enc.len(vars.len());
    for (key, var) in vars.all() {
        enc.str(key);
        encode_var(&mut enc, var)?;
        enc.tag(match var.dialect {
            ast::Dialect::Go => 0,
            ast::Dialect::Jinja => 1,
        });
    }
    Ok(enc.finish())
}

/// Encodes `vars` as a length-prefixed list. `None` and an empty map encode
/// identically so a root task without include vars matches an included copy
/// with an empty map.
fn encode_vars_flat(enc: &mut Encoder, vars: Option<&ast::Vars>) -> Result<(), HashError> {
    let vars = vars.filter(|v| !v.is_empty());
    enc.len(vars.map_or(0, ast::Vars::len));
    for (key, var) in vars.into_iter().flat_map(ast::Vars::all) {
        enc.str(key);
        encode_var(enc, var)?;
    }
    Ok(())
}

/// Returns the task's content hash as `<taskfile>:<local name>:<digest>`.
///
/// The digest is stable for a given task and changes when any meaningful field
/// changes.
pub fn hash(task: &ast::Task) -> Result<String, HashError> {
    let mut enc = Encoder::new();
    encode_task(&mut enc, task)?;

    let hex = format!("{:016x}", XxHash3_64::oneshot(&enc.finish()));

    let taskfile = task
        .location
        .as_ref()
        .map(|l| l.taskfile.as_str())
        .unwrap_or_default();
    Ok(format!("{}:{}:{}", taskfile, task.local_name(), hex))
}

/// Persistent identity for a compiled task's checksum file. Compilation puts
/// the project root first in `dirs`. Resolve structural paths relative to the
/// project or task directory so exported fingerprints survive relocation.
/// Explicit values in commands and env remain significant, even when they
/// contain absolute paths.
pub fn fingerprint(task: &ast::Task) -> Result<String, HashError> {
    let root = task.dirs.first().map(String::as_str).unwrap_or_default();
    let dir = task.compute_dir();
    let dir = dir.to_string_lossy();
    let relative = |base: &str, path: &str| {
        if !base.is_empty() && std::path::Path::new(path).is_absolute() {
            crate::filepathext::rel_str(base, path).unwrap_or_else(|| path.to_string())
        } else {
            path.to_string()
        }
    };
    let mut normalized = task.clone();
    normalized.dirs = vec![relative(root, &dir)];
    if let Some(location) = &mut normalized.location {
        location.taskfile = relative(root, &location.taskfile);
        location.line = 0;
        location.column = 0;
    }
    for glob in normalized
        .sources
        .iter_mut()
        .chain(&mut normalized.generates)
    {
        glob.glob = relative(&dir, &glob.glob);
        glob.fingerprint = relative(&dir, &glob.fingerprint);
    }
    normalized.dotenv = task
        .dotenv
        .iter()
        .map(|path| relative(&dir, path))
        .collect();
    // Aliases name entry points and gain namespace prefixes during merging;
    // they do not change the build. Task references still affect its identity.
    normalized.aliases.clear();
    let mut enc = Encoder::new();
    enc.str(&task.local_name());
    encode_task(&mut enc, &normalized)?;
    // Include declared, compiled env, not the inherited process environment.
    // Variables used only by env must not collapse across include namespaces.
    encode_vars_flat(&mut enc, task.env.as_ref())?;
    Ok(format!("{:016x}", XxHash3_64::oneshot(&enc.finish())))
}

/// A length-prefixed byte encoder producing a canonical, unambiguous
/// serialization. Every scalar is written with its byte length first so that
/// no two distinct field layouts can collide.
struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Encoder { buf: Vec::new() }
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// A single discriminator byte, distinguishing field kinds and options.
    fn tag(&mut self, tag: u8) {
        self.buf.push(tag);
    }

    /// A length-prefixed byte string.
    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(b);
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn bool(&mut self, b: bool) {
        self.buf.push(u8::from(b));
    }

    fn u64(&mut self, n: u64) {
        self.buf.extend_from_slice(&n.to_le_bytes());
    }

    /// A sequence length marker, written before its elements.
    fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }

    fn opt_bool(&mut self, b: Option<bool>) {
        match b {
            None => self.tag(0),
            Some(v) => {
                self.tag(1);
                self.bool(v);
            }
        }
    }
}

fn encode_task(enc: &mut Encoder, t: &ast::Task) -> Result<(), HashError> {
    encode_deps(enc, &t.setup)?;
    encode_cmds(enc, &t.cmds)?;
    encode_deps(enc, &t.deps)?;
    enc.str(&t.label);
    enc.str(&t.desc);
    encode_str_slice(enc, &t.prompt.0);
    enc.str(&t.summary);
    encode_requires(enc, t.requires.as_ref());
    encode_str_slice(enc, &t.aliases);
    encode_globs(enc, &t.sources)?;
    encode_globs(enc, &t.generates)?;
    encode_cache(enc, t.cache.as_ref());
    encode_preconditions(enc, &t.preconditions);
    encode_str_slice(enc, &t.dirs);
    encode_str_slice(enc, &t.set);
    encode_str_slice(enc, &t.shopt);
    // The Vars-typed fields (vars, env, include_vars, included_taskfile_vars)
    // are deliberately NOT hashed: Go's hashstructure does not traverse the
    // orderedmap backing them, so a task's identity reflects its compiled cmds,
    // dir, sources, etc. (which already bake in resolved variable values) but
    // not the raw variables — including the namespace-specific TASK special var.
    // This lets `run: when_changed` dedup identical invocations of the same
    // task reached through different includes.
    encode_str_slice(enc, &t.dotenv);
    enc.opt_bool(t.silent);
    enc.bool(t.interactive);
    enc.bool(t.internal);
    enc.bool(t.ignore_error);
    enc.str(&t.run);
    encode_platforms(enc, &t.platforms);
    enc.str(&t.if_);
    enc.bool(t.watch);
    encode_location(enc, t.location.as_ref());
    enc.bool(t.failfast);
    Ok(())
}

fn encode_str_slice(enc: &mut Encoder, items: &[String]) {
    enc.len(items.len());
    for s in items {
        enc.str(s);
    }
}

fn encode_cmds(enc: &mut Encoder, cmds: &[ast::Cmd]) -> Result<(), HashError> {
    enc.len(cmds.len());
    for c in cmds {
        enc.str(&c.cmd);
        enc.str(&c.task);
        encode_for(enc, c.for_.as_ref())?;
        enc.str(&c.if_);
        enc.bool(c.silent);
        encode_str_slice(enc, &c.set);
        encode_str_slice(enc, &c.shopt);
        encode_opt_vars(enc, c.vars.as_ref())?;
        enc.bool(c.ignore_error);
        enc.bool(c.defer);
        encode_platforms(enc, &c.platforms);
    }
    Ok(())
}

fn encode_deps(enc: &mut Encoder, deps: &[ast::Dep]) -> Result<(), HashError> {
    enc.len(deps.len());
    for d in deps {
        enc.str(&d.task);
        encode_for(enc, d.for_.as_ref())?;
        encode_opt_vars(enc, d.vars.as_ref())?;
        enc.bool(d.silent);
        enc.opt_bool(d.fingerprint);
    }
    Ok(())
}

fn encode_for(enc: &mut Encoder, for_: Option<&ast::For>) -> Result<(), HashError> {
    match for_ {
        None => enc.tag(0),
        Some(f) => {
            enc.tag(1);
            enc.str(&f.from);
            enc.len(f.list.len());
            for v in &f.list {
                encode_value(enc, v)?;
            }
            encode_matrix(enc, f.matrix.as_ref())?;
            enc.str(&f.var);
            enc.str(&f.split);
            enc.str(&f.as_);
        }
    }
    Ok(())
}

fn encode_matrix(enc: &mut Encoder, matrix: Option<&ast::Matrix>) -> Result<(), HashError> {
    match matrix {
        None => enc.tag(0),
        Some(m) => {
            enc.tag(1);
            enc.len(m.len());
            for (key, row) in m.all() {
                enc.str(key);
                enc.str(&row.ref_);
                enc.len(row.value.len());
                for v in &row.value {
                    encode_value(enc, v)?;
                }
            }
        }
    }
    Ok(())
}

fn encode_globs(enc: &mut Encoder, globs: &[ast::Glob]) -> Result<(), HashError> {
    enc.len(globs.len());
    for g in globs {
        enc.str(&g.glob);
        enc.bool(g.negate);
        enc.str(&g.fingerprint);
        if g.group != 0 {
            // Only in a list whose groups matter (a folded task's exclusions
            // apply to its own entries), so other lists hash as before.
            enc.str("\0group");
            enc.u64(u64::from(g.group));
        } else if g.task.is_empty() {
            enc.str(&g.from);
        } else {
            // In the `from` slot, so that the encoding of other entries, and
            // with it every existing hash, is unchanged. No valid `from` holds
            // a NUL.
            enc.str("\0task");
            enc.str(&g.task);
            // No `vars:` and `vars: {}` compile alike.
            encode_vars_flat(enc, g.vars.as_ref())?;
        }
    }
    Ok(())
}

fn encode_platforms(enc: &mut Encoder, platforms: &[ast::Platform]) {
    enc.len(platforms.len());
    for p in platforms {
        enc.str(&p.os);
        enc.str(&p.arch);
    }
}

fn encode_preconditions(enc: &mut Encoder, preconditions: &[ast::Precondition]) {
    enc.len(preconditions.len());
    for p in preconditions {
        enc.str(&p.sh);
        enc.str(&p.msg);
    }
}

fn encode_requires(enc: &mut Encoder, requires: Option<&ast::Requires>) {
    match requires {
        None => enc.tag(0),
        Some(r) => {
            enc.tag(1);
            enc.len(r.vars.len());
            for v in &r.vars {
                enc.str(&v.name);
                encode_str_slice(enc, &v.enum_values);
            }
        }
    }
}

fn encode_cache(enc: &mut Encoder, cache: Option<&ast::Cache>) {
    match cache {
        None => enc.tag(0),
        Some(c) => {
            enc.tag(1);
            enc.str(&c.inherit);
            enc.opt_bool(c.enabled);
            enc.str(&c.if_);
            enc.str(&c.url);
            enc.str(&c.lock);
            enc.str(&c.vk);
            enc.str(&c.namespace);
            // `api_key` is a credential, not an identity: left out.
            enc.str(&c.ttl);
            enc.str(&c.lock_timeout);
        }
    }
}

fn encode_location(enc: &mut Encoder, location: Option<&ast::Location>) {
    match location {
        None => enc.tag(0),
        Some(l) => {
            enc.tag(1);
            enc.u64(l.line as u64);
            enc.u64(l.column as u64);
            enc.str(&l.taskfile);
        }
    }
}

fn encode_opt_vars(enc: &mut Encoder, vars: Option<&ast::Vars>) -> Result<(), HashError> {
    match vars {
        None => enc.tag(0),
        Some(v) => {
            enc.tag(1);
            enc.len(v.len());
            for (key, var) in v.all() {
                enc.str(key);
                encode_var(enc, var)?;
            }
        }
    }
    Ok(())
}

fn encode_var(enc: &mut Encoder, var: &ast::Var) -> Result<(), HashError> {
    encode_opt_value(enc, var.value.as_ref())?;
    encode_opt_value(enc, var.live.as_ref())?;
    match &var.sh {
        None => enc.tag(0),
        Some(s) => {
            enc.tag(1);
            enc.str(s);
        }
    }
    enc.str(&var.ref_);
    enc.str(&var.dir);
    Ok(())
}

fn encode_opt_value(
    enc: &mut Encoder,
    value: Option<&crate::yaml::Value>,
) -> Result<(), HashError> {
    match value {
        None => enc.tag(0),
        Some(v) => {
            enc.tag(1);
            encode_value(enc, v)?;
        }
    }
    Ok(())
}

/// Encodes an arbitrary YAML value canonically. Mapping keys are visited in
/// their existing order; a Taskfile preserves mapping order, so this remains
/// stable for a given parsed value.
fn encode_value(enc: &mut Encoder, value: &crate::yaml::Value) -> Result<(), HashError> {
    use crate::yaml::Value;
    match value {
        Value::Null => enc.tag(0),
        Value::Bool(b) => {
            enc.tag(1);
            enc.bool(*b);
        }
        Value::Number(n) => {
            enc.tag(2);
            enc.str(&n.to_string());
        }
        Value::String(s) => {
            enc.tag(3);
            enc.str(s);
        }
        Value::Sequence(seq) => {
            enc.tag(4);
            enc.len(seq.len());
            for item in seq {
                encode_value(enc, item)?;
            }
        }
        Value::Mapping(map) => {
            enc.tag(5);
            enc.len(map.len());
            for (k, v) in map {
                encode_value(enc, k)?;
                encode_value(enc, v)?;
            }
        }
        Value::Tagged(tagged) => {
            enc.tag(6);
            enc.str(&tagged.tag.to_string());
            encode_value(enc, &tagged.value)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Cmd, Location, Task, Var, VarElement, Vars};
    use crate::yaml::Value;

    fn sample_task() -> Task {
        Task {
            location: Some(Location {
                line: 1,
                column: 1,
                taskfile: "Taskfile.yml".to_string(),
            }),
            namespace: "ns".to_string(),
            full_name: "ns:build".to_string(),
            cmds: vec![Cmd {
                cmd: "echo hello".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn name_uses_taskfile_and_local_name() {
        let t = sample_task();
        assert_eq!(name(&t).unwrap(), "Taskfile.yml:build");
    }

    // The same source task included directly and through a nested include
    // hashes to one identity, so `run: once` dedups it and both reach the same
    // cache entry.
    #[test]
    fn name_is_independent_of_include_depth() {
        let mut direct = sample_task();
        direct.namespace = "sub".to_string();
        direct.task = "sub:build".to_string();
        direct.full_name = "sub:build".to_string();
        let mut nested = sample_task();
        nested.namespace = "outer:sub".to_string();
        nested.task = "outer:sub:build".to_string();
        nested.full_name = "outer:sub:build".to_string();
        assert_eq!(name(&direct).unwrap(), name(&nested).unwrap());
        assert_eq!(name(&nested).unwrap(), "Taskfile.yml:build");
    }

    fn vars(key: &str, value: &str) -> Option<Vars> {
        Some(Vars::from_elements([VarElement {
            key: key.to_string(),
            value: Var {
                value: Some(Value::String(value.to_string())),
                ..Default::default()
            },
        }]))
    }

    // The key is the source hash plus a digest of the declared vars; a task
    // without sources has no key at all.
    #[test]
    fn cache_checksum_extends_the_source_hash() {
        let t = sample_task();
        let key = cache_checksum(&t, "abc").unwrap();
        assert!(
            key.starts_with("abc-") && key.len() == "abc-".len() + 16,
            "{key}"
        );
        assert_eq!(cache_checksum(&t, "abc").unwrap(), key);
        assert_eq!(cache_checksum(&t, "").unwrap(), "");
    }

    // A root task (no include vars) and an included copy (an empty map) are
    // the same task, so they key alike; the direct and nested copies do too.
    #[test]
    fn cache_checksum_ignores_an_empty_include_vars_map() {
        let root = sample_task();
        let mut included = sample_task();
        included.include_vars = Some(Vars::new());
        included.namespace = "outer:sub".to_string();
        included.task = "outer:sub:build".to_string();
        assert_eq!(
            cache_checksum(&root, "abc").unwrap(),
            cache_checksum(&included, "abc").unwrap()
        );
    }

    // Tasks whose commands render differently from the same raw text — through
    // the include's `vars` or their own — must not share an entry.
    #[test]
    fn cache_checksum_digests_include_and_task_vars() {
        let mut gcc = sample_task();
        gcc.include_vars = vars("CC", "gcc");
        let mut clang = gcc.clone();
        clang.include_vars = vars("CC", "clang");
        assert_ne!(
            cache_checksum(&gcc, "abc").unwrap(),
            cache_checksum(&clang, "abc").unwrap()
        );

        let mut o2 = sample_task();
        o2.vars = vars("CFLAGS", "-O2");
        let mut o0 = sample_task();
        o0.vars = vars("CFLAGS", "-O0");
        assert_ne!(
            cache_checksum(&o2, "abc").unwrap(),
            cache_checksum(&o0, "abc").unwrap()
        );
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(empty(&sample_task()).unwrap(), "");
    }

    #[test]
    fn hash_has_expected_prefix() {
        let t = sample_task();
        let h = hash(&t).unwrap();
        assert!(h.starts_with("Taskfile.yml:build:"));
        // taskfile + ":" + local name + ":" + 16 hex chars (xxHash3-64).
        let digest = h.rsplit(':').next().unwrap();
        assert_eq!(digest.len(), 16);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_is_deterministic() {
        let a = hash(&sample_task()).unwrap();
        let b = hash(&sample_task()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn hash_ignores_identity_only_fields() {
        let base = sample_task();
        let mut mutated = base.clone();
        // These fields are excluded from the digest (Go `hash:"ignore"`),
        // though `task`/`full_name`/`namespace` affect the label prefix only
        // via local_name, which stays "build" here.
        mutated.task = "renamed".to_string();
        mutated.prefix = "p".to_string();
        mutated.raw_cmds = vec![Cmd {
            cmd: "unrelated".to_string(),
            ..Default::default()
        }];
        mutated.source_hash = "abc".to_string();
        assert_eq!(hash(&base).unwrap(), hash(&mutated).unwrap());
    }

    #[test]
    fn hash_changes_with_command() {
        let base = sample_task();
        let mut mutated = base.clone();
        mutated.cmds = vec![Cmd {
            cmd: "echo world".to_string(),
            ..Default::default()
        }];
        assert_ne!(hash(&base).unwrap(), hash(&mutated).unwrap());
    }

    #[test]
    fn hash_changes_with_task_reference() {
        let base = sample_task();
        let mut referenced = base.clone();
        referenced.sources.push(ast::Glob {
            task: "lib".to_string(),
            ..Default::default()
        });
        let mut with_vars = base.clone();
        with_vars.sources.push(ast::Glob {
            task: "lib".to_string(),
            vars: Some(Vars::from_elements([VarElement {
                key: "X".to_string(),
                value: Var {
                    value: Some(Value::String("1".to_string())),
                    ..Default::default()
                },
            }])),
            ..Default::default()
        });
        let mut empty = base.clone();
        empty.sources.push(ast::Glob::default());
        let hashes = [&base, &referenced, &with_vars, &empty].map(|t| hash(t).unwrap());
        for (i, a) in hashes.iter().enumerate() {
            for b in hashes.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    // A group is part of the identity only where groups matter: group 0, every
    // entry of a list whose groups do not, encodes as before.
    #[test]
    fn hash_encodes_folded_groups_only() {
        let mut base = sample_task();
        base.sources.push(ast::Glob {
            glob: "/abs/lib/*.c".to_string(),
            ..Default::default()
        });
        let before = hash(&base).unwrap();
        let mut zero = base.clone();
        if let Some(g) = zero.sources.last_mut() {
            g.group = 0;
        }
        assert_eq!(hash(&zero).unwrap(), before);
        let mut one = base.clone();
        let mut two = base.clone();
        if let (Some(a), Some(b)) = (one.sources.last_mut(), two.sources.last_mut()) {
            a.group = 1;
            b.group = 2;
        }
        assert_ne!(hash(&one).unwrap(), before);
        assert_ne!(hash(&one).unwrap(), hash(&two).unwrap());
    }

    #[test]
    fn hash_task_reference_empty_vars_like_none() {
        let mut none = sample_task();
        none.sources.push(ast::Glob {
            task: "lib".to_string(),
            ..Default::default()
        });
        let mut empty = none.clone();
        if let Some(g) = empty.sources.last_mut() {
            g.vars = Some(Vars::new());
        }
        assert_eq!(hash(&none).unwrap(), hash(&empty).unwrap());
    }

    // A `sandbox.writes` entry naming a task grants access only: neither the
    // identity nor the cache key of the referring task moves. `sandbox` is
    // hashed nowhere today; this guards against adding it to `hash`, `name`
    // or `cache_checksum` later.
    #[test]
    fn hash_ignores_sandbox_writes_task_references() {
        let mut base = sample_task();
        base.sandbox = Some(crate::ast::Sandbox::default());
        let mut referenced = base.clone();
        if let Some(s) = &mut referenced.sandbox {
            s.write_tasks.push(ast::Glob {
                task: "lib".to_string(),
                vars: Some(Vars::from_elements([VarElement {
                    key: "X".to_string(),
                    value: Var {
                        value: Some(Value::String("1".to_string())),
                        ..Default::default()
                    },
                }])),
                ..Default::default()
            });
            s.task_grants.push((
                "lib".to_string(),
                vec![ast::Grant {
                    what: "sandbox.writes {task: lib}".to_string(),
                    declared: "out/*".to_string(),
                    path: "/abs/lib/out/*".to_string(),
                    kind: ast::GrantKind::Generates,
                }],
            ));
        }
        assert_eq!(hash(&base).unwrap(), hash(&referenced).unwrap());
        assert_eq!(name(&base).unwrap(), name(&referenced).unwrap());
        assert_eq!(
            cache_checksum(&base, "src").unwrap(),
            cache_checksum(&referenced, "src").unwrap()
        );
    }

    #[test]
    fn hash_ignores_task_env() {
        // Env is a Vars orderedmap, which Go's hashstructure does not traverse;
        // env differences reach the identity only through compiled fields.
        let base = sample_task();
        let mut mutated = base.clone();
        mutated.env = Some(Vars::from_elements([VarElement {
            key: "FOO".to_string(),
            value: Var {
                value: Some(Value::String("bar".into())),
                ..Default::default()
            },
        }]));
        assert_eq!(hash(&base).unwrap(), hash(&mutated).unwrap());
    }

    // A task whose *compiled* commands differ hashes differently even though the
    // vars field itself is not hashed — which is what lets the cycle check tell
    // one turn of a countdown from the next.
    #[test]
    fn hash_follows_compiled_cmds() {
        let mut a = sample_task();
        a.cmds = vec![ast::Cmd {
            cmd: "echo n=3".to_string(),
            ..Default::default()
        }];
        let mut b = a.clone();
        b.cmds = vec![ast::Cmd {
            cmd: "echo n=2".to_string(),
            ..Default::default()
        }];
        assert_ne!(hash(&a).unwrap(), hash(&b).unwrap());
    }

    #[test]
    fn hash_ignores_task_vars() {
        // Two tasks differing only in their (task-level) variables hash the
        // same, so `run: when_changed` dedups the same task reached via
        // different includes. The resolved values still affect identity through
        // the compiled cmds/dir/etc.
        let mut a = sample_task();
        a.vars = Some(Vars::from_elements([VarElement {
            key: "V".to_string(),
            value: Var {
                value: Some(Value::String("one".into())),
                ..Default::default()
            },
        }]));
        let mut b = a.clone();
        b.vars = Some(Vars::from_elements([VarElement {
            key: "V".to_string(),
            value: Var {
                value: Some(Value::Number(2.into())),
                ..Default::default()
            },
        }]));
        assert_eq!(hash(&a).unwrap(), hash(&b).unwrap());
    }

    // Fingerprints of Taskfiles full of scalars whose type depends on YAML
    // resolution rules, pinned to the values serde_yaml_ng produced: a change
    // in how any of them parses would invalidate every `.task` cache. The
    // per-scalar expectations are in `tests/yaml_pins.rs`.
    #[test]
    fn fingerprints_of_tricky_scalars_are_stable() {
        let yaml = indoc_yaml(
            r#"
        version: '3'
        tasks:
          build:
            vars:
              OCT: 017
              UMASK: 0022
              EIGHT: 08
              ZEROS: 00
              UNDER: 1_000
              HEX: 0x1F
              HEXU: 0x1_F
              OCTO: 0o17
              OCTU: 0O17
              BINU: 0B1
              ODDT: tRuE
              ODDN: nUlL
              NANS: .Nan
              HUGE: 1e400
              INF: .inf
              NINF: -.inf
              NAN: .NAN
              FLT: 1.0
              EXP: 1e3
              AVO: 6.02e23
              BIG: 1e20
              UMAX: 18446744073709551615
              IMIN: -9223372036854775808
              BIN: !!binary aGk=
              STR: !!str 123
              INT: !!int '7'
              TAG: !foo bar
              TAGM: !foo {x: 1}
              GLOB: !<tag:example.com,2000:x> 5
              YES: yes
              TRUEV: True
              NUL: ~
              EMPTY:
              QUOTED: '0x1F'
              BLOCK: |
                0x1F
              ANCH: &n 007
              ALIAS: *n
              SEQ: [1, -2, 3.5, true, null, 'x', !t 5]
              MAP:
                map: {1: a, true: b, ~: c, 1.5: d, 0x10: e, !k 2: f, [1, 2]: g, '1': h, <<: i}
            env:
              E1: 017
              E2: 1e3
              E3: .inf
            cmds:
              - echo hi
"#,
        );
        let tf: crate::ast::Taskfile = crate::yaml::from_str(&yaml).unwrap();
        let t = tf.tasks.get("build").unwrap();
        assert_eq!(cache_checksum(t, "abc").unwrap(), "abc-951c3ac90fa3d487");
        assert_eq!(fingerprint(t).unwrap(), "fc60ade48e794989");
    }

    // The YAML 1.1 spellings, signs, tags, merges and block scalars of
    // `tests/yaml_pins.rs`, pinned the same way.
    #[test]
    fn fingerprints_of_yaml_1_1_scalars_are_stable() {
        let yaml = indoc_yaml(
            r#"
        version: 3.10
        tasks:
          build:
            vars:
              B1: yes
              B2: no
              B3: on
              B4: off
              B5: y
              B6: n
              B7: Y
              B8: NO
              B9: Off
              B10: False
              B11: FALSE
              N1: Null
              N2: NULL
              N3: ~
              P1: +1
              P2: +1.5
              P3: +.inf
              Z1: -0
              Z2: -0.0
              TS1: 2001-12-14
              TS2: 2001-12-14t21:59:43.10-05:00
              SX1: 1:20
              SX2: 190:20:30
              F1: .5
              F2: 5.
              F3: 1E3
              F4: 1e+3
              F5: 1.0e-3
              F6: 0.1
              F7: .INF
              F8: .Inf
              F9: -.INF
              F10: .nan
              F11: 3.10
              I1: -0x1F
              I2: 0X1F
              I3: 0b1
              I4: 0o8
              I5: 0x
              I6: 0o17
              I7: 017
              I8: 0x1F
              T1: !!float 1
              T2: !!int 0x1F
              T3: !!str 1
              T4: !custom x
              T5: !!seq [1, 2]
              T6: !custom [1]
              ESC: "\x41é\N"
              FOLD: >
                folded
                text

                para
              LIT: |-
                keep
                lines
              KEEP: |+
                trail

              STRIP: >-
                a
                b
              PLAIN: plain
                multi
                line
              BASE:
                map: &base {a: 1, b: 2}
              MERGE:
                map: {<<: *base, b: 3}
              MERGES:
                map: {<<: [*base, {c: 4}], d: 5}
              TAGGED:
                map: !!map {x: !custom {y: 1}}
              KEYS:
                map: {1: a, 1.0: b, '1': c, -0: d, 0.: e}
            cmds:
              - echo hi
"#,
        );
        let tf: crate::ast::Taskfile = crate::yaml::from_str(&yaml).unwrap();
        assert_eq!(tf.version.as_deref(), Some("3.1"));
        let t = tf.tasks.get("build").unwrap();
        assert_eq!(cache_checksum(t, "abc").unwrap(), "abc-14ea9dbc1e2459b7");
        assert_eq!(fingerprint(t).unwrap(), "17b9784e4440d7ed");
    }

    /// Strips the 8-column indentation of the Taskfile literals above.
    fn indoc_yaml(text: &str) -> String {
        text.lines()
            .skip(1)
            .map(|line| line.strip_prefix("        ").unwrap_or(line))
            .map(|line| format!("{line}\n"))
            .collect()
    }
}
