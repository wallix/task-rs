//! A single task and its polymorphic YAML shapes.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use crate::filepathext;

use super::cache::Cache;
use super::cmd::Cmd;
use super::dep::Dep;
use super::dialect::Dialect;
use super::error::TaskfileDecodeError;
use super::glob::Glob;
use super::location::Location;
use super::platforms::Platform;
use super::precondition::Precondition;
use super::prompt::Prompt;
use super::requires::Requires;
use super::vars::Vars;

/// A task definition.
///
/// Fields tagged in Go with `hash:"ignore"` (`task`, `prefix`, `namespace`,
/// `full_name`, `raw_cmds`, `source_hash`) are populated during merging or
/// compilation, not by the YAML decoder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Task {
    pub task: String,
    pub setup: Vec<Dep>,
    pub cmds: Vec<Cmd>,
    pub deps: Vec<Dep>,
    pub label: String,
    pub desc: String,
    pub prompt: Prompt,
    pub summary: String,
    pub requires: Option<Requires>,
    pub aliases: Vec<String>,
    pub sources: Vec<Glob>,
    pub generates: Vec<Glob>,
    pub cache: Option<Cache>,
    pub preconditions: Vec<Precondition>,
    pub dirs: Vec<String>,
    pub set: Vec<String>,
    pub shopt: Vec<String>,
    pub vars: Option<Vars>,
    pub env: Option<Vars>,
    pub dotenv: Vec<String>,
    /// Explicit silent flag; `None` when unset.
    pub silent: Option<bool>,
    pub interactive: bool,
    pub internal: bool,
    pub prefix: String,
    pub ignore_error: bool,
    pub run: String,
    pub platforms: Vec<Platform>,
    pub if_: String,
    pub watch: bool,
    pub location: Option<Location>,
    pub failfast: bool,
    // Populated during merging.
    pub namespace: String,
    /// The template dialect this task's strings are authored in, copied from the
    /// owning Taskfile's `templater:` field when the file is read.
    pub dialect: Dialect,
    pub include_vars: Option<Vars>,
    pub included_taskfile_vars: Option<Vars>,
    pub full_name: String,
    /// The original unresolved command templates, set during compilation for
    /// stable checksumming independent of variable resolution.
    pub raw_cmds: Vec<Cmd>,
    /// The checksum of sources + raw commands + generates, computed once during
    /// compilation and reused for locking, up-to-date checks, and cache keys.
    pub source_hash: String,
}

impl Task {
    /// Returns the display name, preferring `label`, then `full_name`, then the
    /// task key.
    pub fn name(&self) -> &str {
        if !self.label.is_empty() {
            return &self.label;
        }
        if !self.full_name.is_empty() {
            return &self.full_name;
        }
        &self.task
    }

    /// Returns the task name with its namespace prefix (and separator) trimmed.
    pub fn local_name(&self) -> String {
        let name = self.full_name.as_str();
        let name = name.strip_prefix(&self.namespace).unwrap_or(name);
        name.strip_prefix(':').unwrap_or(name).to_string()
    }

    /// Reports whether silent mode is explicitly enabled. Returns false when
    /// `silent` is unset or explicitly false.
    pub fn is_silent(&self) -> bool {
        self.silent == Some(true)
    }

    /// Resolves the final working directory from the `dirs` stack, scanning
    /// right-to-left for the rightmost absolute path.
    pub fn compute_dir(&self) -> std::path::PathBuf {
        filepathext::join_dirs(&self.dirs)
    }

    /// Checks whether `name` matches this task's name or one of its aliases,
    /// returning the captured wildcard values on a match.
    pub fn wildcard_match(&self, name: &str) -> (bool, Vec<String>) {
        let mut names: Vec<&str> = Vec::with_capacity(self.aliases.len().saturating_add(1));
        names.push(self.task.as_str());
        for alias in &self.aliases {
            names.push(alias.as_str());
        }

        for task_name in names {
            let Some(wildcards) = wildcard_captures(task_name, name) else {
                continue;
            };
            let wildcard_count = task_name.matches('*').count();
            if wildcards.len() != wildcard_count {
                continue;
            }
            return (true, wildcards);
        }

        (false, Vec::new())
    }
}

