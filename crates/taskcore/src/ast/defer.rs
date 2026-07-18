//! The `defer` clause used inside a command list.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;
use super::vars::Vars;

/// A deferred action: either a shell command or a task call.
///
/// Accepts a bare string (the command) or a mapping. In mapping form the
/// command lives under the `defer` key (a deferred task uses `task`/`vars`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Defer {
    pub cmd: String,
    pub task: String,
    pub vars: Option<Vars>,
    pub silent: bool,
}

impl<'de> Deserialize<'de> for Defer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(cmd) => Ok(Defer {
                cmd,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default, rename = "defer")]
                    defer: String,
                    #[serde(default)]
                    task: String,
                    vars: Option<Vars>,
                    #[serde(default)]
                    silent: bool,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Defer {
                    cmd: raw.defer,
                    task: raw.task,
                    vars: raw.vars,
                    silent: raw.silent,
                })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "defer",
            ))),
        }
    }
}
