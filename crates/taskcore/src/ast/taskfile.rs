//! The root Taskfile AST and the merge that folds includes into it.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::cache::Caches;
use super::dialect::Dialect;
use super::error::TaskfileDecodeError;
use super::include::{Include, Includes};
use super::output::Output;
use super::tasks::Tasks;
use super::vars::Vars;

/// The character that separates namespaces in task names.
pub const NAMESPACE_SEPARATOR: &str = ":";

/// The message raised when an included Taskfile declares dotenv files.
pub const ERR_INCLUDED_TASKFILES_CANT_HAVE_DOTENVS: &str = "task: Included Taskfiles can't have dotenv declarations. Please, move the dotenv declaration to the main Taskfile";

/// The abstract syntax tree for a Taskfile.
///
/// `version` and `interval` are kept as their raw YAML strings: the former is a
/// semantic version, the latter a Go duration (e.g. `500ms`), and neither is
/// interpreted at this layer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Taskfile {
    pub location: String,
    pub version: Option<String>,
    pub output: Output,
    pub includes: Includes,
    pub set: Vec<String>,
    pub shopt: Vec<String>,
    pub vars: Vars,
    pub env: Vars,
    pub tasks: Tasks,
    pub silent: bool,
    pub dotenv: Vec<String>,
    pub run: String,
    pub interval: String,
    pub caches: Caches,
    /// The effective template dialect for this file's string fields. When the
    /// `templater:` field is absent the reader resolves this by detection
    /// (defaulting to Jinja); when present it is that value.
    pub templater: Dialect,
    /// Whether the `templater:` field was written explicitly. The reader only
    /// runs dialect detection (and honours the Jinja default) when this is
    /// false; an explicit marker always wins.
    pub templater_explicit: bool,
}

impl Taskfile {
    /// Merges `other` into `self`, applying `include`'s namespacing and import
    /// rules to its tasks and variables.
    pub fn merge(&mut self, other: &Taskfile, include: &Include) -> Result<(), String> {
        if self.version != other.version {
            return Err(format!(
                "task: Taskfiles versions should match. First is \"{}\" but second is \"{}\"",
                self.version.as_deref().unwrap_or_default(),
                other.version.as_deref().unwrap_or_default()
            ));
        }
        if !other.dotenv.is_empty() {
            return Err(ERR_INCLUDED_TASKFILES_CANT_HAVE_DOTENVS.to_string());
        }
        if other.output.is_set() {
            self.output = other.output.clone();
        }

        // When the included Taskfile is globally silent, every one of its tasks
        // that has not set `silent` inherits `silent: true`. A clone carries
        // this adjustment so the source Taskfile is left untouched.
        let mut other_tasks = other.tasks.clone();
        if other.silent {
            for key in other_tasks.keys(crate::sort::Sorter::None) {
                if let Some(task) = other_tasks.get_mut(&key)
                    && task.silent.is_none()
                {
                    task.silent = Some(true);
                }
            }
        }

        self.vars.merge(&other.vars, Some(include));
        self.env.merge(&other.env, Some(include));
        self.tasks.merge(&other_tasks, include, &self.vars)
    }
}

/// Accept a YAML scalar (string, integer, float, bool) and render it as a
/// string, so `version: 3` and `version: '3'` both parse.
fn de_scalar_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(other) => Err(de::Error::custom(format!(
            "invalid version scalar: {other:?}"
        ))),
    }
}

