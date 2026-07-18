//! A shell condition that must succeed before a task runs.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;

/// A precondition: a shell command plus the message shown when it fails.
///
/// Accepts a bare string (the command, with a default message) or a mapping
/// with `sh` and optional `msg`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Precondition {
    pub sh: String,
    pub msg: String,
}

impl<'de> Deserialize<'de> for Precondition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(cmd) => Ok(Precondition {
                msg: format!("`{cmd}` failed"),
                sh: cmd,
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    sh: String,
                    #[serde(default)]
                    msg: String,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                let msg = if raw.msg.is_empty() {
                    format!("{} failed", raw.sh)
                } else {
                    raw.msg
                };
                Ok(Precondition { sh: raw.sh, msg })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "precondition",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precondition_parse() {
        struct Case {
            content: &'static str,
            expected: Precondition,
        }
        let cases = [
            Case {
                content: "test -f foo.txt",
                expected: Precondition {
                    sh: "test -f foo.txt".to_string(),
                    msg: "`test -f foo.txt` failed".to_string(),
                },
            },
            Case {
                content: "sh: '[ 1 = 0 ]'",
                expected: Precondition {
                    sh: "[ 1 = 0 ]".to_string(),
                    msg: "[ 1 = 0 ] failed".to_string(),
                },
            },
            Case {
                content: "\nsh: \"[ 1 = 2 ]\"\nmsg: \"1 is not 2\"\n",
                expected: Precondition {
                    sh: "[ 1 = 2 ]".to_string(),
                    msg: "1 is not 2".to_string(),
                },
            },
        ];
        for case in cases {
            let p: Precondition = serde_yaml_ng::from_str(case.content).unwrap();
            assert_eq!(p, case.expected);
        }
    }
}
