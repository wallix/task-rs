//! An insertion-ordered map of variable names to [`Var`] values.

use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::dialect::Dialect;
use super::error::TaskfileDecodeError;
use super::include::Include;
use super::var::Var;

/// An ordered map of variable names to values. Insertion order is preserved
/// because Taskfiles rely on it for evaluation order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vars {
    om: IndexMap<String, Var>,
}

/// A key-value pair used to initialize a [`Vars`] map.
#[derive(Clone, Debug)]
pub struct VarElement {
    pub key: String,
    pub value: Var,
}

impl Vars {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a map initialized with the given elements, in order.
    pub fn from_elements(els: impl IntoIterator<Item = VarElement>) -> Self {
        let mut vars = Self::new();
        for el in els {
            vars.set(el.key, el.value);
        }
        vars
    }

    /// Returns the number of variables.
    pub fn len(&self) -> usize {
        self.om.len()
    }

    /// Reports whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.om.is_empty()
    }

    /// Returns the value for `key`, if present.
    pub fn get(&self, key: &str) -> Option<&Var> {
        self.om.get(key)
    }

    /// Inserts or updates the value for `key`, returning whether it existed.
    pub fn set(&mut self, key: String, value: Var) -> bool {
        self.om.insert(key, value).is_some()
    }

    /// Iterates over the pairs in insertion order.
    pub fn all(&self) -> impl Iterator<Item = (&String, &Var)> {
        self.om.iter()
    }

    /// Stamps every variable's template dialect. Called when a Taskfile is read
    /// so each variable resolves in its origin file's dialect even after the
    /// include merge moves it into another map.
    pub fn set_dialect(&mut self, dialect: Dialect) {
        for var in self.om.values_mut() {
            var.dialect = dialect;
        }
    }

    /// Iterates over the keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.om.keys()
    }

    /// Iterates over the values in insertion order.
    pub fn values(&self) -> impl Iterator<Item = &Var> {
        self.om.values()
    }

    /// Converts to an unordered map containing only the static variables.
    /// Dynamic (`sh`) variables are skipped so templates see `<no value>`.
    pub fn to_cache_map(&self) -> IndexMap<String, Value> {
        let mut m = IndexMap::with_capacity(self.len());
        for (k, v) in self.all() {
            if let Some(sh) = &v.sh
                && !sh.is_empty()
            {
                continue;
            }
            let value = if v.live.is_some() {
                v.live.clone()
            } else {
                v.value.clone()
            };
            if let Some(value) = value {
                m.insert(k.clone(), value);
            } else {
                m.insert(k.clone(), Value::Null);
            }
        }
        m
    }

    /// Merges `other` into `self`, appending new keys after existing ones. When
    /// `include` is an advanced import, each merged var's `dir` is set to the
    /// include directory.
    pub fn merge(&mut self, other: &Vars, include: Option<&Include>) {
        for (key, value) in other.all() {
            let mut value = value.clone();
            if let Some(inc) = include
                && inc.advanced_import
            {
                value.dir = inc.dir.clone();
            }
            self.om.insert(key.clone(), value);
        }
    }

    /// Merges `other` into `self` but keeps `other`'s entries first in order.
    pub fn reverse_merge(&mut self, other: &Vars, include: Option<&Include>) {
        let mut new_om: IndexMap<String, Var> = IndexMap::new();
        for (key, value) in other.all() {
            let mut value = value.clone();
            if let Some(inc) = include
                && inc.advanced_import
            {
                value.dir = inc.dir.clone();
            }
            new_om.insert(key.clone(), value);
        }
        for (key, value) in self.om.drain(..) {
            new_om.insert(key, value);
        }
        self.om = new_om;
    }
}

impl<'de> Deserialize<'de> for Vars {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::Mapping(map) => {
                let mut vars = Vars::new();
                for (k, v) in map {
                    let key = k.as_str().map(str::to_string).ok_or_else(|| {
                        de::Error::custom(TaskfileDecodeError::type_message("vars"))
                    })?;
                    let var: Var = serde_yaml_ng::from_value(v)
                        .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                    vars.set(key, var);
                }
                Ok(vars)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message("vars"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_insertion_order() {
        let input = "PARAM1: VALUE1\nPARAM2: VALUE2\n";
        let vars: Vars = serde_yaml_ng::from_str(input).unwrap();
        let keys: Vec<&String> = vars.keys().collect();
        assert_eq!(keys, vec!["PARAM1", "PARAM2"]);
        assert_eq!(
            vars.get("PARAM1").unwrap().value,
            Some(Value::String("VALUE1".to_string()))
        );
    }

    #[test]
    fn to_cache_map_skips_dynamic() {
        let input = "A: 1\nB:\n  sh: echo hi\n";
        let vars: Vars = serde_yaml_ng::from_str(input).unwrap();
        let m = vars.to_cache_map();
        assert!(m.contains_key("A"));
        assert!(!m.contains_key("B"));
    }

    #[test]
    fn reverse_merge_keeps_other_first() {
        let mut base = Vars::from_elements([VarElement {
            key: "B".to_string(),
            value: Var::default(),
        }]);
        let other = Vars::from_elements([VarElement {
            key: "A".to_string(),
            value: Var::default(),
        }]);
        base.reverse_merge(&other, None);
        let keys: Vec<&String> = base.keys().collect();
        assert_eq!(keys, vec!["A", "B"]);
    }
}
