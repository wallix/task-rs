//! Variables that must be set (and optionally within an enum) for a task to run.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;

/// The set of required variables for a task.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Requires {
    #[serde(default)]
    pub vars: Vec<VarsWithValidation>,
}

/// A required variable with an optional set of allowed values.
///
/// Accepts a bare string (the variable name) or a mapping with `name` and an
/// optional `enum` list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VarsWithValidation {
    pub name: String,
    pub enum_values: Vec<String>,
}

impl<'de> Deserialize<'de> for VarsWithValidation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(name) => Ok(VarsWithValidation {
                name,
                enum_values: Vec::new(),
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    name: String,
                    #[serde(default, rename = "enum")]
                    enum_values: Vec<String>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(VarsWithValidation {
                    name: raw.name,
                    enum_values: raw.enum_values,
                })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "requires",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_name() {
        let v: VarsWithValidation = serde_yaml_ng::from_str("FOO").unwrap();
        assert_eq!(v.name, "FOO");
        assert!(v.enum_values.is_empty());
    }

    #[test]
    fn mapping_with_enum() {
        let input = "name: FOO\nenum: [a, b, c]\n";
        let v: VarsWithValidation = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(v.name, "FOO");
        assert_eq!(v.enum_values, vec!["a", "b", "c"]);
    }

    #[test]
    fn requires_block() {
        let input = "vars:\n  - FOO\n  - name: BAR\n    enum: [x, y]\n";
        let r: Requires = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(r.vars.len(), 2);
        assert_eq!(r.vars[0].name, "FOO");
        assert_eq!(r.vars[1].name, "BAR");
        assert_eq!(r.vars[1].enum_values, vec!["x", "y"]);
    }
}
