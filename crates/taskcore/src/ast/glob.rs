//! File patterns used in `sources` and `generates` lists.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use super::error::TaskfileDecodeError;

/// A file pattern. It accepts four YAML shapes:
///
/// - scalar: a plain glob pattern (`"src/**/*.go"`).
/// - `exclude:` a negated pattern.
/// - `glob:` + optional `fingerprint:`: the glob defines the file set while
///   `fingerprint` names a single representative file to hash instead.
/// - `from:` references generates from other tasks (e.g. `deps`, `cmds`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Glob {
    pub glob: String,
    pub negate: bool,
    /// When set, only this file is hashed for up-to-date checks.
    pub fingerprint: String,
    /// When set, references generates from other tasks (e.g. `deps`).
    pub from: String,
}

impl<'de> Deserialize<'de> for Glob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::String(glob) => Ok(Glob {
                glob,
                ..Default::default()
            }),
            Value::Mapping(_) => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    exclude: String,
                    #[serde(default)]
                    glob: String,
                    #[serde(default)]
                    fingerprint: String,
                    #[serde(default)]
                    from: String,
                }
                let raw: Raw = serde_yaml_ng::from_value(value)
                    .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                let mut g = Glob::default();
                if !raw.from.is_empty() {
                    g.from = raw.from;
                    return Ok(g);
                }
                if !raw.exclude.is_empty() {
                    g.glob = raw.exclude;
                    g.negate = true;
                } else {
                    g.glob = raw.glob;
                }
                g.fingerprint = raw.fingerprint;
                Ok(g)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message("glob"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar() {
        let g: Glob = serde_yaml_ng::from_str(r#""src/**/*.go""#).unwrap();
        assert_eq!(g.glob, "src/**/*.go");
        assert!(!g.negate);
        assert!(g.fingerprint.is_empty());
    }

    #[test]
    fn exclude() {
        let g: Glob = serde_yaml_ng::from_str(r#"exclude: "vendor/**""#).unwrap();
        assert_eq!(g.glob, "vendor/**");
        assert!(g.negate);
        assert!(g.fingerprint.is_empty());
    }

    #[test]
    fn glob_with_fingerprint() {
        let input =
            "\nglob: \"node_modules/**/*\"\nfingerprint: \"node_modules/.yarn-state.yml\"\n";
        let g: Glob = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(g.glob, "node_modules/**/*");
        assert!(!g.negate);
        assert_eq!(g.fingerprint, "node_modules/.yarn-state.yml");
    }

    #[test]
    fn glob_without_fingerprint() {
        let g: Glob = serde_yaml_ng::from_str(r#"glob: "build/**/*""#).unwrap();
        assert_eq!(g.glob, "build/**/*");
        assert!(!g.negate);
        assert!(g.fingerprint.is_empty());
    }

    #[test]
    fn in_generates_list() {
        let input = "\n- \"build/**/*\"\n- exclude: \"build/tmp/**\"\n- glob: \"node_modules/**/*\"\n  fingerprint: \"node_modules/.yarn-state.yml\"\n";
        let globs: Vec<Glob> = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(globs.len(), 3);
        assert_eq!(globs[0].glob, "build/**/*");
        assert!(!globs[0].negate);
        assert!(globs[0].fingerprint.is_empty());
        assert_eq!(globs[1].glob, "build/tmp/**");
        assert!(globs[1].negate);
        assert!(globs[1].fingerprint.is_empty());
        assert_eq!(globs[2].glob, "node_modules/**/*");
        assert!(!globs[2].negate);
        assert_eq!(globs[2].fingerprint, "node_modules/.yarn-state.yml");
    }

    #[test]
    fn from_deps() {
        let g: Glob = serde_yaml_ng::from_str("from: deps").unwrap();
        assert!(g.glob.is_empty());
        assert!(!g.negate);
        assert!(g.fingerprint.is_empty());
        assert_eq!(g.from, "deps");
    }

    #[test]
    fn from_cmds() {
        let g: Glob = serde_yaml_ng::from_str("from: cmds").unwrap();
        assert!(g.glob.is_empty());
        assert_eq!(g.from, "cmds");
    }

    #[test]
    fn list_with_from() {
        let input = "\n- \"src/**/*.go\"\n- from: deps\n- exclude: \"vendor/**\"\n";
        let globs: Vec<Glob> = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(globs.len(), 3);
        assert_eq!(globs[0].glob, "src/**/*.go");
        assert!(globs[0].from.is_empty());
        assert!(globs[1].glob.is_empty());
        assert_eq!(globs[1].from, "deps");
        assert_eq!(globs[2].glob, "vendor/**");
        assert!(globs[2].negate);
        assert!(globs[2].from.is_empty());
    }
}
