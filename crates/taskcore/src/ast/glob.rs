//! File patterns used in `sources` and `generates` lists.

use serde::Deserialize;
use serde::de::{self, Deserializer};

use super::error::TaskfileDecodeError;
use super::vars::Vars;
use crate::yaml::Value;

/// A file pattern. It accepts five YAML shapes:
///
/// - scalar: a plain glob pattern (`"src/**/*.go"`).
/// - `exclude:` a negated pattern.
/// - `glob:` + optional `fingerprint:`: the glob defines the file set while
///   `fingerprint` names a single representative file to hash instead.
/// - `from:` references the same field of other tasks (`deps`, `cmds`).
/// - `task:` + optional `vars:` references the same field of one named task,
///   called or not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Glob {
    pub glob: String,
    pub negate: bool,
    /// When set, only this file is hashed for up-to-date checks.
    pub fingerprint: String,
    /// When set, references the same field of other tasks (e.g. `deps`).
    pub from: String,
    /// When set, references the same field of this task.
    pub task: String,
    /// The variables `task` is compiled with.
    pub vars: Option<Vars>,
    /// Which task's entry it is, once references are resolved: 0 for the
    /// task's own, then one number per task whose entries were folded in.
    /// Never parsed. A folded task's exclusions apply to its own group only;
    /// the task's own (group 0) apply to the whole list.
    pub group: u32,
    /// Never parsed, never hashed: in a list whose groups collapsed into
    /// group 0, whether only folded tasks have the entry. A path it names is
    /// then not exempt from the task's exclusions ([`Glob::is_named`]).
    ///
    /// Two tasks whose lists differ only in this flag share an identity and
    /// so `.task` state; their checksums still differ when the selected files
    /// do, which costs reruns, never a stale up-to-date. Hashing it would move
    /// the identity of every task folding a named entry next to an exclusion.
    pub folded: bool,
}

impl Glob {
    /// Whether the entry references other tasks (`from:` or `task:`) rather
    /// than matching files itself.
    pub fn is_reference(&self) -> bool {
        !self.from.is_empty() || !self.task.is_empty()
    }

    /// Whether the entry names its path explicitly: an include without
    /// wildcards, or one with a `fingerprint`, of the task itself. Only an
    /// exclusion naming the same path removes it.
    pub fn is_named(&self) -> bool {
        !self.negate
            && !self.folded
            && !self.is_reference()
            && (!self.fingerprint.is_empty() || !self.has_wildcards())
    }

    /// Whether the entry as written, before it is joined onto the task
    /// directory, has wildcards. An exclusion without is matched by equality
    /// with the joined path.
    pub fn has_wildcards(&self) -> bool {
        self.glob.contains(['*', '?', '['])
    }
}

