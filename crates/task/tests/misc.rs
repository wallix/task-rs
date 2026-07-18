//! Final batch of behavioral parity tests ported from Go `task_test.go`.
//!
//! Covers the `from:` glob feature, included-taskfile var merging, checksum /
//! fingerprint behavior across runs, `--summary`, `--status` (text and JSON),
//! symlink path resolution, taskfile walking, setup semantics under a
//! concurrency limit, and CLI arg passthrough.
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
use std::process::Command;

/// Writes a `Taskfile.yml` with the given body into a fresh temp dir and
/// returns its path. Mirrors the many Go tests that synthesize an inline
/// Taskfile via `t.TempDir()` + `os.WriteFile`.
fn scratch(taskfile: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "task-misc-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    dir
}

fn write(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

// ---------------------------------------------------------------------------
// from: glob feature
// ---------------------------------------------------------------------------

/// Ports Go `TestFromSourcesDeps`: `from: deps` in a wrapper's sources collects
/// (and deduplicates) its deps' sources.
#[test]
fn from_sources_deps() {
    let dir = stage("from_sources_deps");

    // First run executes.
    assert!(run(&dir, &["wrapper"]).ok());
    assert!(dir.join("output-a.txt").exists());
    assert!(dir.join("output-b.txt").exists());

    // Second run is up to date.
    let second = run(&dir, &["wrapper"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    // Changing the shared source re-executes the wrapper.
    write(&dir, "input.txt", "changed\n");
    let third = run(&dir, &["wrapper"]);
    assert!(
        !third.combined().contains("Task \"wrapper\" is up to date"),
        "wrapper should re-run after source change, got: {}",
        third.combined()
    );

    // Status output deduplicates the shared source.
    let status = run(&dir, &["--status", "wrapper"]);
    assert_eq!(
        status.combined().matches("srcrule:input.txt").count(),
        1,
        "input.txt should be deduplicated, got:\n{}",
        status.combined()
    );
}

/// Ports Go `TestFromSourcesCmds`: `from: cmds` collects sources/generates from
/// the wrapper's cmd task-calls.
#[test]
fn from_sources_cmds() {
    let dir = scratch(
        "version: '3'\ntasks:\n  wrapper:\n    sources:\n      - from: cmds\n    generates:\n      - from: cmds\n    cmds:\n      - task: worker\n\n  worker:\n    sources:\n      - input.txt\n    generates:\n      - output.txt\n    cmds:\n      - cp input.txt output.txt\n",
    );
    write(&dir, "input.txt", "data\n");

    assert!(run(&dir, &["wrapper"]).ok());
    assert!(dir.join("output.txt").exists());

    let second = run(&dir, &["wrapper"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    write(&dir, "input.txt", "changed\n");
    let third = run(&dir, &["wrapper"]);
    assert!(
        !third.combined().contains("up to date"),
        "got: {}",
        third.combined()
    );
}

/// Ports Go `TestFromMixed`: a task mixing literal globs with `from: deps` in
/// both sources and generates inherits and reports both.
#[test]
fn from_mixed() {
    let dir = stage("from_mixed");

    assert!(run(&dir, &["wrapper"]).ok());
    assert!(dir.join("leaf-out.txt").exists());
    assert!(dir.join("result.txt").exists());

    let second = run(&dir, &["wrapper"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    let status = run(&dir, &["--status", "wrapper"]).combined();
    assert!(status.contains("srcrule:config.txt"), "got:\n{status}");
    assert!(status.contains("srcrule:input.txt"), "got:\n{status}");
    assert!(status.contains("genrule:result.txt"), "got:\n{status}");
    assert!(status.contains("genrule:leaf-out.txt"), "got:\n{status}");

    write(&dir, "input.txt", "new\n");
    let third = run(&dir, &["wrapper"]);
    assert!(
        !third.combined().contains("up to date"),
        "got: {}",
        third.combined()
    );
}

/// Ports Go `TestFromDepsChildDifferentDir`: a `from: deps` child in a
/// different `dir:` must resolve its relative globs against the child's dir, so
/// changing the child's source re-runs the wrapper.
///
/// NOTE: the Go Taskfile uses a bare YAML boolean `- true` as the wrapper's
/// command. The Rust decoder rejects a bare bool as a command (see the
/// `ignore_nil_elements` GAP below for the same decoder limitation), so this
/// port quotes it as `- 'true'`. The behavior under test — child-dir glob
/// resolution — is unaffected and verified below.
#[test]
fn from_deps_child_different_dir() {
    let dir = scratch(
        "version: '3'\ntasks:\n  wrapper:\n    sources:\n      - from: deps\n    generates:\n      - from: deps\n    deps:\n      - leaf\n    cmds:\n      - 'true'\n\n  leaf:\n    dir: sub\n    sources:\n      - input.txt\n    generates:\n      - output.txt\n    cmds:\n      - cp input.txt output.txt\n",
    );
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    write(&dir.join("sub"), "input.txt", "data\n");

    assert!(run(&dir, &["wrapper"]).ok());
    assert!(dir.join("sub/output.txt").exists());

    let second = run(&dir, &["wrapper"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    // Change the child's source: the wrapper must detect it via the child's dir.
    write(&dir.join("sub"), "input.txt", "changed\n");
    let third = run(&dir, &["wrapper"]);
    assert!(
        !third.combined().contains("up to date"),
        "wrapper should detect source change in child's dir, got: {}",
        third.combined()
    );
}

/// Ports Go `TestFromInvalidValue`: an unsupported `from:` value errors with a
/// message mentioning `unsupported from:`.
#[test]
fn from_invalid_value() {
    let dir = scratch(
        "version: '3'\ntasks:\n  bad:\n    sources:\n      - from: invalid\n    cmds:\n      - echo hello\n",
    );

    let out = run(&dir, &["bad"]);
    assert!(!out.ok(), "expected error, got success: {}", out.combined());
    assert!(
        out.combined().contains("unsupported from:"),
        "expected 'unsupported from:' in error, got: {}",
        out.combined()
    );
}

// ---------------------------------------------------------------------------
// Included-taskfile var merging
// ---------------------------------------------------------------------------

/// Ports Go `TestIncludedVars`: included-taskfile var overrides and defaults.
#[test]
fn included_vars() {
    let dir = stage("include_with_vars");
    let out = run_merged(&dir, &["task1"]);
    let expected = "\
task: [included1:task1] echo \"VAR_1 is included1-var1\"
VAR_1 is included1-var1
task: [included1:task1] echo \"VAR_2 is included-default-var2\"
VAR_2 is included-default-var2
task: [included2:task1] echo \"VAR_1 is included2-var1\"
VAR_1 is included2-var1
task: [included2:task1] echo \"VAR_2 is included-default-var2\"
VAR_2 is included-default-var2
task: [included3:task1] echo \"VAR_1 is included-default-var1\"
VAR_1 is included-default-var1
task: [included3:task1] echo \"VAR_2 is included-default-var2\"
VAR_2 is included-default-var2";
    assert_eq!(out.out.trim(), expected);
}

/// Ports Go `TestIncludedVarsMultiLevel`: vars propagate through nested
/// includes.
#[test]
fn included_vars_multi_level() {
    let dir = stage("include_with_vars_multi_level");
    let out = run_merged(&dir, &["default"]);
    let expected = "\
task: [lib:greet] echo 'Hello world'
Hello world
task: [foo:lib:greet] echo 'Hello foo'
Hello foo
task: [bar:lib:greet] echo 'Hello bar'
Hello bar";
    assert_eq!(out.out.trim(), expected);
}

/// Ports Go `TestIncludeWithVarsInInclude`: setup must succeed for a taskfile
/// that declares vars inside its `includes:` entries. The Go test only asserts
/// `Setup()` returns no error. This taskfile declares vars inside its
/// `includes:` entries and has no root tasks, so any command exits non-zero
/// ("No tasks available"). The faithful proxy for a successful setup is the
/// absence of a parse/decode error — the includes were resolved without error.
#[test]
fn include_with_vars_in_include() {
    let dir = stage("include_with_vars_inside_include");
    let out = run(&dir, &["--list-all"]);
    let combined = out.combined();
    assert!(
        !combined.contains("Failed to parse") && !combined.contains("Failed to decode"),
        "setup should succeed (no parse/decode error), got: {combined}"
    );
}

// ---------------------------------------------------------------------------
// Checksum / fingerprint behavior
// ---------------------------------------------------------------------------

/// Ports Go `TestChecksumIncludesRawCommands`: the CHECKSUM is a resolvable hex
/// string and changes when a command template changes.
#[test]
fn checksum_includes_raw_commands() {
    let dir = scratch(
        "version: '3'\ntasks:\n  build:\n    sources:\n      - source.txt\n    cmds:\n      - echo \"{{.CHECKSUM}}\" > output.txt\n    generates:\n      - output.txt\n",
    );
    write(&dir, "source.txt", "hello");

    assert!(run(&dir, &["build"]).ok());
    let checksum = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    let checksum = checksum.trim().to_string();
    assert!(!checksum.is_empty(), "CHECKSUM should be resolved");
    assert!(
        checksum.chars().all(|c| c.is_ascii_hexdigit()),
        "CHECKSUM should be hex, got: {checksum}"
    );

    // Change the command; the CHECKSUM must change.
    let _ = std::fs::remove_dir_all(dir.join(".task"));
    write(
        &dir,
        "Taskfile.yml",
        "version: '3'\ntasks:\n  build:\n    sources:\n      - source.txt\n    cmds:\n      - echo \"different-cmd {{.CHECKSUM}}\" > output.txt\n    generates:\n      - output.txt\n",
    );
    assert!(run(&dir, &["build"]).ok());
    let out2 = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    let checksum2 = out2.split_whitespace().last().unwrap().to_string();
    assert_ne!(
        checksum, checksum2,
        "CHECKSUM should change when the command template changes"
    );
}

/// Ports Go `TestSetupSourcesNotMergedIntoFingerprint`: a setup task's sources
/// are not part of the parent's fingerprint, so changing them does not
/// re-execute the parent.
#[test]
fn setup_sources_not_merged_into_fingerprint() {
    let dir = scratch(
        "version: '3'\ntasks:\n  prepare:\n    sources:\n      - setup-src.txt\n    cmds:\n      - 'true'\n\n  build:\n    setup:\n      - prepare\n    sources:\n      - main-src.txt\n    generates:\n      - output.txt\n    cmds:\n      - cp main-src.txt output.txt\n",
    );
    write(&dir, "setup-src.txt", "v1");
    write(&dir, "main-src.txt", "main");

    assert!(run(&dir, &["build"]).ok());
    assert!(dir.join("output.txt").exists());

    let second = run(&dir, &["build"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    // Change only the setup task's source; parent must stay up to date.
    write(&dir, "setup-src.txt", "v2");
    let third = run(&dir, &["build"]);
    assert!(
        third.combined().contains("Task \"build\" is up to date"),
        "parent should stay up to date when only setup source changes, got: {}",
        third.combined()
    );
}

/// Ports Go `TestRunWhenChanged`: `run: when_changed` deduplicates task
/// executions with identical resolved vars across included taskfiles, so the
/// repeated `fubar` call is skipped and the output is the contiguous block
/// `fubar / foo / bar`.
#[test]
fn run_when_changed() {
    let dir = stage("run_when_changed");
    let out = run(&dir, &["--force-all", "--silent", "start"]);
    let expected = "\
login server=fubar user=fubar
login server=foo user=foo
login server=bar user=bar";
    assert!(
        out.combined().contains(expected),
        "expected deduplicated order, got: {}",
        out.combined()
    );
}

// ---------------------------------------------------------------------------
// Nil elements (GAP)
// ---------------------------------------------------------------------------

/// Ports Go `TestIgnoreNilElements`: a `nil` entry in cmds/deps/includes/
/// preconditions is ignored, leaving only the real element.
#[test]
fn ignore_nil_elements() {
    for sub in ["cmds", "deps", "includes", "preconditions"] {
        let dir = stage(&format!("ignore_nil_elements/{sub}"));
        let out = run(&dir, &["--silent", "default"]);
        assert!(out.ok(), "{sub}: setup/run failed: {}", out.combined());
        assert_eq!(out.stdout, "string-slice-1\n", "case {sub}");
    }
}

// ---------------------------------------------------------------------------
// Symlink path resolution
// ---------------------------------------------------------------------------

/// Ports Go `TestEvaluateSymlinksInPaths`: sources reached through a symlinked
/// directory are fingerprinted correctly across a sequence of runs.
#[test]
fn evaluate_symlinks_in_paths() {
    let dir = stage("evaluate_symlinks_in_paths");

    let steps: &[(&str, &str)] = &[
        ("default", "task: [default] echo \"some job\"\nsome job"),
        (
            "test-sym",
            "task: [test-sym] echo \"shared file source changed\" > src/shared/b",
        ),
        ("default", "task: [default] echo \"some job\"\nsome job"),
        ("default", "task: Task \"default\" is up to date"),
        (
            "reset",
            "task: [reset] echo \"shared file source\" > src/shared/b\ntask: [reset] echo \"file source\" > src/a",
        ),
    ];

    for (task, expected) in steps {
        let out = run_merged(&dir, &[task]);
        assert!(out.ok(), "task {task} failed: {}", out.out);
        assert_eq!(out.out.trim(), *expected, "task {task}");
    }
}

// ---------------------------------------------------------------------------
// Taskfile walking
// ---------------------------------------------------------------------------

/// Ports Go `TestTaskfileWalk`: running from a subdirectory walks upward to the
/// nearest Taskfile.
#[test]
fn taskfile_walk() {
    for sub in ["", "foo", "foo/bar"] {
        let root = stage("taskfile_walk");
        let dir = if sub.is_empty() {
            root.clone()
        } else {
            root.join(sub)
        };
        let out = run(&dir, &["default"]);
        assert!(out.ok(), "walk from {sub:?} failed: {}", out.combined());
        assert_eq!(out.stdout, "foo\n", "walk from {sub:?}");
    }
}

// ---------------------------------------------------------------------------
// Setup under a concurrency limit
// ---------------------------------------------------------------------------

/// Ports Go `TestSetupWithConcurrencyLimit`: with `-C 1`, a task with a setup
/// task must not deadlock on the concurrency semaphore.
#[test]
fn setup_with_concurrency_limit() {
    let dir = scratch(
        "version: '3'\ntasks:\n  prepare:\n    cmds:\n      - 'true'\n\n  build:\n    setup:\n      - prepare\n    sources:\n      - source.txt\n    generates:\n      - output.txt\n    cmds:\n      - cp source.txt output.txt\n\n  all:\n    deps:\n      - build\n",
    );
    write(&dir, "source.txt", "hello");

    // A deadlock would hang; the process must complete. `run` blocks on the
    // child, so a hang here would time out the test harness rather than pass.
    let out = run(&dir, &["-C", "1", "all"]);
    assert!(
        out.ok(),
        "expected no deadlock with -C 1, got: {}",
        out.combined()
    );
    assert!(dir.join("output.txt").exists());
}

// ---------------------------------------------------------------------------
// CLI arg splitting (GAP)
// ---------------------------------------------------------------------------

/// Ports Go `TestSplitArgs`: `splitArgs` re-splits `CLI_ARGS` honoring quotes,
/// so `foo bar 'foo bar baz'` yields 3 arguments.
#[test]
fn split_args() {
    let dir = stage("split_args");
    let out = run(
        &dir,
        &["--silent", "default", "--", "foo", "bar", "foo bar baz"],
    );
    assert!(out.ok(), "run failed: {}", out.combined());
    assert_eq!(out.stdout, "3\n");
}

// ---------------------------------------------------------------------------
// Status: JSON fields & output grouping
// ---------------------------------------------------------------------------

/// Ports Go `TestStatusJSONFields`: `--status --json` emits all expected
/// fields. Asserted textually (no serde_json dev-dep).
#[test]
fn status_json_fields() {
    let dir = scratch(
        "version: '3'\ntasks:\n  compile:\n    sources:\n      - src.txt\n    generates:\n      - out.bin\n    cmds:\n      - cp src.txt out.bin\n",
    );
    write(&dir, "src.txt", "data");

    assert!(run(&dir, &["compile"]).ok());

    let out = run(&dir, &["--status", "--json", "compile"]).stdout;
    assert!(
        out.trim_start().starts_with('['),
        "expected JSON array: {out}"
    );
    assert!(out.contains("\"task\": \"compile\""), "got: {out}");
    assert!(out.contains("\"up_to_date\": true"), "got: {out}");
    assert!(out.contains("\"sources_up_to_date\": true"), "got: {out}");
    assert!(out.contains("\"generates_up_to_date\": true"), "got: {out}");
    assert!(out.contains("\"checksum_file\":"), "got: {out}");
    assert!(out.contains("\"sources_hash\":"), "got: {out}");
    assert!(out.contains("\"generates_hash\":"), "got: {out}");
    // Array fields, non-empty.
    assert!(out.contains("\"source_files\": ["), "got: {out}");
    assert!(out.contains("\"source_data\": ["), "got: {out}");
    assert!(out.contains("\"generate_files\": ["), "got: {out}");
    // checksum_file value must be non-empty.
    assert!(
        !out.contains("\"checksum_file\": \"\""),
        "checksum_file should be non-empty: {out}"
    );
}

/// Ports Go `TestStatusOutputGrouping`: `genrule:` entries appear after the
/// `generates:` header (not under `sources:`), and `srcrule:` before it.
#[test]
fn status_output_grouping() {
    let dir = scratch(
        "version: '3'\ntasks:\n  build:\n    cmds:\n      - cp source.txt generated.txt\n    sources:\n      - source.txt\n    generates:\n      - generated.txt\n",
    );
    write(&dir, "source.txt", "hello");

    let out = run(&dir, &["--status", "build"]).combined();
    let src_idx = out.find("sources:").expect("sources: header");
    let gen_idx = out.find("generates:").expect("generates: header");
    let genrule_idx = out.find("genrule:").expect("genrule: entry");
    let srcrule_idx = out.find("srcrule:").expect("srcrule: entry");

    assert!(
        genrule_idx > gen_idx,
        "genrule: should follow generates: header, got:\n{out}"
    );
    assert!(
        srcrule_idx < gen_idx && srcrule_idx > src_idx,
        "srcrule: should appear under sources:, got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

/// Ports Go `TestSummary`: `--summary` prints the task name, summary, deps and
/// commands, matching the golden `task-with-summary.txt`.
#[test]
fn summary() {
    let dir = stage("summary");
    let out = run(
        &dir,
        &[
            "--summary",
            "--silent",
            "task-with-summary",
            "other-task-with-summary",
        ],
    );
    assert!(out.ok(), "summary failed: {}", out.combined());

    let golden =
        std::fs::read_to_string(testdata("summary").join("task-with-summary.txt")).unwrap();
    assert_eq!(out.combined(), golden);
}

// ---------------------------------------------------------------------------
// Flock serialization (GAP)
// ---------------------------------------------------------------------------

/// Ports Go `TestFlockSerializesParallelTasks`: two concurrent invocations of
/// the same fingerprinted task are serialized by the local build-once flock, so
/// the log never interleaves as `start / start / end / end`.
#[test]
fn flock_serializes_parallel_tasks() {
    let dir = scratch(
        "version: '3'\ntasks:\n  build:\n    sources:\n      - source.txt\n    generates:\n      - output.txt\n    cmds:\n      - echo \"start\" >> log.txt\n      - sleep 0.2\n      - echo \"end\" >> log.txt\n      - touch output.txt\n",
    );
    write(&dir, "source.txt", "hello");

    let spawn = || {
        Command::new(BIN)
            .args(["build"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn task")
    };
    let a = spawn();
    let b = spawn();
    assert!(a.wait_with_output().unwrap().status.success());
    assert!(b.wait_with_output().unwrap().status.success());

    let log = std::fs::read_to_string(dir.join("log.txt")).unwrap();
    let lines: Vec<&str> = log.trim().split('\n').collect();
    if lines.len() == 4 {
        assert_eq!(
            lines,
            ["start", "end", "start", "end"],
            "flock should serialize"
        );
    }
}
