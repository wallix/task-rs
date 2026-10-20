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

/// `task:` in sources folds in the sources of a task that is neither a dep nor
/// called, resolved against that task's dir, without running it.
#[test]
fn task_reference_in_sources() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sources:\n      - task: build\n      - test.txt\n    cmds:\n      - cat sub/input.txt test.txt > result.txt\n\n  build:\n    dir: sub\n    sources:\n      - input.txt\n    generates:\n      - output.txt\n    cmds:\n      - cp input.txt output.txt\n",
    );
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    write(&dir.join("sub"), "input.txt", "data\n");
    write(&dir, "test.txt", "test\n");

    assert!(run(&dir, &["test"]).ok());
    assert!(dir.join("result.txt").exists());
    assert!(!dir.join("sub/output.txt").exists(), "build must not run");

    let second = run(&dir, &["test"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    write(&dir.join("sub"), "input.txt", "changed\n");
    let third = run(&dir, &["test"]);
    assert!(
        !third.combined().contains("up to date"),
        "test should re-run after build's source changed, got: {}",
        third.combined()
    );
}

/// A `task:` reference passes its `vars`, templated in the referencing task.
#[test]
fn task_reference_with_vars() {
    let dir = scratch(
        "version: '3'\nvars:\n  T: linux\ntasks:\n  test:\n    sources:\n      - task: build\n        vars:\n          TARGET: '{{.T}}'\n    cmds:\n      - 'true'\n\n  build:\n    sources:\n      - '{{.TARGET}}.txt'\n    cmds:\n      - 'true'\n",
    );
    write(&dir, "linux.txt", "x\n");
    let status = run(&dir, &["--status", "test"]).combined();
    assert!(
        status.contains(&status_file(&dir.join("linux.txt"))),
        "got:\n{status}"
    );
}

/// The `file:` line `--status` prints for `path`, which must exist.
fn status_file(path: &Path) -> String {
    format!("file: {}", path.display())
}

/// In an included Taskfile, a `task:` reference names a task of the same
/// include (by name or alias), or of the root with a leading `:`, like `deps`.
#[test]
fn task_reference_in_include_is_namespaced() {
    let dir = scratch(
        "version: '3'\nincludes:\n  lib:\n    taskfile: lib\n    dir: lib\ntasks:\n  root-task:\n    sources:\n      - root.txt\n    cmds:\n      - 'true'\n",
    );
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    write(
        &dir.join("lib"),
        "Taskfile.yml",
        "version: '3'\ntasks:\n  test:\n    sources:\n      - task: build\n    cmds:\n      - 'true'\n\n  test-alias:\n    sources:\n      - task: b\n    cmds:\n      - 'true'\n\n  test-root:\n    sources:\n      - task: ':root-task'\n    cmds:\n      - 'true'\n\n  build:\n    aliases: [b]\n    sources:\n      - input.txt\n    cmds:\n      - 'true'\n",
    );
    write(&dir.join("lib"), "input.txt", "x\n");
    write(&dir, "root.txt", "x\n");
    let lib_input = status_file(&dir.join("lib/input.txt"));
    for task in ["lib:test", "lib:test-alias"] {
        let status = run(&dir, &["--status", task]).combined();
        assert!(status.contains(&lib_input), "{task}:\n{status}");
    }
    let status = run(&dir, &["--status", "lib:test-root"]).combined();
    assert!(
        status.contains(&status_file(&dir.join("root.txt"))),
        "got:\n{status}"
    );
}

/// A `task:` reference to an unknown task names the referrer and field, and
/// exits with the task-not-found code; so does a `from: deps` on a missing dep.
#[test]
fn task_reference_to_unknown_task() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sources:\n      - task: missing\n    cmds:\n      - echo hello\n\n  wrapper:\n    deps: [missing]\n    generates:\n      - from: deps\n\n  outer:\n    sources:\n      - task: test\n\n  caller:\n    deps: [test]\n",
    );
    for (task, want, code) in [
        (
            "test",
            "task: test: sources: task \"missing\" does not exist\n",
            200,
        ),
        (
            "wrapper",
            "task: wrapper: generates: task \"missing\" does not exist\n",
            200,
        ),
        // The referrer is named, with the way to the missing task.
        (
            "outer",
            "task: outer: sources: {task: test} → sources {task: missing}: task \"missing\" \
             does not exist\n",
            200,
        ),
        // Under a running task, wrapped like a missing dep.
        (
            "caller",
            "task: Failed to run task \"caller\": task: test: sources: task \"missing\" does not exist\n",
            201,
        ),
    ] {
        let out = run(&dir, &[task]);
        assert_eq!(out.stderr, want, "{task}");
        assert_eq!(out.code, code, "{task}");
    }
}

/// A `task:` name that renders empty is an error, not a glob of the task dir.
#[test]
fn task_reference_empty_name() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sources:\n      - task: '{{.X}}'\n    cmds:\n      - echo hello\n",
    );
    let out = run(&dir, &["test"]);
    assert_eq!(
        out.stderr,
        "task: test: sources: `task: {{.X}}` rendered to an empty task name\n"
    );
    assert_eq!(out.code, 1);
}

/// References that reach their own task are no error: each task's entries
/// count once.
#[test]
fn task_reference_cycle() {
    let dir = scratch(
        "version: '3'\ntasks:\n  self:\n    sources:\n      - task: self\n      - s.txt\n    cmds:\n      - 'true'\n\n  a:\n    sources:\n      - task: b\n      - a.txt\n    cmds:\n      - 'true'\n\n  b:\n    sources:\n      - task: a\n      - b.txt\n    cmds:\n      - 'true'\n",
    );
    for f in ["s.txt", "a.txt", "b.txt"] {
        write(&dir, f, "x\n");
    }
    for (task, files) in [("self", &["s.txt"][..]), ("a", &["a.txt", "b.txt"][..])] {
        let out = run(&dir, &[task]);
        assert!(out.ok(), "{task}: {}", out.combined());
        let status = run(&dir, &["--status", task]).stdout;
        for f in files {
            let line = status_file(&dir.join(f));
            assert_eq!(status.matches(&line).count(), 1, "{task}:\n{status}");
        }
    }
}

