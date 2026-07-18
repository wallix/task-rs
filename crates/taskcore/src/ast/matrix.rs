//! An ordered map of matrix variable names to value lists or references.

use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;

/// A list of values for a matrix key, or a reference to another variable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatrixRow {
    pub ref_: String,
    pub value: Vec<Value>,
}

/// A key-value pair used to initialize a [`Matrix`].
#[derive(Clone, Debug)]
pub struct MatrixElement {
    pub key: String,
    pub value: MatrixRow,
}

/// An ordered map of matrix variable names to rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Matrix {
    om: IndexMap<String, MatrixRow>,
}

impl Matrix {
    /// Creates an empty matrix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a matrix initialized with the given elements, in order.
    pub fn from_elements(els: impl IntoIterator<Item = MatrixElement>) -> Self {
        let mut matrix = Self::new();
        for el in els {
            matrix.set(el.key, el.value);
        }
        matrix
    }

    /// Returns the number of rows.
    pub fn len(&self) -> usize {
        self.om.len()
    }

    /// Reports whether the matrix is empty.
    pub fn is_empty(&self) -> bool {
        self.om.is_empty()
    }

    /// Returns the row for `key`, if present.
    pub fn get(&self, key: &str) -> Option<&MatrixRow> {
        self.om.get(key)
    }

    /// Inserts or updates the row for `key`, returning whether it existed.
    pub fn set(&mut self, key: String, value: MatrixRow) -> bool {
        self.om.insert(key, value).is_some()
    }

    /// Iterates over the rows in insertion order.
    pub fn all(&self) -> impl Iterator<Item = (&String, &MatrixRow)> {
        self.om.iter()
    }

    /// Iterates over the keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.om.keys()
    }

    /// Iterates over the rows in insertion order.
    pub fn values(&self) -> impl Iterator<Item = &MatrixRow> {
        self.om.values()
    }
}

impl<'de> Deserialize<'de> for Matrix {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::Mapping(map) => {
                let mut matrix = Matrix::new();
                for (k, v) in map {
                    let key = k.as_str().map(str::to_string).ok_or_else(|| {
                        de::Error::custom(TaskfileDecodeError::type_message("matrix"))
                    })?;
                    match v {
                        Value::Sequence(seq) => {
                            matrix.set(
                                key,
                                MatrixRow {
                                    ref_: String::new(),
                                    value: seq,
                                },
                            );
                        }
                        Value::Mapping(_) => {
                            #[derive(Deserialize)]
                            struct Ref {
                                #[serde(rename = "ref", default)]
                                r: String,
                            }
                            let refs: Ref = serde_yaml_ng::from_value(v)
                                .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                            matrix.set(
                                key,
                                MatrixRow {
                                    ref_: refs.r,
                                    value: Vec::new(),
                                },
                            );
                        }
                        _ => {
                            return Err(de::Error::custom(TaskfileDecodeError::message(
                                "matrix values must be an array or a reference",
                            )));
                        }
                    }
                }
                Ok(matrix)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "matrix",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_rows() {
        let input = "OS: [linux, windows]\nARCH: [amd64, arm64]\n";
        let m: Matrix = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("OS").unwrap().value.len(), 2);
        let keys: Vec<&String> = m.keys().collect();
        assert_eq!(keys, vec!["OS", "ARCH"]);
    }

    #[test]
    fn ref_row() {
        let input = "OS:\n  ref: SOME_VAR\n";
        let m: Matrix = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(m.get("OS").unwrap().ref_, "SOME_VAR");
        assert!(m.get("OS").unwrap().value.is_empty());
    }

    #[test]
    fn invalid_row() {
        let err = serde_yaml_ng::from_str::<Matrix>("OS: linux").unwrap_err();
        assert!(err.to_string().contains("array or a reference"));
    }
}
