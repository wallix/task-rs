//! Platform selectors: an OS name, an architecture name, or `os/arch`.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer};

use crate::goext;

use super::error::TaskfileDecodeError;

/// A GOOS/GOARCH selector. Either field may be empty when only one was given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

/// Raised when a platform string is not a valid OS, arch, or `os/arch` pair.
#[derive(Debug, PartialEq, Eq)]
pub struct ErrInvalidPlatform {
    pub platform: String,
}

impl fmt::Display for ErrInvalidPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid platform \"{}\"", self.platform)
    }
}

impl std::error::Error for ErrInvalidPlatform {}

impl Platform {
    /// Parses an `OS`, `Arch`, or `OS/Arch` string into this platform.
    pub fn parse_platform(&mut self, input: &str) -> Result<(), ErrInvalidPlatform> {
        let split_values: Vec<&str> = input.split('/').collect();
        let invalid = || ErrInvalidPlatform {
            platform: input.to_string(),
        };
        if split_values.len() > 2 {
            return Err(invalid());
        }
        let first = split_values.first().copied().unwrap_or_default();
        if self.parse_os_or_arch(first).is_err() {
            return Err(invalid());
        }
        if let Some(second) = split_values.get(1)
            && self.parse_arch(second).is_err()
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn parse_os_or_arch(&mut self, os_or_arch: &str) -> Result<(), String> {
        if os_or_arch.is_empty() {
            return Err("task: Blank OS/Arch value provided".to_string());
        }
        if goext::is_known_os(os_or_arch) {
            self.os = os_or_arch.to_string();
            return Ok(());
        }
        if goext::is_known_arch(os_or_arch) {
            self.arch = os_or_arch.to_string();
            return Ok(());
        }
        Err(format!(
            "task: Invalid OS/Arch value provided ({os_or_arch})"
        ))
    }

    fn parse_arch(&mut self, arch: &str) -> Result<(), String> {
        if arch.is_empty() {
            return Err("task: Blank Arch value provided".to_string());
        }
        if !self.arch.is_empty() {
            return Err("task: Multiple Arch values provided".to_string());
        }
        if goext::is_known_arch(arch) {
            self.arch = arch.to_string();
            return Ok(());
        }
        Err(format!("task: Invalid Arch value provided ({arch})"))
    }
}

impl<'de> Deserialize<'de> for Platform {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        let mut p = Platform::default();
        p.parse_platform(&s)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_parsing() {
        struct Case {
            input: &'static str,
            os: &'static str,
            arch: &'static str,
            error: &'static str,
        }
        let cases = [
            Case {
                input: "windows",
                os: "windows",
                arch: "",
                error: "",
            },
            Case {
                input: "linux",
                os: "linux",
                arch: "",
                error: "",
            },
            Case {
                input: "darwin",
                os: "darwin",
                arch: "",
                error: "",
            },
            Case {
                input: "386",
                os: "",
                arch: "386",
                error: "",
            },
            Case {
                input: "amd64",
                os: "",
                arch: "amd64",
                error: "",
            },
            Case {
                input: "arm64",
                os: "",
                arch: "arm64",
                error: "",
            },
            Case {
                input: "windows/386",
                os: "windows",
                arch: "386",
                error: "",
            },
            Case {
                input: "windows/amd64",
                os: "windows",
                arch: "amd64",
                error: "",
            },
            Case {
                input: "windows/arm64",
                os: "windows",
                arch: "arm64",
                error: "",
            },
            Case {
                input: "invalid",
                os: "",
                arch: "",
                error: r#"invalid platform "invalid""#,
            },
            Case {
                input: "invalid/invalid",
                os: "",
                arch: "",
                error: r#"invalid platform "invalid/invalid""#,
            },
            Case {
                input: "windows/invalid",
                os: "",
                arch: "",
                error: r#"invalid platform "windows/invalid""#,
            },
            Case {
                input: "invalid/amd64",
                os: "",
                arch: "",
                error: r#"invalid platform "invalid/amd64""#,
            },
        ];
        for case in cases {
            let mut p = Platform::default();
            let res = p.parse_platform(case.input);
            if !case.error.is_empty() {
                let err = res.expect_err("expected error");
                assert_eq!(case.error, err.to_string());
            } else {
                res.expect("expected success");
                assert_eq!(case.os, p.os);
                assert_eq!(case.arch, p.arch);
            }
        }
    }
}