/// References are followed transitively, each against its own task's dir.
#[test]
fn task_reference_transitive() {
    let dir = scratch(
        "version: '3'\ntasks:\n  a:\n    sources:\n      - task: b\n    cmds:\n      - 'true'\n\n  b:\n    sources:\n      - task: c\n      - b.txt\n\n  c:\n    dir: cdir\n    sources:\n      - c.txt\n",
    );
    std::fs::create_dir_all(dir.join("cdir")).unwrap();
    write(&dir, "b.txt", "b\n");
    write(&dir.join("cdir"), "c.txt", "c\n");

    let status = run(&dir, &["--status", "a"]).combined();
    for f in [dir.join("b.txt"), dir.join("cdir/c.txt")] {
        assert!(status.contains(&status_file(&f)), "got:\n{status}");
    }

    assert!(run(&dir, &["a"]).ok());
    let second = run(&dir, &["a"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );
    write(&dir.join("cdir"), "c.txt", "changed\n");
    let third = run(&dir, &["a"]);
    assert!(
        !third.combined().contains("up to date"),
        "a should re-run after c's source changed, got: {}",
        third.combined()
    );
}

/// A `task:` reference in `generates` folds in that task's generates.
#[test]
fn task_reference_in_generates() {
    let dir = scratch(
        "version: '3'\ntasks:\n  pkg:\n    sources:\n      - pkg.txt\n    generates:\n      - task: build\n    cmds:\n      - 'true'\n\n  build:\n    dir: sub\n    generates:\n      - out.bin\n",
    );
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    write(&dir, "pkg.txt", "p\n");
    write(&dir.join("sub"), "out.bin", "o\n");
    let status = run(&dir, &["--status", "pkg"]).combined();
    let (_, generates) = status.split_once("generates:").unwrap();
    assert!(
        generates.contains(&status_file(&dir.join("sub/out.bin"))),
        "got:\n{status}"
    );
}

/// An `exclude:` after a `task:` entry removes files the reference folded in.
#[test]
fn task_reference_then_exclude() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sources:\n      - task: build\n      - exclude: sub/skip.txt\n    cmds:\n      - 'true'\n\n  build:\n    dir: sub\n    sources:\n      - '*.txt'\n",
    );
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    write(&dir.join("sub"), "keep.txt", "k\n");
    write(&dir.join("sub"), "skip.txt", "s\n");
    let status = run(&dir, &["--status", "test"]).combined();
    assert!(
        status.contains(&status_file(&dir.join("sub/keep.txt"))),
        "got:\n{status}"
    );
    assert!(
        !status.contains(&status_file(&dir.join("sub/skip.txt"))),
        "got:\n{status}"
    );
}

/// A dynamic (`sh:`) var passed through a reference's `vars` reaches the
/// referenced task's sources.
#[test]
fn task_reference_with_sh_var() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sources:\n      - task: build\n        vars:\n          TARGET:\n            sh: echo linux\n    cmds:\n      - 'true'\n\n  build:\n    sources:\n      - '{{.TARGET}}.txt'\n",
    );
    write(&dir, "linux.txt", "x\n");
    let status = run(&dir, &["--status", "test"]).combined();
    assert!(
        status.contains(&status_file(&dir.join("linux.txt"))),
        "got:\n{status}"
    );
}

/// A `task:` name, or a dep name read by `from: deps`, templated from a
/// task-level `sh:` var resolves when the task runs and under `--summary`,
/// whose fast compile does not run `sh:`.
#[test]
fn task_reference_name_from_sh_var() {
    let dir = scratch(
        "version: '3'\ntasks:\n  plain:\n    vars:\n      T:\n        sh: echo build-x\n    sources:\n      - task: '{{.T}}'\n    cmds:\n      - echo ran\n\n  prefixed:\n    vars:\n      T:\n        sh: echo x\n    sources:\n      - task: 'build-{{.T}}'\n    cmds:\n      - echo ran\n\n  wrapper:\n    vars:\n      T:\n        sh: echo x\n    deps:\n      - task: 'build-{{.T}}'\n    sources:\n      - from: deps\n    cmds:\n      - echo ran\n\n  build-x:\n    sources:\n      - b.txt\n",
    );
    write(&dir, "b.txt", "b\n");
    let b = status_file(&dir.join("b.txt"));
    for task in ["plain", "prefixed", "wrapper"] {
        let out = run(&dir, &[task]);
        assert!(out.ok(), "{task}: {}", out.combined());
        let status = run(&dir, &["--status", task]).combined();
        assert!(status.contains(&b), "{task}:\n{status}");
        let summary = run(&dir, &["--summary", task]);
        assert!(summary.ok(), "{task}: {}", summary.combined());
    }
}

