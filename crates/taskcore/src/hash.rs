//! Stable content hashing of a task, used as its cache identity.
//!
//! A task's hash combines the originating Taskfile and local name with a
//! deterministic digest of the task's meaningful fields. Fields populated
//! during merging or compilation (`task`, `prefix`, `namespace`, `full_name`,
//! `raw_cmds`, `source_hash`) are excluded so that a task hashes to the same
//! value regardless of how it was assembled. The digest is an xxHash3 over a
//! canonical byte encoding: it need not match any external tool, only be
//! stable for a given task and change when a meaningful field changes.

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
    encode_globs(enc, &t.sources);
    encode_globs(enc, &t.generates);
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

fn encode_globs(enc: &mut Encoder, globs: &[ast::Glob]) {
    enc.len(globs.len());
    for g in globs {
        enc.str(&g.glob);
        enc.bool(g.negate);
        enc.str(&g.fingerprint);
        enc.str(&g.from);
    }
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
    value: Option<&serde_yaml_ng::Value>,
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
fn encode_value(enc: &mut Encoder, value: &serde_yaml_ng::Value) -> Result<(), HashError> {
    use serde_yaml_ng::Value;
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
    use serde_yaml_ng::Value;

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
}
