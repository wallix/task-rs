//! A single command inside a task's `cmds` list.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::defer::Defer;
use super::error::TaskfileDecodeError;
use super::for_::For;
use super::platforms::Platform;
use super::vars::Vars;

/// A task command. It is polymorphic:
///
/// - a bare string is a shell command;
/// - a mapping with `defer` is a deferred command or task call;
/// - a mapping with `task` is a task call;
/// - a mapping with `cmd` is a shell command with options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cmd {
    pub cmd: String,
    pub task: String,
    pub for_: Option<For>,
    pub if_: String,
    pub silent: bool,
    pub set: Vec<String>,
    pub shopt: Vec<String>,
    pub vars: Option<Vars>,
    pub ignore_error: bool,
    pub defer: bool,
    pub platforms: Vec<Platform>,
}

impl<'de> Deserialize<'de> for Cmd {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(cmd) => Ok(Cmd {
                cmd,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    cmd: String,
                    #[serde(default)]
                    task: String,
                    #[serde(default, rename = "for")]
                    for_: Option<For>,
                    #[serde(default, rename = "if")]
                    if_: String,
                    #[serde(default)]
                    silent: bool,
                    #[serde(default)]
                    set: Vec<String>,
                    #[serde(default)]
                    shopt: Vec<String>,
                    vars: Option<Vars>,
                    #[serde(default, rename = "ignore_error")]
                    ignore_error: bool,
                    defer: Option<Defer>,
                    #[serde(default)]
                    platforms: Vec<Platform>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;

                let mut c = Cmd::default();
                if let Some(d) = raw.defer {
                    // A deferred command.
                    if !d.cmd.is_empty() {
                        c.defer = true;
                        c.cmd = d.cmd;
                        c.silent = raw.silent;
                        return Ok(c);
                    }
                    // A deferred task call.
                    if !d.task.is_empty() {
                        c.defer = true;
                        c.task = d.task;
                        c.vars = d.vars;
                        c.silent = d.silent;
                        return Ok(c);
                    }
                    return Ok(c);
                }

                // A task call.
                if !raw.task.is_empty() {
                    c.task = raw.task;
                    c.vars = raw.vars;
                    c.for_ = raw.for_;
                    c.if_ = raw.if_;
                    c.silent = raw.silent;
                    c.ignore_error = raw.ignore_error;
                    return Ok(c);
                }

                // A command with additional options.
                if !raw.cmd.is_empty() {
                    c.cmd = raw.cmd;
                    c.for_ = raw.for_;
                    c.if_ = raw.if_;
                    c.silent = raw.silent;
                    c.set = raw.set;
                    c.shopt = raw.shopt;
                    c.ignore_error = raw.ignore_error;
                    c.platforms = raw.platforms;
                    return Ok(c);
                }

                Err(de::Error::custom(TaskfileDecodeError::message(
                    "invalid keys in command",
                )))
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "command",
            ))),
        }
    }
}
