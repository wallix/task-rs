//! Behavioral parity tests ported from Go `task_test.go` (core execution
//! behaviors). See `common` for the harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::{run, stage};

// Ports Go `TestDry`.
#[test]
fn dry_run_prints_command_without_executing() {
    let dir = stage("dry");
    let _ = std::fs::remove_file(dir.join("file.txt"));
    let o = run(&dir, &["--dry", "build"]);
    assert!(o.ok(), "dry run failed: {}", o.combined());
    assert_eq!(o.combined().trim(), "task: [build] touch file.txt");
    assert!(
        !dir.join("file.txt").exists(),
        "dry run must not create the file"
    );
}

// Ports Go `TestCyclicDep`.
#[test]
fn cyclic_dependency_errors() {
    let dir = stage("cyclic");
    let o = run(&dir, &["task-1"]);
    assert!(!o.ok(), "a cyclic dependency must fail: {}", o.combined());
}

// Ports Go `TestInternalTask`.
#[test]
fn internal_tasks_run_indirectly_but_not_directly() {
    let dir = stage("internal_task");
    assert_eq!(run(&dir, &["--silent", "task-1"]).stdout, "Hello, World!\n");
    assert_eq!(run(&dir, &["--silent", "task-2"]).stdout, "Hello, World!\n");
    // task-3 is internal and cannot be called directly.
    assert!(!run(&dir, &["--silent", "task-3"]).ok());
}

// Ports Go `TestGenerates`.
#[test]
fn generates_creates_files_and_reports_up_to_date_on_rerun() {
    let dir = stage("generates");
    for task in ["rel.txt", "abs.txt", "my text file.txt"] {
        let first = run(&dir, &[task]);
        assert!(first.ok(), "{task}: {}", first.combined());
        assert!(dir.join("sub/src.txt").exists(), "source should exist");
        assert!(dir.join(task).exists(), "dest {task:?} should exist");
        assert!(
            !first.combined().contains("up to date"),
            "{task:?} should not be up to date on first run"
        );

        let second = run(&dir, &[task]);
        assert!(
            second.combined().contains("up to date"),
            "{task:?} should be up to date on rerun: {}",
            second.combined()
        );
    }
}

// Ports Go `TestTaskIgnoreErrors`.
#[test]
fn ignore_errors_controls_task_and_command_failure() {
    let dir = stage("ignore_errors");
    assert!(run(&dir, &["task-should-pass"]).ok());
    assert!(!run(&dir, &["task-should-fail"]).ok());
    assert!(run(&dir, &["cmd-should-pass"]).ok());
    assert!(!run(&dir, &["cmd-should-fail"]).ok());
}

// Ports Go `TestDisplaysErrorOnVersion1Schema`.
#[test]
fn version_1_schema_is_rejected() {
    let dir = stage("version/v1");
    let o = run(&dir, &[]);
    assert!(!o.ok(), "a v1 schema must be rejected");
    assert!(
        o.combined().contains("chema version") || o.combined().contains("version"),
        "expected a schema-version error, got: {}",
        o.combined()
    );
}
