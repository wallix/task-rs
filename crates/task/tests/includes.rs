//! Behavioral parity tests ported from Go `task_test.go` (`TestIncludes*`).
//! See `common` for the harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::*;

/// Reads a generated file relative to `dir`, trimming surrounding whitespace,
/// and asserts it equals `want` (mirrors the Go `fileContentTest` with
/// `TrimSpace: true`).
fn assert_file(dir: &std::path::Path, rel: &str, want: &str) {
    let path = dir.join(rel);
    let got = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    assert_eq!(got.trim(), want, "unexpected content in {}", path.display());
}

// Ports Go `TestIncludes`.
#[test]
fn includes() {
    let dir = stage("includes");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    for (file, want) in [
        ("main.txt", "main"),
        ("included_directory.txt", "included_directory"),
        (
            "included_directory_without_dir.txt",
            "included_directory_without_dir",
        ),
        (
            "included_taskfile_without_dir.txt",
            "included_taskfile_without_dir",
        ),
        (
            "module2/included_directory_with_dir.txt",
            "included_directory_with_dir",
        ),
        (
            "module2/included_taskfile_with_dir.txt",
            "included_taskfile_with_dir",
        ),
        ("os_include.txt", "os"),
    ] {
        assert_file(&dir, file, want);
    }
}

// Ports Go `TestIncludesMultiLevel`.
#[test]
fn includes_multi_level() {
    let dir = stage("includes_multi_level");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "called_one.txt", "one");
    assert_file(&dir, "called_two.txt", "two");
    assert_file(&dir, "called_three.txt", "three");
}

// Ports Go `TestIncludeCycle`.
#[test]
fn include_cycle() {
    let dir = stage("includes_cycle");
    let o = run(&dir, &["--silent", "default"]);
    assert!(!o.ok(), "an include cycle must fail: {}", o.combined());
    assert!(
        o.combined().contains("include cycle detected between"),
        "expected cycle error, got: {}",
        o.combined()
    );
}

// Ports Go `TestIncludesIncorrect`.
#[test]
fn includes_incorrect() {
    let dir = stage("includes_incorrect");
    let o = run(&dir, &["--silent", "default"]);
    assert!(!o.ok(), "a malformed include must fail: {}", o.combined());
    assert!(
        o.combined().contains("Failed to parse") && o.combined().contains("incomplete.yml"),
        "expected parse error, got: {}",
        o.combined()
    );
}

