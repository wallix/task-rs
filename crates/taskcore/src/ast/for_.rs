//! The `for` clause that expands a command or dependency into iterations.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;
use super::matrix::Matrix;

/// A loop source. Accepts a scalar (`from`), a list of literal values, or a
/// mapping with either `var` or `matrix` (never both).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct For {
    pub from: String,
    pub list: Vec<Value>,
    pub matrix: Option<Matrix>,
    pub var: String,
    pub split: String,
    pub as_: String,
}

impl<'de> Deserialize<'de> for For {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(from) => Ok(For {
                from,
                ..Default::default()
            }),
            Value::Sequence(list) => Ok(For {
                list,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    matrix: Option<Matrix>,
                    #[serde(default)]
                    var: String,
                    #[serde(default)]
                    split: String,
                    #[serde(default, rename = "as")]
                    as_: String,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                let matrix_len = raw.matrix.as_ref().map_or(0, Matrix::len);
                if raw.var.is_empty() && matrix_len == 0 {
                    return Err(de::Error::custom(TaskfileDecodeError::message(
                        "invalid keys in for",
                    )));
                }
                if !raw.var.is_empty() && matrix_len != 0 {
                    return Err(de::Error::custom(TaskfileDecodeError::message(
                        "cannot use both var and matrix in for",
                    )));
                }
                Ok(For {
                    matrix: raw.matrix,
                    var: raw.var,
                    split: raw.split,
                    as_: raw.as_,
                    ..Default::default()
                })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message("for"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_from() {
        let f: For = serde_yaml_ng::from_str("sources").unwrap();
        assert_eq!(f.from, "sources");
    }

    #[test]
    fn list_literal() {
        let f: For = serde_yaml_ng::from_str("[a, b, c]").unwrap();
        assert_eq!(f.list.len(), 3);
    }

    #[test]
    fn var_form() {
        let f: For = serde_yaml_ng::from_str("var: MY_VAR\nas: item\n").unwrap();
        assert_eq!(f.var, "MY_VAR");
        assert_eq!(f.as_, "item");
    }

    #[test]
    fn matrix_form() {
        let f: For = serde_yaml_ng::from_str("matrix:\n  OS: [linux]\n").unwrap();
        assert_eq!(f.matrix.unwrap().len(), 1);
    }

    #[test]
    fn both_var_and_matrix_errors() {
        let err = serde_yaml_ng::from_str::<For>("var: X\nmatrix:\n  OS: [linux]\n").unwrap_err();
        assert!(err.to_string().contains("both var and matrix"));
    }

    #[test]
    fn empty_mapping_errors() {
        let err = serde_yaml_ng::from_str::<For>("split: ','\n").unwrap_err();
        assert!(err.to_string().contains("invalid keys in for"));
    }
}
