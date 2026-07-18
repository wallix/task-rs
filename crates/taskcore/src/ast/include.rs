//! Included Taskfiles and the ordered map of namespaces to includes.

use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;
use super::vars::Vars;

/// Information about an included Taskfile.
///
/// Accepts a bare string (the Taskfile path) or a mapping. The mapping form is
/// an "advanced import" and enables directory/vars propagation during merge.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Include {
    pub namespace: String,
    pub taskfile: String,
    pub dir: String,
    pub optional: bool,
    pub internal: bool,
    pub aliases: Vec<String>,
    pub excludes: Vec<String>,
    pub advanced_import: bool,
    pub vars: Option<Vars>,
    pub flatten: bool,
}

impl<'de> Deserialize<'de> for Include {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(taskfile) => Ok(Include {
                taskfile,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    taskfile: String,
                    #[serde(default)]
                    dir: String,
                    #[serde(default)]
                    optional: bool,
                    #[serde(default)]
                    internal: bool,
                    #[serde(default)]
                    flatten: bool,
                    #[serde(default)]
                    aliases: Vec<String>,
                    #[serde(default)]
                    excludes: Vec<String>,
                    vars: Option<Vars>,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                Ok(Include {
                    taskfile: raw.taskfile,
                    dir: raw.dir,
                    optional: raw.optional,
                    internal: raw.internal,
                    aliases: raw.aliases,
                    excludes: raw.excludes,
                    advanced_import: true,
                    vars: raw.vars,
                    flatten: raw.flatten,
                    ..Default::default()
                })
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "include",
            ))),
        }
    }
}

/// A key-value pair used to initialize an [`Includes`] map.
#[derive(Clone, Debug)]
pub struct IncludeElement {
    pub key: String,
    pub value: Include,
}

/// An ordered map of namespaces to includes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Includes {
    om: IndexMap<String, Include>,
}

impl Includes {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a map initialized with the given elements, in order.
    pub fn from_elements(els: impl IntoIterator<Item = IncludeElement>) -> Self {
        let mut includes = Self::new();
        for el in els {
            includes.set(el.key, el.value);
        }
        includes
    }

    /// Returns the number of includes.
    pub fn len(&self) -> usize {
        self.om.len()
    }

    /// Reports whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.om.is_empty()
    }

    /// Returns the include for `key`, if present.
    pub fn get(&self, key: &str) -> Option<&Include> {
        self.om.get(key)
    }

    /// Inserts or updates the include for `key`, returning whether it existed.
    pub fn set(&mut self, key: String, value: Include) -> bool {
        self.om.insert(key, value).is_some()
    }

    /// Iterates over the pairs in insertion order.
    pub fn all(&self) -> impl Iterator<Item = (&String, &Include)> {
        self.om.iter()
    }

    /// Iterates over the keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.om.keys()
    }

    /// Iterates over the values in insertion order.
    pub fn values(&self) -> impl Iterator<Item = &Include> {
        self.om.values()
    }
}

impl<'de> Deserialize<'de> for Includes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::Mapping(map) => {
                let mut includes = Includes::new();
                for (k, v) in map {
                    let key = k.as_str().map(str::to_string).ok_or_else(|| {
                        de::Error::custom(TaskfileDecodeError::type_message("includes"))
                    })?;
                    let mut include: Include = serde_yaml_ng::from_value(v)
                        .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                    include.namespace = key.clone();
                    includes.set(key, include);
                }
                Ok(includes)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "includes",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_include() {
        let i: Include = serde_yaml_ng::from_str("./other/Taskfile.yml").unwrap();
        assert_eq!(i.taskfile, "./other/Taskfile.yml");
        assert!(!i.advanced_import);
    }

    #[test]
    fn advanced_include() {
        let input = "taskfile: ./sub/Taskfile.yml\ndir: ./sub\ninternal: true\n";
        let i: Include = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(i.taskfile, "./sub/Taskfile.yml");
        assert_eq!(i.dir, "./sub");
        assert!(i.internal);
        assert!(i.advanced_import);
    }

    #[test]
    fn includes_map_sets_namespace_and_order() {
        let input = "docs: ./docs/Taskfile.yml\nlib:\n  taskfile: ./lib/Taskfile.yml\n";
        let includes: Includes = serde_yaml_ng::from_str(input).unwrap();
        let keys: Vec<&String> = includes.keys().collect();
        assert_eq!(keys, vec!["docs", "lib"]);
        assert_eq!(includes.get("docs").unwrap().namespace, "docs");
        assert!(includes.get("lib").unwrap().advanced_import);
    }
}