impl<'de> Deserialize<'de> for Taskfile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default, deserialize_with = "de_scalar_string")]
                    version: Option<String>,
                    #[serde(default)]
                    output: Output,
                    includes: Option<Includes>,
                    #[serde(default)]
                    set: Vec<String>,
                    #[serde(default)]
                    shopt: Vec<String>,
                    vars: Option<Vars>,
                    env: Option<Vars>,
                    tasks: Option<Tasks>,
                    #[serde(default)]
                    silent: bool,
                    #[serde(default)]
                    dotenv: Vec<String>,
                    #[serde(default)]
                    run: String,
                    #[serde(default)]
                    interval: String,
                    #[serde(default)]
                    caches: Caches,
                    templater: Option<Dialect>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Taskfile {
                    location: String::new(),
                    version: raw.version,
                    output: raw.output,
                    includes: raw.includes.unwrap_or_default(),
                    set: raw.set,
                    shopt: raw.shopt,
                    vars: raw.vars.unwrap_or_default(),
                    env: raw.env.unwrap_or_default(),
                    tasks: raw.tasks.unwrap_or_default(),
                    silent: raw.silent,
                    dotenv: raw.dotenv,
                    run: raw.run,
                    interval: raw.interval,
                    caches: raw.caches,
                    templater: raw.templater.unwrap_or_default(),
                    templater_explicit: raw.templater.is_some(),
                })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "taskfile",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_taskfile() {
        let input = "version: '3'\ntasks:\n  build:\n    cmds:\n      - echo hi\n";
        let tf: Taskfile = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(tf.version.as_deref(), Some("3"));
        assert_eq!(tf.tasks.len(), 1);
        assert!(tf.tasks.get("build").is_some());
    }

    #[test]
    fn accepts_bare_integer_and_float_version() {
        // Go accepts an unquoted `version: 3`; both scalar forms map to "3"/"3.1".
        let tf: Taskfile = serde_yaml_ng::from_str("version: 3\ntasks: {}\n").unwrap();
        assert_eq!(tf.version.as_deref(), Some("3"));
        let tf: Taskfile = serde_yaml_ng::from_str("version: 3.1\ntasks: {}\n").unwrap();
        assert_eq!(tf.version.as_deref(), Some("3.1"));
    }

    #[test]
    fn empty_maps_default_to_empty() {
        let input = "version: '3'\n";
        let tf: Taskfile = serde_yaml_ng::from_str(input).unwrap();
        assert!(tf.includes.is_empty());
        assert!(tf.vars.is_empty());
        assert!(tf.env.is_empty());
        assert!(tf.tasks.is_empty());
    }

    #[test]
    fn caches_map() {
        let input = "\nversion: '3'\ncaches:\n  default:\n    url: 'redis://host/{{.CHECKSUM}}'\n  doc:\n    enabled: false\n    url: 'file:///tmp/doc.zip'\ntasks:\n  build:\n    cmds:\n      - echo hi\n";
        let tf: Taskfile = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(tf.caches.len(), 2);
        assert_eq!(
            tf.caches.get("default").unwrap().url,
            "redis://host/{{.CHECKSUM}}"
        );
        assert_eq!(tf.caches.get("doc").unwrap().enabled, Some(false));
    }

    #[test]
    fn ttl_inherited_from_caches_map() {
        let input = "\nversion: '3'\ncaches:\n  default:\n    url: 'redis://host/{{.CHECKSUM}}'\n    ttl: 72h\ntasks:\n  build:\n    cmds:\n      - echo hi\n";
        let tf: Taskfile = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(tf.caches.get("default").unwrap().ttl, "72h");
    }

    #[test]
    fn merge_version_mismatch_errors() {
        let mut a = Taskfile {
            version: Some("3".to_string()),
            ..Default::default()
        };
        let b = Taskfile {
            version: Some("2".to_string()),
            ..Default::default()
        };
        let err = a.merge(&b, &Include::default()).unwrap_err();
        assert!(err.contains("versions should match"));
    }

    #[test]
    fn merge_rejects_included_dotenv() {
        let mut a = Taskfile {
            version: Some("3".to_string()),
            ..Default::default()
        };
        let b = Taskfile {
            version: Some("3".to_string()),
            dotenv: vec![".env".to_string()],
            ..Default::default()
        };
        let err = a.merge(&b, &Include::default()).unwrap_err();
        assert!(err.contains("dotenv declarations"));
    }

    #[test]
    fn merge_namespaces_included_tasks() {
        let mut a = Taskfile {
            version: Some("3".to_string()),
            ..Default::default()
        };
        let mut b_tasks = Tasks::new();
        b_tasks.set("build".to_string(), super::super::task::Task::default());
        let b = Taskfile {
            version: Some("3".to_string()),
            tasks: b_tasks,
            ..Default::default()
        };
        let include = Include {
            namespace: "sub".to_string(),
            ..Default::default()
        };
        a.merge(&b, &include).unwrap();
        assert!(a.tasks.get("sub:build").is_some());
    }

    #[test]
    fn merge_silent_propagates_to_tasks() {
        let mut a = Taskfile {
            version: Some("3".to_string()),
            ..Default::default()
        };
        let mut b_tasks = Tasks::new();
        b_tasks.set("build".to_string(), super::super::task::Task::default());
        let b = Taskfile {
            version: Some("3".to_string()),
            silent: true,
            tasks: b_tasks,
            ..Default::default()
        };
        let include = Include {
            namespace: "sub".to_string(),
            ..Default::default()
        };
        a.merge(&b, &include).unwrap();
        assert_eq!(a.tasks.get("sub:build").unwrap().silent, Some(true));
    }
}
