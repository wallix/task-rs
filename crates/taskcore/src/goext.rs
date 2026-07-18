//! Recognition of Go's known `GOOS`/`GOARCH` values, used to distinguish
//! platform selectors from ordinary identifiers.

/// The set of operating-system names Go recognizes.
const KNOWN_OS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "js",
    "linux",
    "nacl",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "windows",
    "zos",
    "__test__",
];

/// The set of architecture names Go recognizes.
const KNOWN_ARCH: &[&str] = &[
    "386",
    "amd64",
    "amd64p32",
    "arm",
    "armbe",
    "arm64",
    "arm64be",
    "loong64",
    "mips",
    "mipsle",
    "mips64",
    "mips64le",
    "mips64p32",
    "mips64p32le",
    "ppc",
    "ppc64",
    "ppc64le",
    "riscv",
    "riscv64",
    "s390",
    "s390x",
    "sparc",
    "sparc64",
    "wasm",
];

/// Reports whether `s` is a recognized operating-system name.
pub fn is_known_os(s: &str) -> bool {
    KNOWN_OS.contains(&s)
}

/// Reports whether `s` is a recognized architecture name.
pub fn is_known_arch(s: &str) -> bool {
    KNOWN_ARCH.contains(&s)
}

/// An error raised when a Go-style duration string cannot be parsed.
#[derive(Debug, PartialEq, Eq)]
pub struct DurationError(String);

impl std::fmt::Display for DurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid duration {:?}", self.0)
    }
}

impl std::error::Error for DurationError {}

/// Parses a Go-style duration string (e.g. `300ms`, `1.5s`, `2h45m`) into a
/// [`std::time::Duration`]. Supported units are `ns`, `us`/`µs`, `ms`, `s`,
/// `m`, and `h`. A leading sign is accepted; a bare `0` is zero. This is a small
/// hand-rolled parser matching Go's `time.ParseDuration` for the subset the
/// Taskfile cache lock timeout uses.
pub fn parse_duration(input: &str) -> Result<std::time::Duration, DurationError> {
    let err = || DurationError(input.to_string());
    let s = input.trim();
    if s.is_empty() {
        return Err(err());
    }
    if s == "0" {
        return Ok(std::time::Duration::ZERO);
    }

    let (neg, mut rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if neg {
        // Negative durations are not meaningful for a timeout.
        return Err(err());
    }
    if rest.is_empty() {
        return Err(err());
    }

    let mut total = 0f64;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(err)?;
        if num_end == 0 {
            return Err(err());
        }
        let value: f64 = rest
            .get(..num_end)
            .ok_or_else(err)?
            .parse()
            .map_err(|_| err())?;
        let after = rest.get(num_end..).ok_or_else(err)?;
        let (unit, tail) = split_unit(after).ok_or_else(err)?;
        let seconds = match unit {
            "ns" => value / 1e9,
            "us" | "µs" | "μs" => value / 1e6,
            "ms" => value / 1e3,
            "s" => value,
            "m" => value * 60.0,
            "h" => value * 3600.0,
            _ => return Err(err()),
        };
        total += seconds;
        rest = tail;
    }
    Ok(std::time::Duration::from_secs_f64(total))
}

/// Splits a leading unit token off `s`, returning `(unit, remainder)`.
fn split_unit(s: &str) -> Option<(&str, &str)> {
    for unit in ["ns", "us", "µs", "μs", "ms", "s", "m", "h"] {
        if let Some(tail) = s.strip_prefix(unit) {
            return Some((unit, tail));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_os() {
        assert!(is_known_os("linux"));
        assert!(is_known_os("windows"));
        assert!(is_known_os("darwin"));
        assert!(!is_known_os("beos"));
        assert!(!is_known_os(""));
    }

    #[test]
    fn known_arch() {
        assert!(is_known_arch("amd64"));
        assert!(is_known_arch("arm64"));
        assert!(is_known_arch("wasm"));
        assert!(!is_known_arch("x86_64"));
        assert!(!is_known_arch(""));
    }

    #[test]
    fn parse_duration_units() {
        use std::time::Duration;
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("300ms").unwrap(), Duration::from_millis(300));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_duration("2h45m").unwrap(),
            Duration::from_secs(2 * 3600 + 45 * 60)
        );
    }

    #[test]
    fn parse_duration_rejects_bad_input() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("-5s").is_err());
    }
}