/// Matches `name` against `pattern`, where each `*` in the pattern is a greedy
/// wildcard capturing any run of characters. Returns the captured substrings on
/// a full-string match, or `None` if it does not match.
fn wildcard_captures(pattern: &str, name: &str) -> Option<Vec<String>> {
    let segments: Vec<&str> = pattern.split('*').collect();
    // A pattern with no `*` matches only its literal equal.
    if segments.len() == 1 {
        return if pattern == name {
            Some(Vec::new())
        } else {
            None
        };
    }

    let mut captures: Vec<String> = Vec::with_capacity(segments.len().saturating_sub(1));
    let mut pos = 0usize;

    // The first literal segment must anchor at the start.
    let first = segments.first().copied().unwrap_or_default();
    if !name.get(pos..)?.starts_with(first) {
        return None;
    }
    pos = pos.saturating_add(first.len());

    let middle = segments.get(1..segments.len().saturating_sub(1))?;
    let last = segments.last().copied().unwrap_or_default();

    // Each `*` greedily consumes up to the next literal segment. Greedy `.*`
    // matches the rightmost occurrence, so search from the end of the
    // remaining input.
    for sep in middle {
        let rest = name.get(pos..)?;
        let idx = if sep.is_empty() { 0 } else { rest.rfind(sep)? };
        let capture = rest.get(..idx)?.to_string();
        captures.push(capture);
        pos = pos.saturating_add(idx).saturating_add(sep.len());
    }

    // The final `*` captures whatever remains before the trailing literal.
    let rest = name.get(pos..)?;
    if !rest.ends_with(last) {
        return None;
    }
    let capture_end = rest.len().saturating_sub(last.len());
    captures.push(rest.get(..capture_end)?.to_string());

    Some(captures)
}

/// Deserializes a sequence, skipping null (`~`) elements — matching Go, which
/// decodes a nil list entry to a nil pointer and ignores it.
fn de_vec_skip_null<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let items: Vec<Option<T>> = Vec::deserialize(deserializer)?;
    Ok(items.into_iter().flatten().collect())
}

