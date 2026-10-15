//! Per-task sandbox settings: which files a task's commands may touch.

use serde::Deserialize;
use serde::de::{self, Deserializer};

use super::error::TaskfileDecodeError;
use super::glob::Glob;
use super::vars::Vars;
use crate::yaml::Value;

/// The `sandbox:` block of a task.
///
/// `sandbox: true` restricts the task's commands to its `sources` inside the
/// root directory; a mapping does the same and widens it with `reads` and
/// `writes`. `sandbox: false` (or no key) leaves the task unrestricted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sandbox {
    /// Files the commands may read without them being fingerprinted, in the
    /// `sources` glob syntax (e.g. `.git/**`).
    pub reads: Vec<Glob>,
    /// Directories the commands may change freely (build trees, caches),
    /// relative to the task directory. Missing ones are created before the
    /// task runs.
    pub writes: Vec<String>,
    /// The `task:` entries of `writes` (`task` and `vars` set only): the
    /// directories that task's `generates` write in are writable too.
    pub write_tasks: Vec<Glob>,
    /// The `generates` of each of `write_tasks`, absolute, with the name of
    /// the task they come from. Filled by the full compile.
    pub task_generates: Vec<(String, Vec<Glob>)>,
}

impl Sandbox {
    /// Decodes the `sandbox:` value: `None` when it is `false`.
    pub(crate) fn decode<E: de::Error>(value: Value) -> Result<Option<Self>, E> {
        match value {
            // `sandbox: ~` is rejected, as `schema.json` and the other
            // boolean keys (`watch`, `interactive`) reject it.
            Value::Bool(false) => Ok(None),
            Value::Bool(true) => Ok(Some(Self::default())),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Raw {
                    #[serde(default)]
                    reads: Vec<Glob>,
                    #[serde(default)]
                    writes: Vec<WritesEntry>,
                }
                let raw: Raw = crate::yaml::from_value(value)
                    .map_err(|e| E::custom(TaskfileDecodeError::wrap(e)))?;
                // `from:` and `task:` are resolved for sources, generates and
                // sandbox.writes only: here they would silently grant nothing.
                if raw.reads.iter().any(Glob::is_reference) {
                    return Err(E::custom(TaskfileDecodeError::message(
                        "sandbox.reads does not support `from:` or `task:`; list the files, or \
                         add them to sources",
                    )));
                }
                let mut sandbox = Self {
                    reads: raw.reads,
                    ..Self::default()
                };
                for entry in raw.writes {
                    match entry {
                        WritesEntry::Dir(dir) => sandbox.writes.push(dir),
                        WritesEntry::Task(g) => sandbox.write_tasks.push(g),
                    }
                }
                Ok(Some(sandbox))
            }
            _ => Err(E::custom(TaskfileDecodeError::type_message("sandbox"))),
        }
    }
}

/// A `sandbox.writes` entry: a directory, or `task:` + optional `vars:`.
enum WritesEntry {
    Dir(String),
    Task(Glob),
}

impl<'de> Deserialize<'de> for WritesEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        if !matches!(value, Value::Mapping(_)) {
            return crate::yaml::from_value(value)
                .map(Self::Dir)
                .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            task: Option<String>,
            vars: Option<Vars>,
        }
        const SHAPE: &str =
            "a sandbox.writes entry is a directory, or `task:` with optional `vars:`";
        let raw: Raw = crate::yaml::from_value(value).map_err(|e| {
            de::Error::custom(TaskfileDecodeError::message(format!("{SHAPE}: {e}")))
        })?;
        match (raw.task, raw.vars) {
            (Some(task), _) if task.is_empty() => Err(de::Error::custom(
                TaskfileDecodeError::message("a sandbox.writes `task:` entry names no task"),
            )),
            (Some(task), vars) => Ok(Self::Task(Glob {
                task,
                vars,
                ..Glob::default()
            })),
            (None, Some(_)) => Err(de::Error::custom(TaskfileDecodeError::message(
                "`vars:` requires `task:`",
            ))),
            (None, None) => Err(de::Error::custom(TaskfileDecodeError::message(SHAPE))),
        }
    }
}

/// Deserializes an optional `sandbox:` field.
pub(crate) fn de_opt_sandbox<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Sandbox>, D::Error> {
    let value = Value::deserialize(deserializer)
        .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
    Sandbox::decode(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "de_opt_sandbox")]
        sandbox: Option<Sandbox>,
    }

    fn parse(yaml: &str) -> Option<Sandbox> {
        crate::yaml::from_str::<Holder>(yaml).unwrap().sandbox
    }

    #[test]
    fn bool_forms() {
        assert_eq!(parse("sandbox: true"), Some(Sandbox::default()));
        assert_eq!(parse("sandbox: false"), None);
        assert_eq!(parse("{}"), None);
    }

    #[test]
    fn mapping() {
        let s = parse(
            "sandbox:\n  reads: [\".git/**\", {exclude: \".git/index\"}]\n  writes: [build]\n",
        )
        .unwrap();
        assert_eq!(s.reads.len(), 2);
        assert_eq!(s.reads[0].glob, ".git/**");
        assert!(s.reads[1].negate);
        assert_eq!(s.writes, vec!["build".to_string()]);
        assert!(s.write_tasks.is_empty());
    }

    #[test]
    fn writes_naming_a_task() {
        let s = parse(
            "sandbox:\n  writes: [build/tmp, {task: rust:build}, {task: i18n, vars: {LANG: fr}}]\n",
        )
        .unwrap();
        assert_eq!(s.writes, vec!["build/tmp".to_string()]);
        assert_eq!(s.write_tasks.len(), 2);
        assert_eq!(s.write_tasks[0].task, "rust:build");
        assert!(s.write_tasks[0].vars.is_none());
        assert_eq!(s.write_tasks[1].task, "i18n");
        assert_eq!(s.write_tasks[1].vars.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn rejects_malformed_writes_entries() {
        for (input, want) in [
            (
                "{task: a, glob: b}",
                "is a directory, or `task:` with optional `vars:`",
            ),
            (
                "{from: deps}",
                "is a directory, or `task:` with optional `vars:`",
            ),
            ("{}", "is a directory, or `task:` with optional `vars:`"),
            ("{vars: {A: b}}", "`vars:` requires `task:`"),
            ("{task: ''}", "names no task"),
            (
                "{task: [a]}",
                "is a directory, or `task:` with optional `vars:`",
            ),
            ("[a]", "Failed to decode Taskfile"),
        ] {
            let yaml = format!("sandbox:\n  writes: [{input}]\n");
            let err = crate::yaml::from_str::<Holder>(&yaml)
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains(want), "{input}: {err}");
        }
    }

    #[test]
    fn rejects_unknown_keys_and_scalars() {
        assert!(crate::yaml::from_str::<Holder>("sandbox:\n  write: [x]\n").is_err());
        assert!(crate::yaml::from_str::<Holder>("sandbox: yes-please\n").is_err());
        assert!(crate::yaml::from_str::<Holder>("sandbox: ~\n").is_err());
        let from = crate::yaml::from_str::<Holder>("sandbox:\n  reads: [{from: deps}]\n");
        let err = from.err().unwrap().to_string();
        assert!(err.contains("does not support `from:`"), "{err}");
        let task = crate::yaml::from_str::<Holder>("sandbox:\n  reads: [{task: a}]\n");
        let err = task.err().unwrap().to_string();
        assert!(err.contains("or `task:`"), "{err}");
    }
}