/// A `task:` reference in `sandbox.reads` is rejected rather than ignored.
#[test]
fn task_reference_in_sandbox_reads() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sandbox:\n      reads:\n        - task: build\n    cmds:\n      - 'true'\n\n  build:\n    cmds:\n      - 'true'\n",
    );
    let out = run(&dir, &["test"]);
    assert!(!out.ok(), "expected error, got success: {}", out.combined());
    assert!(
        out.combined()
            .contains("does not support `from:` or `task:`"),
        "got: {}",
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

/// The drift warning names the sources whose content changed, not those the
/// run only touched.
#[test]
fn drift_warning_names_content_changes_only() {
    let dir = scratch(
        "version: '3'\ntasks:\n  build:\n    sources: ['*.txt']\n    cmds:\n      - touch input.txt\n      - printf out >> generated.txt\n",
    );
    write(&dir, "input.txt", "in");
    let out = run(&dir, &["build"]);
    assert!(out.ok(), "{}", out.combined());
    assert!(
        out.combined()
            .contains("changed during execution (generated.txt);"),
        "{}",
        out.combined()
    );
}

/// Appending to a matched source changes it on every run. The warning names
/// the file, and the unpublished fingerprint keeps the task out of date.
#[test]
fn sources_changed_during_execution_warns_and_names_files() {
    let dir = scratch(
        "\
version: '3'
tasks:
  build:
    sources:
      - '*.txt'
    cmds:
      - cmd: printf out >> generated.txt
",
    );
    write(&dir, "input.txt", "in");

    let warns_about_generated = |out: &str| {
        out.contains("WARNING: sources for")
            && out.contains("changed during execution")
            && out.contains("generated.txt")
    };

    let first = run(&dir, &["build"]);
    assert!(first.ok(), "run failed: {}", first.combined());
    assert!(
        warns_about_generated(&first.combined()),
        "expected a warning naming generated.txt, got: {}",
        first.combined()
    );

    // Source drift makes the task run and warn again.
    let second = run(&dir, &["build"]);
    assert!(
        !second.combined().contains("up to date"),
        "task should not be up to date after source drift, got: {}",
        second.combined()
    );
    assert!(
        warns_about_generated(&second.combined()),
        "expected the warning to recur on the second run, got: {}",
        second.combined()
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

// ---------------------------------------------------------------------------
// `--update` flag surface
// ---------------------------------------------------------------------------

/// The self-update is a flag, not a subcommand, so a Taskfile that defines a
/// task called `update` still runs it — `task update` means what it always did.
#[test]
fn a_task_named_update_still_runs() {
    let dir = scratch("version: '3'\ntasks:\n  update:\n    cmds:\n      - echo updated\n");
    let out = run(&dir, &["--silent", "update"]);
    assert!(out.ok(), "run failed: {}", out.combined());
    assert_eq!(out.stdout, "updated\n");
}

/// `--check` only qualifies `--update`; on its own it is rejected rather than
/// silently ignored (or taken for a task name).
#[test]
fn check_without_update_is_rejected() {
    let dir = scratch("version: '3'\ntasks:\n  default:\n    cmds:\n      - echo ran\n");
    let out = run(&dir, &["--check"]);
    assert!(!out.ok(), "expected a failure: {}", out.combined());
    assert!(
        out.combined().contains("--check only applies to --update"),
        "{}",
        out.combined()
    );
}

// ---------------------------------------------------------------------------
// Taskfile parse errors
// ---------------------------------------------------------------------------

/// A positioned YAML error shows the source lines with a caret at the error.
#[test]
fn syntax_error_shows_source_snippet() {
    let dir = scratch("version: '3'\n\ntasks:\n  a: [1\n");
    let out = run(&dir, &["a"]);
    assert_eq!(out.code, 100, "{}", out.combined());
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "task: Failed to parse Taskfile.yml:\n\
         task: Failed to decode Taskfile: unclosed bracket '[' at line 4 column 6\n  \
         2 | \n  \
         3 | tasks:\n\
         > 4 |   a: [1\n    \
         |      ^\n"
    );
}

// ---------------------------------------------------------------------------
// Fingerprint compatibility
// ---------------------------------------------------------------------------

/// Tasks that reach no task running other tasks, and fold no task excluding
/// files, keep the identities 4.8.0 recorded, wherever the checkout is; their
/// checksums are pinned to fingerprint format v1. `plain` renders CHECKSUM in
/// a command, so its identity follows the checksum.
#[test]
fn reference_fingerprints_are_stable() {
    let pinned = [
        (
            "plain",
            "3d732301f4ee3b05",
            "cf3ec8b966e4a78cb649d46671cf3ff5\nca261b66908db25886f8be79bcca3019\n",
        ),
        (
            "folds-leaf",
            "0f91b18950262769",
            "e18e6bb9607aba945361af60fbf9f27a\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "leaf",
            "0f28b4ff1bdfde54",
            "029204a226fb72b6451b8318aa300d29\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "wrapper",
            "0844c9990e58b745",
            "9c09208094c427ba4bb8962307ddbbed\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "other-leaf",
            "b86f3a6d485ac5a0",
            "dfd4009a233b987c1925210176a62d33\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "mid",
            "f03abb008c682212",
            "b25d666660aa16bc345a33f9b9d37ce5\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "top",
            "419d0c7179e81088",
            "dfd4009a233b987c1925210176a62d33\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "wrap-flat",
            "64b6415d10c15c97",
            "209c6b46a4d8e694015292f99ba19945\nf2f36b292df325ee8abf7808417ca552\n",
        ),
        (
            "refers-leaf",
            "6751cbd5d0149b97",
            "526c8e0632a5b44c06a68b900de7aa40\nf2f36b292df325ee8abf7808417ca552\n",
        ),
    ];
    for _ in 0..2 {
        let dir = stage("ref_follows_runs/compat");
        for task in ["plain", "folds-leaf", "wrapper", "top", "wrap-flat"] {
            let out = run(&dir, &[task]);
            assert!(out.ok(), "{task}: {}", out.combined());
            if task == "plain" {
                assert!(
                    out.stdout
                        .contains("CHECKSUM=cf3ec8b966e4a78cb649d46671cf3ff5\n"),
                    "{}",
                    out.combined()
                );
            }
        }
        for (task, identity, content) in pinned {
            let file = checksum_file(&dir, task).unwrap_or_else(|| panic!("{task}"));
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(name, format!("{task}-{identity}"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), content, "{task}");
        }
        // A wrapper folding a dep whose reference now reaches more: changed.
        assert!(run(&dir, &["wrap-ref"]).ok());
        let file = checksum_file(&dir, "wrap-ref").unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert_ne!(name, "wrap-ref-64dd264baacd1ec0");
        // It now reaches what `wrap-flat` lists, so both checksum alike.
        let flat = checksum_file(&dir, "wrap-flat").unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            std::fs::read_to_string(&flat).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// What decides what a task runs or reads cannot read CHECKSUM: a task var,
/// a setup entry or a source reading it fails the task, naming the task and
/// field and saying where it is available, and runs nothing.
#[test]
fn checksum_outside_late_fields_is_refused() {
    for (task, at) in [
        (
            "vars:\n      X: '{{.CHECKSUM}}'\n    cmds: ['echo {{.X}}']",
            "task: build: vars: X: ",
        ),
        (
            "setup: ['x-{{.CHECKSUM}}']\n    cmds: [echo ran]",
            "task: build: setup: ",
        ),
        (
            "sources: ['{{.CHECKSUM}}.txt']\n    cmds: [echo ran]",
            "task: build: sources: ",
        ),
        (
            "env:\n      E: {sh: 'touch ran-env; echo {{.CHECKSUM}}'}\n    cmds: [echo ran]",
            "task: build: env: ",
        ),
    ] {
        let dir = scratch(&format!(
            "version: '3'\ntasks:\n  build:\n    {task}\n  'x-*':\n    cmds: [echo setup]\n"
        ));
        let out = run(&dir, &["build"]);
        assert_eq!(out.code, 1, "{task}: {}", out.combined());
        assert!(
            out.stderr.contains(at)
                && out.stderr.contains(
                    "reads CHECKSUM: CHECKSUM is only available in commands, preconditions and \
                     the cache block, after setup"
                ),
            "{task}: {}",
            out.stderr
        );
        assert!(!out.stdout.contains("ran"), "{task}: {}", out.combined());
        assert!(!out.stdout.contains("setup"), "{task}: {}", out.combined());
        assert!(!dir.join("ran-env").exists(), "{task}");
    }

    // A wildcard task is named as it was called.
    let dir = scratch(
        "version: '3'\ntasks:\n  'w-*':\n    label: '{{.CHECKSUM}}'\n    cmds: [echo ran]\n",
    );
    let out = run(&dir, &["w-a"]);
    assert_eq!(out.code, 1, "{}", out.combined());
    assert!(out.stderr.contains("task: w-a: label: "), "{}", out.stderr);
}

/// `--summary` renders a refused CHECKSUM empty, as it did before the
/// refusal, rather than failing or printing the template.
#[test]
fn summary_renders_a_refused_checksum_empty() {
    let dir = scratch(
        "version: '3'\ntasks:\n  build:\n    desc: 'd-{{.CHECKSUM}}-'\n    cmds: [echo ran]\n",
    );
    // The environment's CHECKSUM does not show through either.
    let out = run_with_env(&dir, &["--summary", "build"], &[("CHECKSUM", "env")]);
    assert!(out.ok(), "{}", out.combined());
    assert!(out.stdout.contains("d--\n"), "{}", out.combined());
}

/// A var named CHECKSUM, at any level, is ignored: commands see the
/// computed value, empty without sources, and other vars cannot read it.
#[test]
fn a_checksum_var_is_ignored() {
    let dir = scratch(concat!(
        "version: '3'\nvars:\n  CHECKSUM: top\ntasks:\n",
        "  build:\n    vars: {CHECKSUM: mine}\n    cmds: ['echo \"[{{.CHECKSUM}}]\"']\n",
        "  read:\n    vars: {CHECKSUM: mine, Y: '{{.CHECKSUM}}'}\n    cmds: [echo ran]\n",
    ));
    let out = run(&dir, &["build"]);
    assert!(out.ok(), "{}", out.combined());
    assert!(out.stdout.contains("[]\n"), "{}", out.combined());
    let out = run(&dir, &["read"]);
    assert_eq!(out.code, 1, "{}", out.combined());
    assert!(
        out.stderr.contains("task: read: vars: Y: "),
        "{}",
        out.stderr
    );
}

/// Consumers outside the task's own fields still see CHECKSUM: `--summary`
/// prints the compiled vars, and group markers render against them.
#[test]
fn checksum_reaches_summary_and_group_markers() {
    let dir = scratch(
        "version: '3'\noutput:\n  group:\n    begin: 'B={{.CHECKSUM}}'\ntasks:\n  build:\n    sources: [s.txt]\n    cmds: [echo hi]\n",
    );
    write(&dir, "s.txt", "s");
    let summary = run(&dir, &["--summary", "build"]);
    assert!(summary.ok(), "{}", summary.combined());
    let line = summary
        .stdout
        .lines()
        .find(|l| l.trim_start().starts_with("CHECKSUM: "))
        .unwrap_or_else(|| panic!("{}", summary.combined()))
        .to_string();
    let value = line
        .trim()
        .trim_start_matches("CHECKSUM: ")
        .trim_matches('"')
        .to_string();
    assert_eq!(value.len(), 32, "{line}");
    let out = run(&dir, &["build"]);
    assert!(
        out.stdout.contains(&format!("B={value}\n")),
        "{}",
        out.combined()
    );
}

/// Group markers preserve template text in variable values, as commands do.
#[test]
fn group_markers_render_var_values_once() {
    let dir = scratch(concat!(
        "version: '3'\noutput:\n  group:\n    begin: 'B={{.V}}'\ntasks:\n  build:\n",
        "    vars:\n      V: '{{\"{{\"}}.TASK}}'\n    cmds: ['echo \"C={{.V}}\"']\n",
    ));
    let out = run(&dir, &["build"]);
    assert!(out.ok(), "{}", out.combined());
    assert!(out.stdout.contains("C={{.TASK}}\n"), "{}", out.combined());
    assert!(out.stdout.contains("B={{.TASK}}\n"), "{}", out.combined());
}

/// A source that is neither a regular file nor a symlink fails the task,
/// naming it, instead of blocking on it.
#[cfg(unix)]
#[test]
fn fifo_source_is_unsupported() {
    let dir =
        scratch("version: '3'\ntasks:\n  build:\n    sources: [pipe]\n    cmds: [echo ran]\n");
    let status = std::process::Command::new("mkfifo")
        .arg(dir.join("pipe"))
        .status()
        .unwrap();
    assert!(status.success());
    let out = run(&dir, &["build"]);
    assert!(!out.ok(), "{}", out.combined());
    assert!(
        out.combined().contains("pipe: unsupported file type"),
        "{}",
        out.combined()
    );
    assert!(!out.stdout.contains("ran"), "{}", out.combined());
}

/// Once a task's references resolved, what renders after them (here a
/// precondition) still renders in the task's own dialect.
#[test]
fn jinja_after_references_resolved() {
    let dir = scratch(
        "version: '3'\ntemplater: jinja\ntasks:\n  test:\n    sources:\n      - task: build\n    preconditions:\n      - sh: test -n '{{ CHECKSUM }}'\n        msg: 'no checksum: {{ CHECKSUM }}'\n    cmds:\n      - echo ran\n\n  build:\n    sources: [b.txt]\n",
    );
    write(&dir, "b.txt", "b\n");
    let out = run(&dir, &["test"]);
    assert!(out.ok(), "{}", out.combined());
    assert!(out.stdout.contains("ran\n"), "{}", out.combined());
}

/// Without references, a cache block error still comes before a template
/// error in a command, as in 4.8.0.
#[test]
fn cache_error_before_template_error_without_references() {
    let dir = scratch(
        "version: '3'\ntasks:\n  both:\n    sources: [a.txt]\n    cache:\n      vk: 127.0.0.1:9/repo\n      url: file:///tmp/x.zip\n    cmds:\n      - echo '{{.X | nosuchfn}}'\n",
    );
    write(&dir, "a.txt", "a\n");
    for args in [&["both"][..], &["--status", "both"], &["--dry", "both"]] {
        let out = run(&dir, args);
        assert_eq!(
            out.stderr,
            "task: both: cache: `vk` derives the cache and lock URLs; drop `url` and `lock`\n",
            "{args:?}"
        );
        assert_eq!(out.code, 1, "{args:?}");
    }
}

/// A template error in a command is reported, rather than lost when the
/// references resolve, whether run, under `--status` or `--dry`.
#[test]
fn template_error_with_references_is_reported() {
    let dir = scratch(
        "version: '3'\ntasks:\n  test:\n    sources: [a.txt]\n    generates: [{task: gen}]\n    cmds:\n      - echo '{{.X | nosuchfn}}'\n\n  gen:\n    generates: [out.txt]\n",
    );
    write(&dir, "a.txt", "a\n");
    for args in [&["test"][..], &["--status", "test"], &["--dry", "test"]] {
        let out = run(&dir, args);
        assert_eq!(out.code, 1, "{args:?}: {}", out.combined());
        assert!(
            out.stderr.contains("unsupported Go construct \"nosuchfn\""),
            "{args:?}: {}",
            out.combined()
        );
    }
}

// ---------------------------------------------------------------------------
// Exclusions of folded tasks
// ---------------------------------------------------------------------------

/// The `file:` lines of `--status` for `task`, relative to `dir`, split into
/// the sources and generates sections.
fn status_files(dir: &Path, task: &str) -> (Vec<String>, Vec<String>) {
    let out = run(dir, &["--status", task]);
    assert!(out.ok(), "{task}: {}", out.combined());
    let root = format!("file: {}/", dir.display());
    let (sources, generates) = out.stdout.split_once("  generates:").unwrap();
    let files = |s: &str| {
        s.lines()
            .filter_map(|l| l.trim().strip_prefix(&root).map(str::to_string))
            .collect()
    };
    (files(sources), files(generates))
}

/// A folded task's `exclude:` removes its own matches only: not the
/// referrer's, nor another folded task's, even for the same pattern.
#[test]
fn folded_exclusions_are_task_local() {
    let dir = stage("ref_follows_runs/exclusions");
    let (sources, generates) = status_files(&dir, "refs");
    assert_eq!(
        sources,
        [
            "a/a.txt",
            "b/keep.tmp",
            "b/skip.tmp",
            "own.tmp",
            "shared/s.tmp",
            "shared/s.txt"
        ],
        "{sources:?}"
    );
    assert_eq!(generates, ["out/a/y.txt", "out/b/x.log"], "{generates:?}");
    // The same through `from: deps`.
    let (sources, _) = status_files(&dir, "wrapper");
    assert_eq!(
        sources,
        ["a/a.txt", "b/keep.tmp", "b/skip.tmp", "shared/s.txt"],
        "{sources:?}"
    );
    // A fingerprint folded in from another task is not excluded either.
    let (sources, _) = status_files(&dir, "fingerprinted");
    assert_eq!(sources, ["a/a.txt", "fp/stamp.tmp", "shared/s.txt"]);
}

/// The referrer's own `exclude:` still removes what it folds in.
#[test]
fn own_exclusions_apply_to_folded_entries() {
    let dir = stage("ref_follows_runs/exclusions");
    let (sources, _) = status_files(&dir, "trims");
    assert_eq!(sources, ["b/keep.tmp"], "{sources:?}");
}

/// The cache archive and the up-to-date check follow the same rule.
#[test]
fn folded_exclusions_shape_the_archive_and_the_checksum() {
    let dir = stage("ref_follows_runs/exclusions");
    let out = run(&dir, &["--status", "--json", "refs"]).stdout;
    let (_, cache_files) = out.split_once("\"cache_files\": [").unwrap();
    let cache_files = cache_files.split_once(']').unwrap().0;
    assert!(cache_files.contains("out/b/x.log"), "{out}");
    assert!(!cache_files.contains("out/a/y.log"), "{out}");
    assert!(run(&dir, &["refs"]).ok());
    assert!(run(&dir, &["refs"]).combined().contains("up to date"));
    // A file only another task's exclusion matches is a source.
    write(&dir.join("b"), "keep.tmp", "changed\n");
    let third = run(&dir, &["refs"]);
    assert!(
        !third.combined().contains("up to date"),
        "{}",
        third.combined()
    );
}

/// A list whose groups matter keeps one identity and checksum wherever the
/// checkout is.
#[test]
fn grouped_fingerprints_are_stable() {
    for _ in 0..2 {
        let dir = stage("ref_follows_runs/exclusions");
        assert!(run(&dir, &["refs"]).ok());
        let file = checksum_file(&dir, "refs").unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "refs-5102e16695266f97");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "43378b7dbbb47fe2daa3dde3251412ff\n62a69eb9701b8d5c48441d320d4593a3\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `from: cmds` reads a templated deferred call as it runs after a success;
/// one depending on the exit code is refused.
#[test]
fn from_cmds_renders_deferred_calls() {
    let dir = scratch(
        "version: '3'\ntasks:\n  wrapper:\n    vars: {T: leaf}\n    sources: [from: cmds]\n    cmds:\n      - defer: {task: '{{.T}}'}\n      - 'true'\n\n  on-exit:\n    sources: [from: cmds]\n    cmds:\n      - defer: {task: leaf, vars: {E: '{{.EXIT_CODE}}'}}\n      - 'true'\n\n  leaf:\n    sources: [leaf.txt]\n",
    );
    write(&dir, "leaf.txt", "l\n");
    let status = run(&dir, &["--status", "wrapper"]);
    assert!(status.ok(), "{}", status.combined());
    assert!(
        status.stdout.contains(&status_file(&dir.join("leaf.txt"))),
        "{}",
        status.combined()
    );
    let out = run(&dir, &["on-exit"]);
    assert_eq!(
        out.stderr,
        "task: on-exit: sources: \"on-exit\"'s deferred call \"leaf\" depends on EXIT_CODE: \
         list its targets with explicit `task:` entries\n"
    );
    assert_eq!(out.code, 1);
}

// ---------------------------------------------------------------------------
// A `task:` reference covers what the task runs
// ---------------------------------------------------------------------------

/// The sources `--status` lists for `task`, relative to `dir`.
fn sources_of(dir: &Path, task: &str) -> Vec<String> {
    status_files(dir, task).0
}

/// The `srcrule:` entries of `task`, files that may not exist included.
fn rules_of(dir: &Path, args: &[&str]) -> Vec<String> {
    let out = run(dir, args);
    assert!(out.ok(), "{args:?}: {}", out.combined());
    let root = format!("{}/", dir.display());
    // `srcrule:<glob>`, or `srcrule@<group>:<glob>` kept as `@<group>:<glob>`.
    out.stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("srcrule"))
        .map(|r| {
            let (group, glob) = r.split_once(':').unwrap();
            let glob = glob.strip_prefix(&root).unwrap_or(glob);
            if group.is_empty() {
                glob.to_string()
            } else {
                format!("{group}:{glob}")
            }
        })
        .collect()
}

/// `setup`, `deps`, `task:` commands (`for:` over a list and over an `sh:`
/// var, deferred ones too): all their sources count, and only theirs.
#[test]
fn a_task_reference_covers_what_the_task_runs() {
    let dir = stage("ref_follows_runs/walk");
    let want = [
        "own.txt",
        "src/c.txt",
        "src/d.txt",
        "src/f-a.txt",
        "src/f-b.txt",
        "src/g-p.txt",
        "src/g-q.txt",
        "src/s.txt",
        "src/x.txt",
        "src/z.txt",
    ];
    assert_eq!(sources_of(&dir, "edges"), want);
    assert!(run(&dir, &["edges"]).ok());
    write(&dir, "unrelated.txt", "changed\n");
    let second = run(&dir, &["edges"]);
    assert!(
        second.combined().contains("up to date"),
        "{}",
        second.combined()
    );
    // A dep of the referenced task: stale.
    write(&dir.join("src"), "d.txt", "changed\n");
    let third = run(&dir, &["edges"]);
    assert!(
        !third.combined().contains("up to date"),
        "{}",
        third.combined()
    );
}

/// The value each `lang` task prints when the wrapper runs.
fn printed(out: &str, key: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in: {out}"))
        .to_string()
}

/// Vars reach each call as a run passes them: the reference's, a dep's own
/// overriding them, the command line's; names from `sh:` vars, aliases,
/// wildcards and included tasks resolve as a run resolves them.
#[test]
fn a_task_reference_passes_vars_as_a_run_does() {
    let dir = stage("ref_follows_runs/walk");
    for extra in [&[][..], &["L=de"][..]] {
        let _ = std::fs::remove_dir_all(dir.join(".task"));
        let ran = run(&dir, &[&["run-langs"][..], extra].concat());
        assert!(ran.ok(), "{}", ran.combined());
        let mut want: Vec<String> = ["lang", "lang-own"]
            .iter()
            .map(|k| format!("src/lang/{}/m.txt", printed(&ran.stdout, k)))
            .collect();
        want.sort();
        let status = status_files_with(&dir, &[&["langs"][..], extra].concat());
        assert_eq!(status, want, "{extra:?}");
    }
    assert_eq!(
        sources_of(&dir, "named"),
        [
            "inc/inc.txt",
            "src/aliased.txt",
            "src/by-sh.txt",
            "src/w-foo.txt"
        ]
    );
}

/// [`sources_of`] with more arguments.
fn status_files_with(dir: &Path, args: &[&str]) -> Vec<String> {
    let args = [&["--status"][..], args].concat();
    let out = run(dir, &args);
    assert!(out.ok(), "{args:?}: {}", out.combined());
    let root = format!("file: {}/", dir.display());
    let (sources, _) = out.stdout.split_once("  generates:").unwrap();
    sources
        .lines()
        .filter_map(|l| l.trim().strip_prefix(&root).map(str::to_string))
        .collect()
}

/// `CHECKSUM` in a dep's vars, in a dep's name or in a deferred call's vars
/// is refused, in both dialects: it is computed after setup, once what the
/// task calls is known. Run, and reached through a reference, alike.
#[test]
fn checksum_in_calls_is_refused() {
    let dir = stage("ref_follows_runs/walk");
    for prefix in ["", "jin:"] {
        for task in ["ck", "checksums"] {
            let out = run(&dir, &[&format!("{prefix}{task}")]);
            assert_eq!(out.code, 1, "{prefix}{task}: {}", out.combined());
            assert!(
                out.stderr.contains(
                    "reads CHECKSUM: CHECKSUM is only available in commands, preconditions and \
                     the cache block, after setup"
                ),
                "{prefix}{task}: {}",
                out.stderr
            );
        }
    }
    assert!(!dir.join("out").exists());
}

/// Identical var text from a Go and from a Jinja caller is rendered again
/// in each caller's dialect: two calls.
#[test]
fn a_reached_call_is_keyed_by_the_dialect_of_its_vars() {
    let dir = stage("ref_follows_runs/walk");
    assert_eq!(
        rules_of(&dir, &["--status", "dialects"]),
        ["src/v-a.txt", "src/v-a{#b#}.txt"]
    );
}

/// A deferred call is followed as it runs on success; one depending on the
/// exit code, or on `CHECKSUM`, is refused.
#[test]
fn deferred_calls_are_followed() {
    let dir = stage("ref_follows_runs/walk");
    // `from: cmds` renders it too.
    assert_eq!(sources_of(&dir, "deferred"), ["src/z.txt"]);
    for (task, want) in [
        (
            "deferred-exit-code",
            "task: deferred-exit-code: sources: {task: on-exit}: \"on-exit\"'s deferred call \
             \"z\" depends on EXIT_CODE: list its targets with explicit `task:` entries\n",
        ),
        (
            "deferred-checksum",
            "task: deferred-checksum: sources: {task: on-checksum}: cmds: template \
             \"{{.CHECKSUM}}\" reads CHECKSUM: CHECKSUM is only available in commands, preconditions and the cache \
             block, after setup\n",
        ),
    ] {
        let out = run(&dir, &[task]);
        assert_eq!(out.stderr, want, "{task}");
        assert_eq!(out.code, 1, "{task}");
    }
}

/// A command call whose name renders empty runs nothing, as in a run; a dep
/// whose name does is a missing task, as in a run.
#[test]
fn empty_names_in_reached_calls() {
    let dir = stage("ref_follows_runs/walk");
    assert_eq!(sources_of(&dir, "empty-cmd"), ["src/z.txt"]);
    let out = run(&dir, &["empty-dep"]);
    assert_eq!(
        out.stderr,
        "task: empty-dep: sources: {task: deps-empty} → deps \"\": task \"\" does not exist\n"
    );
    assert_eq!(out.code, 200);
}

/// A missing task reached at any depth, even behind an `if:`, fails alike
/// when run, under `--status` and `--dry`, naming the way to it; under a
/// caller, wrapped as a failed run unless `--exit-code`.
#[test]
fn a_missing_task_reached_fails_with_the_chain() {
    let dir = stage("ref_follows_runs/walk");
    let chain = "{task: u1} → deps \"u0\" → cmds \"missing\": task \"missing\" does not exist";
    let sources = format!("task: unknown: sources: {chain}\n");
    for args in [
        &["unknown"][..],
        &["--status", "unknown"],
        &["--dry", "unknown"],
    ] {
        let out = run(&dir, args);
        assert_eq!(out.stderr, sources, "{args:?}");
        assert_eq!(out.code, 200, "{args:?}");
    }
    let out = run_with_env(&dir, &["unknown-writes"], &[("TASK_SANDBOX", "off")]);
    assert_eq!(
        out.stderr,
        format!("task: unknown-writes: sandbox.writes: {chain}\n")
    );
    assert_eq!(out.code, 200);
    let wrapped = format!("task: Failed to run task \"calls-unknown\": {sources}");
    for (args, code) in [
        (&["calls-unknown"][..], 201),
        (&["-x", "calls-unknown"], 200),
    ] {
        let out = run(&dir, args);
        assert_eq!(out.stderr, wrapped, "{args:?}");
        assert_eq!(out.code, code, "{args:?}");
    }
}

/// A task-level `platforms:` excluding the host prunes a task the reached
/// one runs, with what it runs; a command's `platforms:` does not stop a
/// `task:` call, as in a run.
#[cfg(not(windows))]
#[test]
fn platforms_prune_reached_calls_as_a_run_does() {
    let dir = stage("ref_follows_runs/walk");
    assert_eq!(sources_of(&dir, "platforms"), ["src/here.txt"]);
}

/// A task reached along two paths counts once, in a stable order.
#[test]
fn a_diamond_counts_once() {
    let dir = stage("ref_follows_runs/walk");
    assert_eq!(
        sources_of(&dir, "diamond"),
        ["src/d.txt", "src/left.txt", "src/right.txt"]
    );
    let first = run(&dir, &["--status", "diamond"]).stdout;
    assert_eq!(run(&dir, &["--status", "diamond"]).stdout, first);
}

/// A task calling itself with other vars covers each; a deps cycle under a
/// reference fails only when run; a recursion the walk cannot see the end
/// of hits the cap, at exactly 256 calls of one task.
#[test]
fn recursion_under_a_reference() {
    let dir = stage("ref_follows_runs/walk");
    assert_eq!(
        sources_of(&dir, "both-langs"),
        ["src/lang/en/m.txt", "src/lang/fr/m.txt"]
    );
    assert_eq!(sources_of(&dir, "refs-a-cycle"), ["src/c.txt"]);
    let cycle = run(&dir, &["cyc-a"]);
    assert!(
        cycle.stderr.contains("Cyclic dependency detected"),
        "{}",
        cycle.combined()
    );
    assert_eq!(cycle.code, 201);

    let x255 = format!("FROM={}", "x".repeat(255));
    assert!(run(&dir, &["countdown", &x255]).ok());
    let x256 = format!("{x255}x");
    let out = run(&dir, &["countdown", &x256]);
    assert_eq!(out.code, 204, "{}", out.combined());
    assert!(
        out.stderr.starts_with(
            "task: countdown: sources: {task: count}: more than 256 calls of \"count\" reached \
             (count N= ← count N=x ← count N=xx ← … ← count N=xxxxx"
        ),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.ends_with(
            "): list the targets explicitly, or reference a task that does not recurse\n"
        ),
        "{}",
        out.stderr
    );
    let out = run(&dir, &["forever"]);
    assert_eq!(out.code, 204, "{}", out.combined());
    assert!(!out.stdout.contains("never"), "{}", out.combined());
    // Under a caller, wrapped as a failed run unless `--exit-code`.
    for (args, code) in [
        (&["calls-forever"][..], 201),
        (&["-x", "calls-forever"], 204),
    ] {
        let out = run(&dir, args);
        assert_eq!(out.code, code, "{args:?}: {}", out.combined());
        assert!(
            out.stderr.starts_with(
                "task: Failed to run task \"calls-forever\": task: forever: sources: {task: \
                 count-up}: more than 256 calls"
            ),
            "{}",
            out.stderr
        );
    }
}

/// A reached task's `exclude:` removes neither its deps' files nor the
/// referrer's.
#[test]
fn a_reached_tasks_exclusions_stay_its_own() {
    let dir = stage("ref_follows_runs/walk");
    assert_eq!(
        sources_of(&dir, "run-excludes"),
        ["own.txt", "src/d.txt", "src/x.txt"]
    );
}

/// A task excluded on this platform that a reached task runs is left out,
/// uncompiled, whichever reference comes first; one a reference names is in.
#[cfg(not(windows))]
#[test]
fn platform_pruning_does_not_depend_on_order() {
    let dir = stage("ref_follows_runs/walk");
    for task in ["pruned-first", "pruned-last"] {
        assert_eq!(
            sources_of(&dir, task),
            ["src/elsewhere.txt", "src/here.txt"],
            "{task}"
        );
    }
    assert_eq!(sources_of(&dir, "pruned-sh"), ["src/here.txt"]);
}

/// A deferred call naming its target from CHECKSUM is refused too, in both
/// dialects, with its references in `generates`.
#[test]
fn deferred_call_naming_from_checksum_is_refused() {
    let dir = stage("ref_follows_runs/walk");
    for prefix in ["", "jin:"] {
        let out = run(&dir, &[&format!("{prefix}gen-checksum")]);
        assert_eq!(out.code, 1, "{prefix}: {}", out.combined());
        assert!(
            out.stderr.contains("ck-name-")
                && out
                    .stderr
                    .contains("reads CHECKSUM: CHECKSUM is only available"),
            "{prefix}: {}",
            out.stderr
        );
    }
}

/// A reference cycle with no files leaves no checksum, and no error.
#[test]
fn a_reference_cycle_without_files_has_no_checksum() {
    let dir = stage("ref_follows_runs/walk");
    let out = run(&dir, &["self-only"]);
    assert!(out.ok(), "{}", out.combined());
    assert!(out.stdout.contains("CHECKSUM=[]\n"), "{}", out.combined());
}

/// An alias two tasks share, reached by a reference, is as ambiguous as when
/// run: exit code 203, wrapped under a caller unless `--exit-code`.
#[test]
fn an_ambiguous_alias_reached_keeps_its_exit_code() {
    let dir = stage("ref_follows_runs/walk");
    // Run directly: as ever.
    let out = run(&dir, &["dup"]);
    assert_eq!(
        out.stderr,
        "task: Multiple tasks (dup-a, dup-b) match the alias \"dup\"\n"
    );
    assert_eq!(out.code, 203);
    let conflict = "task: conflict: sources: {task: calls-dup} → deps \"dup\": multiple tasks \
                    (dup-a, dup-b) match the alias \"dup\"\n";
    for (args, code, wrapped) in [
        (&["conflict"][..], 203, false),
        (&["calls-conflict"], 201, true),
        (&["-x", "calls-conflict"], 203, true),
    ] {
        let out = run(&dir, args);
        let want = if wrapped {
            format!("task: Failed to run task \"calls-conflict\": {conflict}")
        } else {
            conflict.to_string()
        };
        assert_eq!(out.stderr, want, "{args:?}");
        assert_eq!(out.code, code, "{args:?}");
    }
}

/// Groups are numbered in the order their entries come, here the reached
/// task's before the one reaching it, and the identity follows.
#[test]
fn groups_are_numbered_as_entries_come() {
    for _ in 0..2 {
        let dir = stage("ref_follows_runs/exclusions");
        assert_eq!(
            rules_of(&dir, &["--status", "nested"]),
            ["@1:!**/*.tmp", "@1:a/**", "@1:shared/**", "@2:b/**"]
        );
        assert!(run(&dir, &["nested"]).ok());
        let file = checksum_file(&dir, "nested").unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "nested-21ae427803177def");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "cac60e0a61ad5eadb708765f1faae251\nf2f36b292df325ee8abf7808417ca552\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
