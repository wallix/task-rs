//! Interactive confirmation prompts shown before a task runs.

use serde::Deserialize;
use serde::de::{self, Deserializer};

use super::error::TaskfileDecodeError;
use crate::yaml::Value;

/// One or more prompt strings. Accepts a bare string or a list of strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prompt(pub Vec<String>);

impl<'de> Deserialize<'de> for Prompt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(s) => Ok(Prompt(vec![s])),
            Value::Sequence(_) => {
                let list: Vec<String> = crate::yaml::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Prompt(list))
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "prompt",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar() {
        let p: Prompt = crate::yaml::from_str("are you sure?").unwrap();
        assert_eq!(p.0, vec!["are you sure?"]);
    }

    #[test]
    fn sequence() {
        let p: Prompt = crate::yaml::from_str("[a, b]").unwrap();
        assert_eq!(p.0, vec!["a", "b"]);
    }
}
