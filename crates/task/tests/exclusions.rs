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
