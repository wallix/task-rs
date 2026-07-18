//! Program version reporting.
//!
//! The base version is embedded from `version.txt`, which the release
//! script keeps up to date. Commit hash and dirty status are injected at
//! build time through the optional `TASK_COMMIT` and `TASK_DIRTY`
//! environment variables (a build script or CI populates them); when absent
//! only the base version is reported.

/// The embedded release version, trimmed of surrounding whitespace.
fn version() -> &'static str {
    include_str!("version.txt").trim()
}

/// The abbreviated commit hash injected at build time, if any (first seven
/// characters, matching the Go build).
fn commit() -> Option<&'static str> {
    match option_env!("TASK_COMMIT") {
        Some(c) if !c.is_empty() => Some(c.get(..7).unwrap_or(c)),
        _ => None,
    }
}

/// Whether the working tree was dirty at build time.
fn dirty() -> bool {
    matches!(option_env!("TASK_DIRTY"), Some("true" | "1"))
}

/// Returns the release version of Task.
pub fn get_version() -> &'static str {
    version()
}

/// Returns the version with build metadata (commit hash and dirty marker)
/// appended when available, formatted as `<version>+<commit>[.dirty]`.
pub fn get_version_with_build_info() -> String {
    let mut metadata: Vec<&str> = Vec::new();
    if let Some(c) = commit() {
        metadata.push(c);
    }
    if dirty() {
        metadata.push("dirty");
    }
    if metadata.is_empty() {
        return version().to_string();
    }
    format!("{}+{}", version(), metadata.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn version_is_semver_like() {
        let v = get_version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH, got {v}");
        for p in parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "non-numeric component in {v}"
            );
        }
    }

    #[test]
    fn version_matches_changelog() {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("../../CHANGELOG.md");
        let changelog = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        // First "## v3.54.0"-style header in the file.
        let changelog_version = changelog
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("## v")?;
                let ver: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if ver.split('.').count() == 3 {
                    Some(ver)
                } else {
                    None
                }
            })
            .expect("no version header found in CHANGELOG.md");

        assert_eq!(
            changelog_version,
            get_version(),
            "version.txt ({}) does not match latest CHANGELOG entry ({})",
            get_version(),
            changelog_version
        );
    }

    #[test]
    fn build_info_without_metadata_equals_version() {
        // In a plain test build no TASK_COMMIT/TASK_DIRTY are set.
        if commit().is_none() && !dirty() {
            assert_eq!(get_version_with_build_info(), get_version());
        }
    }
}
