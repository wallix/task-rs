//! Behavioral parity tests ported from Go `task_test.go` (dotenv + variable
//! behaviors). See `common` for the harness.
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

/// Runs the binary in `dir` with `args` and extra environment variables set,
/// mirroring Go tests that call `t.Setenv` before executing.
fn run_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn task binary");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// Reads a generated file relative to `dir`, optionally trimming whitespace.
fn read_file(dir: &Path, name: &str, trim: bool) -> String {
    let s =
        std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("reading {name}: {e}"));
    if trim { s.trim().to_string() } else { s }
}

// Ports Go `TestDotenvShouldIncludeAllEnvFiles`. The Taskfile references
// sibling `../include1` env files, so the whole `dotenv` tree is staged.
#[test]
fn dotenv_should_include_all_env_files() {
    let root = stage("dotenv");
    let dir = root.join("default");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(
        read_file(&dir, "include.txt", false),
        "INCLUDE1='from_include1' INCLUDE2='from_include2'\n"
    );
}

// Ports Go `TestDotenvShouldErrorWhenIncludingDependantDotenvs`. References a
// sibling include, so the whole `dotenv` tree is staged.
#[test]
fn dotenv_should_error_when_including_dependant_dotenvs() {
    let root = stage("dotenv");
    let dir = root.join("error_included_envs");
    let o = run(&dir, &["default"]);
    assert!(!o.ok(), "expected an error: {}", o.combined());
    assert!(
        o.combined().contains("move the dotenv"),
        "missing 'move the dotenv' message: {}",
        o.combined()
    );
}

// Ports Go `TestDotenvShouldAllowMissingEnv`.
#[test]
fn dotenv_should_allow_missing_env() {
    let dir = stage("dotenv/missing_env");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(
        read_file(&dir, "include.txt", false),
        "INCLUDE1='' INCLUDE2=''\n"
    );
}

// Ports Go `TestDotenvHasLocalEnvInPath`.
#[test]
fn dotenv_has_local_env_in_path() {
    let dir = stage("dotenv/local_env_in_path");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(
        read_file(&dir, "var.txt", false),
        "VAR='var_in_dot_env_1'\n"
    );
}

// Ports Go `TestDotenvHasLocalVarInPath`.
#[test]
fn dotenv_has_local_var_in_path() {
    let dir = stage("dotenv/local_var_in_path");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(
        read_file(&dir, "var.txt", false),
        "VAR='var_in_dot_env_3'\n"
    );
}

// Ports Go `TestDotenvHasEnvVarInPath`. Sets ENV_VAR so the dotenv path
// `.env.{{.ENV_VAR}}` resolves to `.env.testing`.
#[test]
fn dotenv_has_env_var_in_path() {
    let dir = stage("dotenv/env_var_in_path");
    let o = run_env(&dir, &["default"], &[("ENV_VAR", "testing")]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(
        read_file(&dir, "var.txt", false),
        "VAR='var_in_dot_env_2'\n"
    );
}

// Ports Go `TestTaskDotenvParseErrorMessage`.
#[test]
fn task_dotenv_parse_error_message() {
    let dir = stage("dotenv/parse_error");
    let path = dir.join(".env-with-error");
    let expected = format!("error reading env file {}:", path.display());
    let o = run(&dir, &["default"]);
    assert!(!o.ok(), "expected an error: {}", o.combined());
    assert!(
        o.combined().contains(&expected),
        "missing expected substring {expected:?}: {}",
        o.combined()
    );
}

// Ports Go `TestTaskDotenv`.
#[test]
fn task_dotenv() {
    let dir = stage("dotenv_task/default");
    let o = run(&dir, &["dotenv"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(read_file(&dir, "dotenv.txt", true), "foo");
}

// Ports Go `TestTaskDotenvFail`.
#[test]
fn task_dotenv_fail() {
    let dir = stage("dotenv_task/default");
    let o = run(&dir, &["no-dotenv"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(read_file(&dir, "no-dotenv.txt", true), "global");
}

// Ports Go `TestTaskDotenvOverriddenByEnv`.
#[test]
fn task_dotenv_overridden_by_env() {
    let dir = stage("dotenv_task/default");
    let o = run(&dir, &["dotenv-overridden-by-env"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(
        read_file(&dir, "dotenv-overridden-by-env.txt", true),
        "overridden"
    );
}

// Ports Go `TestTaskDotenvWithVarName`.
#[test]
fn task_dotenv_with_var_name() {
    let dir = stage("dotenv_task/default");
    let o = run(&dir, &["dotenv-with-var-name"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(read_file(&dir, "dotenv-with-var-name.txt", true), "foo");
}

// Ports Go `TestCmdsVariables`. Runs verbosely and asserts the computed
// checksum of the sources appears in the output. `{{.CHECKSUM}}` is a 128-bit
// xxHash3 rendered identically to Go, so the expected value matches exactly.
#[test]
fn cmds_variables() {
    let dir = stage("cmds_vars");
    let _ = std::fs::remove_dir_all(dir.join(".task"));
    let o = run(&dir, &["--verbose", "build-checksum"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert!(
        o.combined().contains("44f88b429595ccb250265c2d1eca60a0"),
        "missing checksum in output: {}",
        o.combined()
    );
}

// Ports Go `TestExpand`. The `pwd` task has `dir: '~'`, so it should print the
// user's home directory.
#[test]
fn expand() {
    let dir = stage("expand");
    let home = dirs_home();
    let o = run(&dir, &["pwd"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(o.combined().trim(), home);
}

fn dirs_home() -> String {
    std::env::var("HOME").expect("HOME must be set")
}

// Ports Go `TestTaskVersion`. v1 and v2 schemas are no longer supported (must
// error); v3 must succeed.
#[test]
fn task_version() {
    // v1 and v2 must fail at setup.
    for v in ["version/v1", "version/v2"] {
        let dir = stage(v);
        let o = run(&dir, &["--list-all"]);
        assert!(!o.ok(), "{v} should error: {}", o.combined());
    }
    // v3 must succeed.
    let dir = stage("version/v3");
    let o = run(&dir, &["--list-all"]);
    assert!(o.ok(), "v3 should succeed: {}", o.combined());
}

// Ports Go `TestShortTaskNotation`.
#[test]
fn short_task_notation() {
    let dir = stage("short_task_notation");
    let o = run(&dir, &["--silent", "default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    assert_eq!(o.stdout, "string-slice-1\nstring-slice-2\nstring\n");
}

// Ports Go `TestDynamicVariablesShouldRunOnTheTaskDir`.
#[test]
fn dynamic_variables_should_run_on_the_task_dir() {
    let dir = stage("dir/dynamic_var");
    let o = run(&dir, &["default"]);
    assert!(o.ok(), "run failed: {}", o.combined());
    for (name, expect) in [
        ("subdirectory/from_root_taskfile.txt", "subdirectory\n"),
        ("subdirectory/from_included_taskfile.txt", "subdirectory\n"),
        (
            "subdirectory/from_included_taskfile_task.txt",
            "subdirectory\n",
        ),
        ("subdirectory/from_interpolated_dir.txt", "subdirectory\n"),
    ] {
        assert_eq!(read_file(&dir, name, false), expect, "file {name}");
    }
}
