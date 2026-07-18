//! A task dependency.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;
use super::for_::For;
use super::vars::Vars;

/// A dependency. Accepts a bare string (the task name) or a mapping with a
/// task call and optional `for`, `vars`, `silent`, and `fingerprint`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dep {
    pub task: String,
    pub for_: Option<For>,
    pub vars: Option<Vars>,
    pub silent: bool,
    pub fingerprint: Option<bool>,
}

impl<'de> Deserialize<'de> for Dep {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(task) => Ok(Dep {
                task,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    task: String,
                    #[serde(default, rename = "for")]
                    for_: Option<For>,
                    vars: Option<Vars>,
                    #[serde(default)]
                    silent: bool,
                    #[serde(default)]
                    fingerprint: Option<bool>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Dep {
                    task: raw.task,
                    for_: raw.for_,
                    vars: raw.vars,
                    silent: raw.silent,
                    fingerprint: raw.fingerprint,
                })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "dependency",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_task_name() {
        let d: Dep = serde_yaml_ng::from_str("task-name").unwrap();
        assert_eq!(d.task, "task-name");
    }

    #[test]
    fn task_call_with_vars() {
        let input = "task: another-task\nvars:\n  PARAM1: VALUE1\n";
        let d: Dep = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(d.task, "another-task");
        assert_eq!(d.vars.as_ref().unwrap().len(), 1);
    }
}
