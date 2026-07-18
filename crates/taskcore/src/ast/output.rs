//! The output style of a Taskfile or task.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;

/// The output style. Accepts a bare style name, or a mapping that must carry a
/// `group` key (setting the style to `group`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub group: OutputGroup,
}

impl Output {
    /// Reports whether a custom output style is set.
    pub fn is_set(&self) -> bool {
        !self.name.is_empty()
    }
}

impl<'de> Deserialize<'de> for Output {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(name) => Ok(Output {
                name,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    group: Option<OutputGroup>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                match raw.group {
                    Some(group) => Ok(Output {
                        name: "group".to_string(),
                        group,
                    }),
                    None => Err(de::Error::custom(TaskfileDecodeError::message(
                        "output style must have the \"group\" key when in mapping form",
                    ))),
                }
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "output",
            ))),
        }
    }
}

/// Style options specific to the `group` output style.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct OutputGroup {
    #[serde(default)]
    pub begin: String,
    #[serde(default)]
    pub end: String,
    #[serde(default, rename = "error_only")]
    pub error_only: bool,
}

impl OutputGroup {
    /// Reports whether custom begin/end markers are set.
    pub fn is_set(&self) -> bool {
        !self.begin.is_empty() || !self.end.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_name() {
        let o: Output = serde_yaml_ng::from_str("prefixed").unwrap();
        assert_eq!(o.name, "prefixed");
        assert!(o.is_set());
    }

    #[test]
    fn group_mapping() {
        let input = "group:\n  begin: START\n  end: END\n  error_only: true\n";
        let o: Output = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(o.name, "group");
        assert_eq!(o.group.begin, "START");
        assert!(o.group.error_only);
        assert!(o.group.is_set());
    }

    #[test]
    fn mapping_without_group_errors() {
        let err = serde_yaml_ng::from_str::<Output>("other: 1\n").unwrap_err();
        assert!(err.to_string().contains("must have the \"group\" key"));
    }
}
