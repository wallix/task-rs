//! A single Taskfile variable, static or dynamic.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::dialect::Dialect;
use super::error::TaskfileDecodeError;

/// A variable value. It is either a static value (any scalar, list, or map),
/// a shell command (`sh`), a reference to another variable (`ref`), or a `map`
/// literal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Var {
    /// The static value, or the `map` literal.
    pub value: Option<Value>,
    /// The live (resolved) value, populated during compilation.
    pub live: Option<Value>,
    /// A shell command whose output becomes the value.
    pub sh: Option<String>,
    /// A reference to another variable.
    pub ref_: String,
    /// The directory in which a dynamic variable is evaluated.
    pub dir: String,
    /// The template dialect this variable's value/`sh` string is authored in,
    /// stamped from the owning Taskfile's dialect when the file is read.
    pub dialect: Dialect,
}

impl Var {
    /// Builds a static string variable.
    pub fn from_string(value: impl Into<String>) -> Self {
        Var {
            value: Some(Value::String(value.into())),
            ..Default::default()
        }
    }

    /// Builds a static boolean variable.
    pub fn from_bool(value: bool) -> Self {
        Var {
            value: Some(Value::Bool(value)),
            ..Default::default()
        }
    }

    /// Builds a static variable holding a list of strings.
    pub fn from_string_list(values: impl IntoIterator<Item = String>) -> Self {
        let seq = values.into_iter().map(Value::String).collect();
        Var {
            value: Some(Value::Sequence(seq)),
            ..Default::default()
        }
    }
}

impl<'de> Deserialize<'de> for Var {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let node = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        if let Value::Mapping(map) = &node {
            let key = map
                .iter()
                .next()
                .and_then(|(k, _)| k.as_str())
                .unwrap_or("<none>")
                .to_string();
            match key.as_str() {
                "sh" | "ref" | "map" => {
                    // The YAML keys are `sh`, `ref`, `map`; `ref` is a Rust
                    // keyword so it is decoded via an explicit alias.
                    #[derive(Deserialize)]
                    struct RawKeys {
                        sh: Option<String>,
                        #[serde(rename = "ref")]
                        r: Option<String>,
                        map: Option<Value>,
                    }
                    let raw: RawKeys = serde_yaml_ng::from_value(node)
                        .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                    Ok(Var {
                        sh: raw.sh,
                        ref_: raw.r.unwrap_or_default(),
                        value: raw.map,
                        ..Default::default()
                    })
                }
                other => Err(de::Error::custom(TaskfileDecodeError::message(format!(
                    "\"{other}\" is not a valid variable type. Try \"sh\", \"ref\", \"map\" or using a scalar value"
                )))),
            }
        } else {
            Ok(Var {
                value: Some(node),
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_value() {
        let v: Var = serde_yaml_ng::from_str("hello").unwrap();
        assert_eq!(v.value, Some(Value::String("hello".to_string())));
        assert!(v.sh.is_none());
    }

    #[test]
    fn sh_form() {
        let v: Var = serde_yaml_ng::from_str("sh: echo hi").unwrap();
        assert_eq!(v.sh.as_deref(), Some("echo hi"));
    }

    #[test]
    fn ref_form() {
        let v: Var = serde_yaml_ng::from_str("ref: OTHER").unwrap();
        assert_eq!(v.ref_, "OTHER");
    }

    #[test]
    fn invalid_type() {
        let err = serde_yaml_ng::from_str::<Var>("bogus: 1").unwrap_err();
        assert!(err.to_string().contains("not a valid variable type"));
    }

    #[test]
    fn constructors_build_static_values() {
        assert_eq!(
            Var::from_string("x").value,
            Some(Value::String("x".to_string()))
        );
        assert_eq!(Var::from_bool(true).value, Some(Value::Bool(true)));
        assert_eq!(
            Var::from_string_list(["a".to_string(), "b".to_string()]).value,
            Some(Value::Sequence(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
            ]))
        );
    }
}
