//! Per-task remote cache configuration and taskfile-level cache models.

use std::collections::HashMap;

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;

/// Remote cache and distributed-lock settings.
///
/// At task level, `cache:` either names a model to inherit (`cache: default`)
/// or provides a mapping of overrides. In a mapping, `enabled` is treated as
/// an explicit bool when it is a YAML boolean, otherwise as a template
/// condition stored in `if_`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cache {
    /// Model name to inherit from (empty means no inheritance).
    pub inherit: String,
    /// Explicit enable flag; `None` means always enabled when the block is present.
    pub enabled: Option<bool>,
    /// Template condition for a dynamic enable check.
    pub if_: String,
    /// Template string producing the cache URL.
    pub url: String,
    /// Template string producing the lock URL.
    pub lock: String,
    /// Cached asset TTL (e.g. `48h`, `7d`).
    pub ttl: String,
    /// Maximum wait for lock contention (e.g. `5m`, `1h`).
    pub lock_timeout: String,
}

impl<'de> Deserialize<'de> for Cache {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        let mut c = Cache::default();
        match value {
            Value::String(s) => {
                c.inherit = s;
                Ok(c)
            }
            Value::Mapping(map) => {
                for (k, v) in map {
                    let Some(key) = k.as_str() else { continue };
                    match key {
                        "inherit" => c.inherit = value_as_string(&v),
                        "enabled" => match v {
                            Value::Bool(b) => c.enabled = Some(b),
                            other => c.if_ = value_as_string(&other),
                        },
                        "url" => c.url = value_as_string(&v),
                        "lock" => c.lock = value_as_string(&v),
                        "ttl" => c.ttl = value_as_string(&v),
                        "lock_timeout" => c.lock_timeout = value_as_string(&v),
                        _ => {}
                    }
                }
                Ok(c)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "cache",
            ))),
        }
    }
}

/// Returns the scalar rendering of a YAML value, matching the raw node value
/// the Go decoder reads. Non-scalar values yield an empty string.
fn value_as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        _ => String::new(),
    }
}

/// A map of named cache models defined at the taskfile level.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Caches(pub HashMap<String, Cache>);

impl Caches {
    /// Returns the number of models.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether there are no models.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the model with the given name, if present.
    pub fn get(&self, name: &str) -> Option<&Cache> {
        self.0.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_string_sets_inherit() {
        let c: Cache = serde_yaml_ng::from_str("default").unwrap();
        assert_eq!(c.inherit, "default");
        assert!(c.enabled.is_none());
        assert!(c.ttl.is_empty());
    }

    #[test]
    fn mapping_with_bool_enabled() {
        let c: Cache = serde_yaml_ng::from_str("enabled: true\nurl: redis://host/key").unwrap();
        assert_eq!(c.enabled, Some(true));
        assert_eq!(c.url, "redis://host/key");
    }

    #[test]
    fn mapping_with_string_enabled_becomes_if() {
        let c: Cache = serde_yaml_ng::from_str(r#"enabled: '{{ne .FOO ""}}'"#).unwrap();
        assert!(c.enabled.is_none());
        assert_eq!(c.if_, r#"{{ne .FOO ""}}"#);
    }

    #[test]
    fn mapping_with_url_and_lock() {
        let c: Cache =
            serde_yaml_ng::from_str("url: file:///tmp/x.zip\nlock: redis://host/lock").unwrap();
        assert_eq!(c.url, "file:///tmp/x.zip");
        assert_eq!(c.lock, "redis://host/lock");
    }

    #[test]
    fn mapping_with_inherit_and_url_override() {
        let c: Cache = serde_yaml_ng::from_str("inherit: doc\nurl: file:///override").unwrap();
        assert_eq!(c.inherit, "doc");
        assert_eq!(c.url, "file:///override");
    }

    #[test]
    fn mapping_with_ttl() {
        let c: Cache = serde_yaml_ng::from_str("url: redis://host/key\nttl: 48h").unwrap();
        assert_eq!(c.url, "redis://host/key");
        assert_eq!(c.ttl, "48h");
    }

    #[test]
    fn ttl_in_mapping_with_other_fields() {
        let c: Cache = serde_yaml_ng::from_str(
            "inherit: doc\nurl: file:///tmp/x.zip\nlock: redis://host/lock\nttl: 7d",
        )
        .unwrap();
        assert_eq!(c.inherit, "doc");
        assert_eq!(c.url, "file:///tmp/x.zip");
        assert_eq!(c.lock, "redis://host/lock");
        assert_eq!(c.ttl, "7d");
    }

    #[test]
    fn ttl_absent_defaults_empty() {
        let c: Cache = serde_yaml_ng::from_str("url: redis://host/key").unwrap();
        assert!(c.ttl.is_empty());
    }
}