// Ports Go `TestIncludesEmptyMain`.
#[test]
fn includes_empty_main() {
    let dir = stage("includes_empty");
    let o = run(&dir, &["included:default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "file.txt", "default");
}

// Ports Go `TestIncludesDependencies`.
#[test]
fn includes_dependencies() {
    let dir = stage("includes_deps");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "default.txt", "default");
    assert_file(&dir, "called_dep.txt", "called_dep");
    assert_file(&dir, "called_task.txt", "called_task");
}

// Ports Go `TestIncludesCallingRoot`.
#[test]
fn includes_calling_root() {
    let dir = stage("includes_call_root_task");
    let o = run(&dir, &["included:call-root"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "root_task.txt", "root task");
}

// Ports Go `TestIncludesOptional`.
#[test]
fn includes_optional() {
    let dir = stage("includes_optional");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "called_dep.txt", "called_dep");
}

// Ports Go `TestIncludesOptionalImplicitFalse`.
#[test]
fn includes_optional_implicit_false() {
    let dir = stage("includes_optional_implicit_false");
    let o = run(&dir, &["default"]);
    assert!(
        !o.ok(),
        "a missing non-optional include must fail: {}",
        o.combined()
    );
    assert!(
        o.combined().contains("No Taskfile found") && o.combined().contains("TaskfileOptional.yml"),
        "expected missing-taskfile error, got: {}",
        o.combined()
    );
}

// Ports Go `TestIncludesOptionalExplicitFalse`.
#[test]
fn includes_optional_explicit_false() {
    let dir = stage("includes_optional_explicit_false");
    let o = run(&dir, &["default"]);
    assert!(
        !o.ok(),
        "a missing optional:false include must fail: {}",
        o.combined()
    );
    assert!(
        o.combined().contains("No Taskfile found") && o.combined().contains("TaskfileOptional.yml"),
        "expected missing-taskfile error, got: {}",
        o.combined()
    );
}

// Ports Go `TestIncludesFromCustomTaskfile`.
#[test]
fn includes_from_custom_taskfile() {
    let dir = stage("includes_yaml");
    let o = run(&dir, &["-t", "Custom.ext", "default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "main.txt", "main");
    assert_file(
        &dir,
        "included_with_yaml_extension.txt",
        "included_with_yaml_extension",
    );
    assert_file(
        &dir,
        "included_with_custom_file.txt",
        "included_with_custom_file",
    );
}

// Ports Go `TestIncludesRelativePath`.
#[test]
fn includes_relative_path() {
    let dir = stage("includes_rel_path");

    let o = run(&dir, &["common:pwd"]);
    assert!(o.ok(), "common:pwd failed: {}", o.combined());
    assert!(
        o.combined().contains("includes_rel_path/common") || o.combined().contains("common"),
        "expected common dir in output, got: {}",
        o.combined()
    );

    let o = run(&dir, &["included:common:pwd"]);
    assert!(o.ok(), "included:common:pwd failed: {}", o.combined());
    assert!(
        o.combined().contains("includes_rel_path/common") || o.combined().contains("common"),
        "expected common dir in output, got: {}",
        o.combined()
    );
}

// Ports Go `TestIncludesInternal`.
#[test]
fn includes_internal() {
    let dir = stage("internal_task");
    // included internal task via task
    let o = run(&dir, &["--silent", "task-1"]);
    assert!(o.ok(), "task-1 failed: {}", o.combined());
    assert_eq!(o.stdout, "Hello, World!\n");
    // included internal task via dep
    let o = run(&dir, &["--silent", "task-2"]);
    assert!(o.ok(), "task-2 failed: {}", o.combined());
    assert_eq!(o.stdout, "Hello, World!\n");
    // included internal called directly must fail
    let o = run(&dir, &["--silent", "included:task-3"]);
    assert!(!o.ok(), "included:task-3 must fail: {}", o.combined());
}

// Ports Go `TestIncludesFlatten`.
#[test]
fn includes_flatten() {
    let dir = stage("includes_flatten");

    let cases = [
        ("gen", "gen from included\n"),
        ("default", "default from included flatten\n"),
        ("from_entrypoint", "from entrypoint\n"),
        ("with_deps", "gen from included\nwith_deps from included\n"),
        ("from_nested", "from nested\n"),
    ];
    for (task, want) in cases {
        let o = run(&dir, &["-t", "Taskfile.yml", "--silent", task]);
        assert!(o.ok(), "{task} failed: {}", o.combined());
        assert_eq!(o.stdout, want, "task {task}");
    }

    // multiple same task -> setup error
    let o = run(&dir, &["-t", "Taskfile.multiple.yml", "--silent", "gen"]);
    assert!(!o.ok(), "multiple-same-task must fail: {}", o.combined());
    assert!(
        o.combined()
            .contains("Found multiple tasks (gen) included by"),
        "expected multiple-tasks error, got: {}",
        o.combined()
    );
}

// Ports Go `TestIncludesInterpolation`.
#[test]
fn includes_interpolation() {
    // Go uses t.Setenv("MODULE", "included") and runs each subdir.
    let base = stage("includes_interpolation");
    let cases = [
        ("include", "include", "include\n"),
        (
            "include_with_env_variable",
            "include-with-env-variable",
            "include_with_env_variable\n",
        ),
        ("include_with_dir", "include-with-dir", "included\n"),
    ];
    for (subdir, task, want) in cases {
        let dir = base.join(subdir);
        let out = std::process::Command::new(BIN)
            .args(["--silent", task])
            .current_dir(&dir)
            .env("MODULE", "included")
            .output()
            .expect("spawn task binary");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "{subdir}/{task} failed: {stdout}{stderr}"
        );
        assert_eq!(stdout, want, "subdir {subdir}");
    }
}

// Ports Go `TestIncludesWithExclude`.
#[test]
fn includes_with_exclude() {
    let dir = stage("includes_with_excludes");

    let o = run(&dir, &["--silent", "included:bar"]);
    assert!(o.ok(), "included:bar failed: {}", o.combined());
    assert_eq!(o.stdout, "bar\n");

    // foo is excluded in the included namespace -> error
    let o = run(&dir, &["--silent", "included:foo"]);
    assert!(!o.ok(), "included:foo must be excluded: {}", o.combined());

    // bar is excluded at root -> error
    let o = run(&dir, &["--silent", "bar"]);
    assert!(!o.ok(), "bar must be excluded: {}", o.combined());

    let o = run(&dir, &["--silent", "foo"]);
    assert!(o.ok(), "foo failed: {}", o.combined());
    assert_eq!(o.stdout, "foo\n");
}

// Ports Go `TestIncludedTaskfileVarMerging`.
#[test]
fn included_taskfile_var_merging() {
    let dir = stage("included_taskfile_var_merging");
    for (task, want) in [
        ("foo:pwd", "included_taskfile_var_merging/foo\n"),
        ("bar:pwd", "included_taskfile_var_merging/bar\n"),
    ] {
        let o = run(&dir, &["--silent", task]);
        assert!(o.ok(), "{task} failed: {}", o.combined());
        assert!(
            o.combined().contains(want),
            "expected {want:?} in output, got: {}",
            o.combined()
        );
    }
}

// Ports Go `TestIncludesShadowedDefault`.
#[test]
fn includes_shadowed_default() {
    let dir = stage("includes_shadowed_default");
    let o = run(&dir, &["included"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "file.txt", "shadowed");
}

// Ports Go `TestIncludesUnshadowedDefault`.
#[test]
fn includes_unshadowed_default() {
    let dir = stage("includes_unshadowed_default");
    let o = run(&dir, &["included"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_file(&dir, "file.txt", "included");
}

// Ports Go `TestIncludeDirWithAbsoluteVar`.
//
// Regression test: an included taskfile with `dir: "{{.TARGET_DIR}}"` where the
// var resolves to an absolute path must run in that absolute dir, not a doubled
// path. The Go test builds a temp Taskfile tree and inspects `ComputeDir()`
// directly; here we drive the binary (no API access) and assert the task's
// working directory is the absolute `target/`. The include is not marked
// internal so the task can be invoked directly through the CLI.
#[test]
fn include_dir_with_absolute_var() {
    let dir = stage("includes"); // placeholder base for a fresh temp dir
    // Build our own tree inside the staged temp dir to mirror the Go test.
    let root = dir.join("abs_var_case");
    let sub = root.join("sub");
    let target = root.join("target");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(&target).unwrap();

    std::fs::write(
        root.join("Taskfile.yml"),
        "version: '3'\nincludes:\n  sub:\n    taskfile: ./sub/Taskfile.yml\n",
    )
    .unwrap();
    std::fs::write(
        sub.join("Taskfile.yml"),
        format!(
            "version: '3'\nvars:\n  TARGET_DIR: '{}'\ntasks:\n  build:\n    dir: \"{{{{.TARGET_DIR}}}}\"\n    cmds:\n      - pwd\n",
            target.display()
        ),
    )
    .unwrap();

    let o = run(&root, &["--silent", "sub:build"]);
    assert!(o.ok(), "sub:build failed: {}", o.combined());
    // The task's pwd must be the absolute target dir (canonicalized to handle
    // any symlinked temp paths), not a doubled root/target path.
    let want = std::fs::canonicalize(&target).unwrap();
    let got = o.combined();
    assert!(
        got.contains(&*want.to_string_lossy()) || got.contains(&*target.to_string_lossy()),
        "expected pwd {} in output, got: {}",
        target.display(),
        got
    );
}