impl<'de> Deserialize<'de> for Glob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(glob) => Ok(Glob {
                glob,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    exclude: String,
                    #[serde(default)]
                    glob: String,
                    #[serde(default)]
                    fingerprint: String,
                    #[serde(default)]
                    from: String,
                    #[serde(default)]
                    task: String,
                    vars: Option<Vars>,
                }
                let raw: Raw = crate::yaml::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                let mut g = Glob::default();
                if !raw.task.is_empty() {
                    if !(raw.exclude.is_empty()
                        && raw.glob.is_empty()
                        && raw.fingerprint.is_empty()
                        && raw.from.is_empty())
                    {
                        return Err(de::Error::custom(TaskfileDecodeError::message(
                            "a `task:` entry takes only `vars:`",
                        )));
                    }
                    g.task = raw.task;
                    g.vars = raw.vars;
                    return Ok(g);
                }
                if raw.vars.is_some() {
                    return Err(de::Error::custom(TaskfileDecodeError::message(
                        "`vars:` requires `task:`",
                    )));
                }
                if !raw.from.is_empty() {
                    g.from = raw.from;
                    return Ok(g);
                }
                if !raw.exclude.is_empty() {
                    g.glob = raw.exclude;
                    g.negate = true;
                } else {
                    g.glob = raw.glob;
                }
                g.fingerprint = raw.fingerprint;
                Ok(g)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message("glob"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar() {
        let g: Glob = crate::yaml::from_str(r#""src/**/*.go""#).unwrap();
        assert_eq!(g.glob, "src/**/*.go");
        assert!(!g.negate);
        assert!(g.fingerprint.is_empty());
    }

    #[test]
    fn exclude() {
        let g: Glob = crate::yaml::from_str(r#"exclude: "vendor/**""#).unwrap();
        assert_eq!(g.glob, "vendor/**");
        assert!(g.negate);
        assert!(g.fingerprint.is_empty());
    }

    #[test]
    fn glob_with_fingerprint() {
        let input =
            "\nglob: \"node_modules/**/*\"\nfingerprint: \"node_modules/.yarn-state.yml\"\n";
        let g: Glob = crate::yaml::from_str(input).unwrap();
        assert_eq!(g.glob, "node_modules/**/*");
        assert!(!g.negate);
        assert_eq!(g.fingerprint, "node_modules/.yarn-state.yml");
    }

    #[test]
    fn glob_without_fingerprint() {
        let g: Glob = crate::yaml::from_str(r#"glob: "build/**/*""#).unwrap();
        assert_eq!(g.glob, "build/**/*");
        assert!(!g.negate);
        assert!(g.fingerprint.is_empty());
    }

    #[test]
    fn in_generates_list() {
        let input = "\n- \"build/**/*\"\n- exclude: \"build/tmp/**\"\n- glob: \"node_modules/**/*\"\n  fingerprint: \"node_modules/.yarn-state.yml\"\n";
        let globs: Vec<Glob> = crate::yaml::from_str(input).unwrap();
        assert_eq!(globs.len(), 3);
        assert_eq!(globs[0].glob, "build/**/*");
        assert!(!globs[0].negate);
        assert!(globs[0].fingerprint.is_empty());
        assert_eq!(globs[1].glob, "build/tmp/**");
        assert!(globs[1].negate);
        assert!(globs[1].fingerprint.is_empty());
        assert_eq!(globs[2].glob, "node_modules/**/*");
        assert!(!globs[2].negate);
        assert_eq!(globs[2].fingerprint, "node_modules/.yarn-state.yml");
    }

    #[test]
    fn from_deps() {
        let g: Glob = crate::yaml::from_str("from: deps").unwrap();
        assert!(g.glob.is_empty());
        assert!(!g.negate);
        assert!(g.fingerprint.is_empty());
        assert_eq!(g.from, "deps");
    }

    #[test]
    fn from_cmds() {
        let g: Glob = crate::yaml::from_str("from: cmds").unwrap();
        assert!(g.glob.is_empty());
        assert_eq!(g.from, "cmds");
    }

    #[test]
    fn list_with_from() {
        let input = "\n- \"src/**/*.go\"\n- from: deps\n- exclude: \"vendor/**\"\n";
        let globs: Vec<Glob> = crate::yaml::from_str(input).unwrap();
        assert_eq!(globs.len(), 3);
        assert_eq!(globs[0].glob, "src/**/*.go");
        assert!(globs[0].from.is_empty());
        assert!(globs[1].glob.is_empty());
        assert_eq!(globs[1].from, "deps");
        assert_eq!(globs[2].glob, "vendor/**");
        assert!(globs[2].negate);
        assert!(globs[2].from.is_empty());
    }

    #[test]
    fn task_reference() {
        let g: Glob = crate::yaml::from_str("task: build").unwrap();
        assert_eq!(g.task, "build");
        assert!(g.vars.is_none());
        assert!(g.glob.is_empty());
        assert!(g.is_reference());
    }

    #[test]
    fn task_reference_with_vars() {
        let g: Glob = crate::yaml::from_str("task: build\nvars:\n  TARGET: linux\n").unwrap();
        assert_eq!(g.task, "build");
        assert_eq!(g.vars.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn task_reference_rejects_glob_fields() {
        for input in [
            "task: build\nglob: src/**",
            "task: build\nexclude: src/**",
            "task: build\nfingerprint: x",
            "task: build\nfrom: deps",
        ] {
            let err = crate::yaml::from_str::<Glob>(input).unwrap_err();
            assert!(
                err.to_string().contains("takes only `vars:`"),
                "{input}: {err}"
            );
        }
    }

    #[test]
    fn vars_without_task() {
        let err = crate::yaml::from_str::<Glob>("glob: src/**\nvars:\n  A: b").unwrap_err();
        assert!(err.to_string().contains("requires `task:`"), "{err}");
    }
}
