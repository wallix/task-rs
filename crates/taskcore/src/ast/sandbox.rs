//! Per-task sandbox settings: which files a task's commands may touch.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;
use super::glob::Glob;

/// The `sandbox:` block of a task.
///
/// `sandbox: true` restricts the task's commands to its `sources` inside the
/// root directory; a mapping does the same and widens it with `reads` and
/// `writes`. `sandbox: false` (or no key) leaves the task unrestricted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sandbox {
    /// Files the commands may read without them being fingerprinted, in the
    /// `sources` glob syntax (e.g. `.git/**`).
    pub reads: Vec<Glob>,
    /// Directories the commands may change freely (build trees, caches),
    /// relative to the task directory. Missing ones are created before the
    /// task runs.
    pub writes: Vec<String>,
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
                    writes: Vec<String>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| E::custom(TaskfileDecodeError::wrap(e)))?;
                // `from:` is resolved for sources and generates only: here it
                // would silently grant nothing.
                if raw.reads.iter().any(|g| !g.from.is_empty()) {
                    return Err(E::custom(TaskfileDecodeError::message(
                        "sandbox.reads does not support `from:`; list the files, or add them to \
                         sources",
                    )));
                }
                Ok(Some(Self {
                    reads: raw.reads,
                    writes: raw.writes,
                }))
            }
            _ => Err(E::custom(TaskfileDecodeError::type_message("sandbox"))),
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
        serde_yaml_ng::from_str::<Holder>(yaml).unwrap().sandbox
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
    }

    #[test]
    fn rejects_unknown_keys_and_scalars() {
        assert!(serde_yaml_ng::from_str::<Holder>("sandbox:\n  write: [x]\n").is_err());
        assert!(serde_yaml_ng::from_str::<Holder>("sandbox: yes-please\n").is_err());
        assert!(serde_yaml_ng::from_str::<Holder>("sandbox: ~\n").is_err());
        let from = serde_yaml_ng::from_str::<Holder>("sandbox:\n  reads: [{from: deps}]\n");
        let err = from.err().unwrap().to_string();
        assert!(err.contains("does not support `from:`"), "{err}");
    }
}