/// As [`de_vec_skip_null`], for an optional sequence field.
fn de_opt_vec_skip_null<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let items: Option<Vec<Option<T>>> = Option::deserialize(deserializer)?;
    Ok(items.map(|v| v.into_iter().flatten().collect()))
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let node = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match node {
            // An empty task body (`taskname:` with no value) is a valid no-op task.
            Value::Null => Ok(Task::default()),
            // Shortcut for a simple task with a list of commands.
            Value::Sequence(_) => {
                let cmds: Vec<Cmd> = serde_yaml_ng::from_value(node)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Task {
                    cmds,
                    ..Default::default()
                })
            }
            // The full task object.
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default, deserialize_with = "de_vec_skip_null")]
                    setup: Vec<Dep>,
                    #[serde(default, deserialize_with = "de_opt_vec_skip_null")]
                    cmds: Option<Vec<Cmd>>,
                    cmd: Option<Cmd>,
                    #[serde(default, deserialize_with = "de_vec_skip_null")]
                    deps: Vec<Dep>,
                    #[serde(default)]
                    label: String,
                    #[serde(default)]
                    desc: String,
                    #[serde(default)]
                    prompt: Prompt,
                    #[serde(default)]
                    summary: String,
                    #[serde(default)]
                    aliases: Vec<String>,
                    #[serde(default)]
                    sources: Vec<Glob>,
                    #[serde(default)]
                    generates: Vec<Glob>,
                    cache: Option<Cache>,
                    #[serde(default, deserialize_with = "de_vec_skip_null")]
                    preconditions: Vec<Precondition>,
                    #[serde(default)]
                    dir: String,
                    #[serde(default)]
                    set: Vec<String>,
                    #[serde(default)]
                    shopt: Vec<String>,
                    vars: Option<Vars>,
                    env: Option<Vars>,
                    #[serde(default)]
                    dotenv: Vec<String>,
                    silent: Option<bool>,
                    #[serde(default)]
                    interactive: bool,
                    #[serde(default)]
                    internal: bool,
                    #[serde(default)]
                    prefix: String,
                    #[serde(default, rename = "ignore_error")]
                    ignore_error: bool,
                    #[serde(default)]
                    run: String,
                    #[serde(default)]
                    platforms: Vec<Platform>,
                    #[serde(default, rename = "if")]
                    if_: String,
                    requires: Option<Requires>,
                    #[serde(default)]
                    watch: bool,
                    #[serde(default)]
                    failfast: bool,
                }
                let raw: Raw = serde_yaml_ng::from_value(node)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;

                let cmds = match (raw.cmd, raw.cmds) {
                    (Some(_), Some(_)) => {
                        return Err(de::Error::custom(TaskfileDecodeError::message(
                            "task cannot have both cmd and cmds",
                        )));
                    }
                    (Some(cmd), None) => vec![cmd],
                    (None, cmds) => cmds.unwrap_or_default(),
                };

                let dirs = if raw.dir.is_empty() {
                    Vec::new()
                } else {
                    vec![raw.dir]
                };

                Ok(Task {
                    setup: raw.setup,
                    cmds,
                    deps: raw.deps,
                    label: raw.label,
                    desc: raw.desc,
                    prompt: raw.prompt,
                    summary: raw.summary,
                    aliases: raw.aliases,
                    sources: raw.sources,
                    generates: raw.generates,
                    cache: raw.cache,
                    preconditions: raw.preconditions,
                    dirs,
                    set: raw.set,
                    shopt: raw.shopt,
                    vars: raw.vars,
                    env: raw.env,
                    dotenv: raw.dotenv,
                    silent: raw.silent,
                    interactive: raw.interactive,
                    internal: raw.internal,
                    prefix: raw.prefix,
                    ignore_error: raw.ignore_error,
                    run: raw.run,
                    platforms: raw.platforms,
                    if_: raw.if_,
                    requires: raw.requires,
                    watch: raw.watch,
                    failfast: raw.failfast,
                    ..Default::default()
                })
            }
            // Any scalar is the shortcut syntax for a single-command task.
            other => {
                let cmd: Cmd = serde_yaml_ng::from_value(other)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Task {
                    cmds: vec![cmd],
                    ..Default::default()
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_task_body_is_empty_noop() {
        // `taskname:` with no value is a valid no-op task (matches Go).
        let t: Task = serde_yaml_ng::from_str("~").unwrap();
        assert!(t.cmds.is_empty());
        assert!(t.deps.is_empty());
    }

    #[test]
    fn scalar_task_is_single_command() {
        let t: Task = serde_yaml_ng::from_str("echo hi").unwrap();
        assert_eq!(t.cmds.len(), 1);
        assert_eq!(t.cmds[0].cmd, "echo hi");
    }

    #[test]
    fn sequence_task_is_command_list() {
        let t: Task = serde_yaml_ng::from_str("- echo a\n- echo b\n").unwrap();
        assert_eq!(t.cmds.len(), 2);
    }

    #[test]
    fn both_cmd_and_cmds_errors() {
        let input = "cmd: echo a\ncmds:\n  - echo b\n";
        let err = serde_yaml_ng::from_str::<Task>(input).unwrap_err();
        assert!(err.to_string().contains("both cmd and cmds"));
    }

    #[test]
    fn dir_becomes_dirs_stack() {
        let t: Task = serde_yaml_ng::from_str("dir: ./sub\ncmds:\n  - echo hi\n").unwrap();
        assert_eq!(t.dirs, vec!["./sub".to_string()]);
    }

    #[test]
    fn is_silent_reflects_explicit_flag() {
        let mut t = Task::default();
        assert!(!t.is_silent());
        t.silent = Some(false);
        assert!(!t.is_silent());
        t.silent = Some(true);
        assert!(t.is_silent());
    }

    #[test]
    fn name_prefers_label_then_full_name() {
        let mut t = Task {
            task: "build".to_string(),
            ..Default::default()
        };
        assert_eq!(t.name(), "build");
        t.full_name = "ns:build".to_string();
        assert_eq!(t.name(), "ns:build");
        t.label = "Build it".to_string();
        assert_eq!(t.name(), "Build it");
    }

    #[test]
    fn local_name_trims_namespace() {
        let t = Task {
            namespace: "ns".to_string(),
            full_name: "ns:build".to_string(),
            ..Default::default()
        };
        assert_eq!(t.local_name(), "build");
    }

    #[test]
    fn wildcard_match_captures() {
        let t = Task {
            task: "generate:*".to_string(),
            ..Default::default()
        };
        let (matched, caps) = t.wildcard_match("generate:mocks");
        assert!(matched);
        assert_eq!(caps, vec!["mocks".to_string()]);
    }

    #[test]
    fn wildcard_match_multiple() {
        let t = Task {
            task: "*:*".to_string(),
            ..Default::default()
        };
        let (matched, caps) = t.wildcard_match("a:b");
        assert!(matched);
        assert_eq!(caps, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn wildcard_match_alias() {
        let t = Task {
            task: "build".to_string(),
            aliases: vec!["b:*".to_string()],
            ..Default::default()
        };
        let (matched, caps) = t.wildcard_match("b:release");
        assert!(matched);
        assert_eq!(caps, vec!["release".to_string()]);
    }

    #[test]
    fn wildcard_no_match() {
        let t = Task {
            task: "build".to_string(),
            ..Default::default()
        };
        let (matched, caps) = t.wildcard_match("test");
        assert!(!matched);
        assert!(caps.is_empty());
    }

    #[test]
    fn generates_fingerprint() {
        let input = "\ncmds:\n  - yarn install --immutable\nsources:\n  - package.json\n  - yarn.lock\ngenerates:\n  - glob: \"node_modules/**/*\"\n    fingerprint: \"node_modules/.yarn-state.yml\"\n";
        let task: Task = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(task.generates.len(), 1);
        assert_eq!(task.generates[0].glob, "node_modules/**/*");
        assert_eq!(
            task.generates[0].fingerprint,
            "node_modules/.yarn-state.yml"
        );
        assert!(!task.generates[0].negate);
        assert_eq!(task.sources.len(), 2);
        assert_eq!(task.sources[0].glob, "package.json");
        assert_eq!(task.sources[1].glob, "yarn.lock");
    }

    #[test]
    fn generates_mixed() {
        let input = "\ncmds:\n  - make build\ngenerates:\n  - \"build/**/*\"\n  - glob: \"node_modules/**/*\"\n    fingerprint: \"node_modules/.yarn-state.yml\"\n  - exclude: \"build/tmp/**\"\n";
        let task: Task = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(task.generates.len(), 3);
        assert_eq!(task.generates[0].glob, "build/**/*");
        assert!(task.generates[0].fingerprint.is_empty());
        assert!(!task.generates[0].negate);
        assert_eq!(task.generates[1].glob, "node_modules/**/*");
        assert_eq!(
            task.generates[1].fingerprint,
            "node_modules/.yarn-state.yml"
        );
        assert!(!task.generates[1].negate);
        assert_eq!(task.generates[2].glob, "build/tmp/**");
        assert!(task.generates[2].fingerprint.is_empty());
        assert!(task.generates[2].negate);
    }
}
