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

// The sprig helpers are reachable in Go function position, not only after a
// pipe, and take the subject as their last argument there.
#[test]
fn sprig_helpers_work_in_function_position() {
    let dir = stage("template_funcs");
    let o = run(&dir, &["funcs"]);
    assert!(o.ok(), "output: {}", o.combined());
    let out = o.combined();
    for want in [
        "suffix=dir/fr",
        "prefix=fr.po",
        "split=dir,fr.po",
        "has=truefalse",
        "first=dir",
        "default=fallback",
        "title=Hello Wide World",
    ] {
        assert!(out.contains(want), "expected {want:?} in: {out}");
    }

    // The pipeline spelling renders the same text as the function spelling,
    // including the five that mean something else as a minijinja builtin filter.
    let piped = run(&dir, &["pipes"]);
    assert!(piped.ok(), "output: {}", piped.combined());
    for want in [
        "suffix=dir/fr",
        "prefix=fr.po",
        "split=dir,fr.po",
        "default=fallback",
        "title=HELLO World",
        "first=dir",
        "last=fr.po",
        "join=dir/fr.po",
    ] {
        assert!(
            piped.combined().contains(want),
            "expected {want:?} in: {}",
            piped.combined()
        );
    }
}

// A file written natively in Jinja keeps standard Jinja meaning: only the Go
// dialect gets sprig's, and it gets it by translating a pipe into a call rather
// than by overriding the builtin filters.
#[test]
fn jinja_filters_keep_their_standard_meaning() {
    let dir = stage("template_funcs");
    let o = run(&dir, &["--taskfile", "jinja.yml", "jinja"]);
    assert!(o.ok(), "output: {}", o.combined());
    let out = o.combined();
    for want in [
        // Empty is not undefined, so the fallback does not fire...
        "default=\n",
        // ...nor does it for a legitimate zero.
        "zero=0",
        // Undefined still takes it.
        "missing=fallback",
        // Jinja's `title` lowercases the tail.
        "title=Hello World",
        // The sprig meaning stays reachable in function position.
        "sprig_default=fallback",
        "sprig_title=HELLO World",
        "sprig_join=dir/fr.po",
    ] {
        assert!(out.contains(want), "expected {want:?} in: {out}");
    }
}

// The Go dialect keeps sprig's meaning after a pipe, and `--migrate` has to
// carry that meaning into the converted file: the five helpers whose Jinja
// builtin means something else are translated to a call, not to a filter, so
// the migrated Taskfile renders exactly what the Go one did.
#[test]
fn migration_preserves_sprig_meaning_after_a_pipe() {
    let go = stage("template_funcs");
    let before = run(&go, &["pipes"]);
    assert!(before.ok(), "output: {}", before.combined());

    let migrated = stage("template_funcs");
    let w = run(&migrated, &["--migrate", "--write"]);
    assert!(w.ok(), "stderr: {}", w.stderr);
    let on_disk = std::fs::read_to_string(migrated.join("Taskfile.yml")).unwrap();
    // A call with the subject last, not `EMPTY | default("fallback")`, which
    // would mean "only if undefined" once the file is Jinja.
    assert!(
        on_disk.contains(r#"default("fallback", EMPTY)"#),
        "migrated to: {on_disk}"
    );
    // A helper whose builtin already matches sprig stays an idiomatic filter.
    assert!(on_disk.contains("| trimSuffix("), "migrated to: {on_disk}");

    let after = run(&migrated, &["pipes"]);
    assert!(after.ok(), "output: {}", after.combined());
    for line in before.combined().lines().filter(|l| l.contains('=')) {
        assert!(
            after.combined().contains(line),
            "migration changed {line:?}; after: {}",
            after.combined()
        );
    }
}
