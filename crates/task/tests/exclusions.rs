//! `exclude:` in `sources`: what an exclusion removes (fork behaviour; Go Task
//! expands an exclusion with a walk of its own).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::*;
use std::path::Path;

/// The source files `--status --json` lists for `task`, relative to `dir`.
fn sources(dir: &Path, task: &str) -> Vec<String> {
    let r = run(dir, &["--status", "--json", task]);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
    let root = format!("{}/", dir.canonicalize().unwrap().display());
    let raw = format!("{}/", dir.display());
    v[0]["source_files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .map(|f| {
                    let f = f.as_str().unwrap();
                    f.strip_prefix(&root)
                        .or_else(|| f.strip_prefix(&raw))
                        .unwrap_or(f)
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The fixture with its links: `build/again` back to `build`, `link` to
/// `src`.
#[cfg(unix)]
fn staged() -> std::path::PathBuf {
    let dir = stage("exclusions");
    std::os::unix::fs::symlink(".", dir.join("build/again")).unwrap();
    std::os::unix::fs::symlink("src", dir.join("link")).unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn exclusions_match_paths_however_reached() {
    let dir = staged();
    assert_eq!(sources(&dir, "through-loop"), ["src/a.txt"]);
    assert_eq!(
        sources(&dir, "link-to-excluded"),
        ["Taskfile.yml", "gen", "input.txt"]
    );
}

#[test]
fn trailing_slash_and_empty_trees_match_directories_only() {
    let dir = stage("exclusions");
    assert_eq!(sources(&dir, "slash-absolute"), ["input.txt"]);
    assert_eq!(sources(&dir, "slash-relative"), ["src/a.txt"]);
    assert_eq!(sources(&dir, "tree-of-a-file"), ["gen"]);
}

/// A path named without wildcards stays against every exclusion but one
/// naming it; the exemption holds in the naming task only.
#[test]
fn named_paths_stay() {
    let dir = stage("exclusions");
    assert_eq!(sources(&dir, "named"), ["src/.env", "target/.keep/b.o"]);
    assert_eq!(
        sources(&dir, "folded-named"),
        ["src/.env", "target/.keep/b.o"]
    );
}

/// Named through a link, a path stays all the same.
#[cfg(unix)]
#[test]
fn named_paths_stay_through_links() {
    let dir = staged();
    assert_eq!(
        sources(&dir, "named"),
        ["build/again/x.o", "src/.env", "target/.keep/b.o"]
    );
}

/// Exclusions match dot entries, which includes still skip unless named.
#[test]
fn exclusions_match_dot_entries() {
    let dir = stage("exclusions");
    assert_eq!(sources(&dir, "dot-entries"), ["build/x.o", "src/.env"]);
    assert_eq!(sources(&dir, "dot-wildcard"), ["src/a.txt"]);
}

/// Without an include naming a dot entry an exclusion matches, the checksum
/// is the one 4.9.0 computed for the same fixture. Unix only: a Windows
/// checkout may turn the fixture's line endings into CRLF.
#[cfg(unix)]
#[test]
fn checksums_without_dot_contradictions_are_unchanged() {
    let dir = stage("exclusions");
    assert!(run(&dir, &["no-contradiction"]).ok());
    let r = run(&dir, &["--status", "--json", "no-contradiction"]);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
    assert_eq!(v[0]["sources_hash"], "41df4028ec4d35946128feb59c995f73");
    assert_eq!(
        sources(&dir, "no-contradiction"),
        [
            "fixture/src/a.txt",
            "fixture/src/web/.yarnrc.yml",
            "fixture/src/web/node_modules.txt"
        ]
    );
}

/// A task's own exclusions prune the walks of the sources it takes from
/// other tasks without changing what they list.
#[cfg(unix)]
#[test]
fn exclusions_prune_referenced_sources_alike() {
    let dir = staged();
    // As 4.9.0 lists them; `link` is a link to `src`.
    assert_eq!(
        sources(&dir, "folded"),
        [
            "link/lib.rs",
            "link/node_modules/p/x.rs",
            "src/a.txt",
            "src/lib.rs"
        ]
    );
    assert_eq!(
        sources(&dir, "rust"),
        [
            "link/lib.rs",
            "link/node_modules/p/x.rs",
            "src/lib.rs",
            "src/node_modules/p/x.rs"
        ]
    );
}
