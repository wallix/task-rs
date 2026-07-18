//! CLI tests for `--json`/`--nested` task listing (editor integrations) and
//! `--completion` script output.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

fn taskfile_dir(contents: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "task-listing-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Taskfile.yml"), contents).unwrap();
    dir
}

const TASKFILE: &str = "version: '3'\ntasks:\n  build:\n    desc: Build it\n    aliases: [b]\n    cmds: ['echo x']\n  ns:sub:\n    desc: Nested\n    cmds: ['echo y']\n";

#[test]
fn json_listing_shape() {
    let dir = taskfile_dir(TASKFILE);
    let r = common::run(&dir, &["--list-all", "--json"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");

    let tasks = v["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["name"], "build");
    assert_eq!(tasks[0]["task"], "build");
    assert_eq!(tasks[0]["desc"], "Build it");
    assert_eq!(tasks[0]["aliases"][0], "b");
    // Line/column recovered from the source (build: is line 3).
    assert_eq!(tasks[0]["location"]["line"], 3);
    assert_eq!(tasks[0]["location"]["column"], 3);
    // The namespaced task keeps its whole key and a real line.
    assert_eq!(tasks[1]["task"], "ns:sub");
    assert_eq!(tasks[1]["location"]["line"], 7);
}

#[test]
fn json_nested_groups_namespaces() {
    let dir = taskfile_dir(TASKFILE);
    let r = common::run(&dir, &["--list-all", "--json", "--nested"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");

    // `build` stays at the root; `ns:sub` moves under the `ns` namespace.
    let root_tasks = v["tasks"].as_array().unwrap();
    assert_eq!(root_tasks.len(), 1);
    assert_eq!(root_tasks[0]["task"], "build");
    assert_eq!(v["namespaces"]["ns"]["tasks"][0]["task"], "ns:sub");
}

#[test]
fn completion_scripts_print() {
    let dir = taskfile_dir(TASKFILE);
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let r = common::run(&dir, &["--completion", shell]);
        assert!(r.ok(), "{shell}: {}", r.stderr);
        assert!(!r.stdout.is_empty(), "{shell} produced no script");
    }
    let bad = common::run(&dir, &["--completion", "tcsh"]);
    assert!(!bad.ok());
    assert!(bad.stderr.contains("unknown shell"));
}
