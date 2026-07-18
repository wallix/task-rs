//! Behavioral parity tests ported from Go `task_test.go` covering the
//! `dir:` attribute, `--status` reporting, and `setup:` semantics. See
//! `common` for the harness.
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

/// Returns the final path component of `pwd`-style output, mirroring the Go
/// tests' `filepath.Base(strings.TrimSuffix(out, "\n"))`.
fn base_of(out: &str) -> String {
    let trimmed = out.trim();
    Path::new(trimmed)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// Ports Go `TestWhenNoDirAttributeItRunsInSameDirAsTaskfile`.
#[test]
fn no_dir_attribute_runs_in_taskfile_dir() {
    let dir = stage("dir");
    let o = run(&dir, &["whereami"]);
    assert!(o.ok(), "whereami failed: {}", o.combined());
    // With no `dir:` the task runs in the Taskfile's directory (the staged dir).
    let expected = base_of(&dir.to_string_lossy());
    assert_eq!(
        base_of(&o.stdout),
        expected,
        "mismatch in working directory"
    );
}

// Ports Go `TestWhenDirAttributeAndDirExistsItRunsInThatDir`.
#[test]
fn dir_attribute_existing_runs_in_that_dir() {
    let dir = stage("dir/explicit_exists");
    let o = run(&dir, &["whereami"]);
    assert!(o.ok(), "whereami failed: {}", o.combined());
    assert_eq!(
        base_of(&o.stdout),
        "exists",
        "mismatch in working directory"
    );
}

// Ports Go `TestWhenDirAttributeItCreatesMissingAndRunsInThatDir`.
#[test]
fn dir_attribute_creates_missing_and_runs_in_it() {
    let dir = stage("dir/explicit_doesnt_exist");
    let created = dir.join("createme");
    assert!(
        !created.exists(),
        "createme should not exist before the run"
    );

    let o = run(&dir, &["whereami"]);
    assert!(o.ok(), "whereami failed: {}", o.combined());
    assert_eq!(
        base_of(&o.stdout),
        "createme",
        "mismatch in working directory"
    );
    assert!(created.exists(), "the missing dir should have been created");
}

// Ports Go `TestDynamicVariablesRunOnTheNewCreatedDir`.
#[test]
fn dynamic_variables_run_on_created_dir() {
    let dir = stage("dir/dynamic_var_on_created_dir");
    let created = dir.join("created");
    assert!(!created.exists(), "created should not exist before the run");

    let o = run(&dir, &["default"]);
    assert!(o.ok(), "default failed: {}", o.combined());
    // The `sh:` var runs `pwd` in the (created) task dir; the echoed value's
    // basename must be the created dir.
    assert_eq!(
        base_of(&o.stdout),
        "created",
        "mismatch in working directory"
    );
    assert!(created.exists(), "the missing dir should have been created");
}

// Ports Go `TestStatusChecksum` (case: build).
#[test]
fn status_checksum_build() {
    let dir = stage("checksum");
    let _ = std::fs::remove_file(dir.join("generated.txt"));
    let checksum = dir.join(".task/checksum/build");
    assert!(!checksum.exists());

    let o = run(&dir, &["build"]);
    assert!(o.ok(), "build failed: {}", o.combined());
    assert!(dir.join("generated.txt").exists(), "generated.txt missing");
    assert!(checksum.exists(), "checksum file missing after run");

    // Rerun with unchanged sources: reported up to date, checksum untouched.
    let mtime_before = std::fs::metadata(&checksum).unwrap().modified().unwrap();
    let second = run(&dir, &["build"]);
    assert!(
        second.combined().contains("is up to date"),
        "expected up to date on rerun: {}",
        second.combined()
    );
    let mtime_after = std::fs::metadata(&checksum).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "checksum file must not be regenerated when the hash is unchanged"
    );
}

// Ports Go `TestStatusChecksum` (case: build-wildcard). The wildcard task uses
// `{{index .MATCH 0}}` in its cmd/generates.
#[test]
fn status_checksum_build_wildcard() {
    let dir = stage("checksum");
    let _ = std::fs::remove_file(dir.join("generated-wildcard.txt"));
    let o = run(&dir, &["build-wildcard"]);
    assert!(o.ok(), "build-wildcard failed: {}", o.combined());
    assert!(dir.join("generated-wildcard.txt").exists());
}

// Ports Go `TestStatusChecksum` (case: build-with-status).
#[test]
fn status_checksum_build_with_status() {
    let dir = stage("checksum");
    let _ = std::fs::remove_file(dir.join("generated.txt"));
    let checksum = dir.join(".task/checksum/build-with-status");
    assert!(!checksum.exists());

    let o = run(&dir, &["build-with-status"]);
    assert!(o.ok(), "build-with-status failed: {}", o.combined());
    assert!(dir.join("generated.txt").exists(), "generated.txt missing");
    assert!(checksum.exists(), "checksum file missing after run");

    let mtime_before = std::fs::metadata(&checksum).unwrap().modified().unwrap();
    let second = run(&dir, &["build-with-status"]);
    assert!(
        second.combined().contains("is up to date"),
        "expected up to date on rerun: {}",
        second.combined()
    );
    let mtime_after = std::fs::metadata(&checksum).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "checksum must not be regenerated"
    );
}

/// Writes a minimal sources/generates Taskfile plus its source file into a
/// fresh staged temp dir (reuses the `dir` case only for its throwaway temp
/// location; overwrites its Taskfile).
fn stage_build_taskfile() -> std::path::PathBuf {
    let dir = stage("dir");
    // Clean out the staged fixture files we don't need.
    let _ = std::fs::remove_file(dir.join("Taskfile.yml"));
    std::fs::write(dir.join("source.txt"), "hello").unwrap();
    std::fs::write(
        dir.join("Taskfile.yml"),
        "version: '3'\n\
         tasks:\n\
        \x20 build:\n\
        \x20   cmds:\n\
        \x20     - cp source.txt generated.txt\n\
        \x20   sources:\n\
        \x20     - source.txt\n\
        \x20   generates:\n\
        \x20     - generated.txt\n",
    )
    .unwrap();
    dir
}

// Ports Go `TestStatusCommand`.
#[test]
fn status_command() {
    let dir = stage_build_taskfile();

    // Before running: task should not be up to date.
    let before = run(&dir, &["--status", "build"]);
    let out = before.combined();
    assert!(out.contains("is not up to date"), "got: {out}");
    assert!(out.contains("sources: changed"), "got: {out}");
    assert!(out.contains("generates: changed"), "got: {out}");
    assert!(out.contains("srcrule:"), "got: {out}");
    assert!(out.contains("file:"), "got: {out}");

    // Run the task.
    assert!(run(&dir, &["build"]).ok());

    // After running: both sources and generates should be up to date.
    let after = run(&dir, &["--status", "build"]);
    let out = after.combined();
    assert!(out.contains("is up to date"), "got: {out}");
    assert!(out.contains("sources: up to date"), "got: {out}");
    assert!(out.contains("generates: up to date"), "got: {out}");
}

/// Extracts the string value of `"<field>": "..."` from pretty JSON, if any.
fn json_str_field<'a>(json: &'a str, field: &str) -> Option<&'a str> {
    let key = format!("\"{field}\":");
    let after = json.split_once(&key)?.1.trim_start();
    let rest = after.strip_prefix('"')?;
    rest.split_once('"').map(|(v, _)| v)
}

// Ports Go `TestStatusCommandJSON`. The `task` crate has no `serde_json`
// dev-dependency, so the stable pretty-printed JSON is asserted textually:
// exact bool fields and non-empty string/array fields.
#[test]
fn status_command_json() {
    let dir = stage_build_taskfile();

    // Before running.
    let before = run(&dir, &["--status", "--json", "build"]);
    assert!(before.ok(), "status json failed: {}", before.combined());
    let out = before.stdout;
    // A single-element array.
    assert!(
        out.trim_start().starts_with('['),
        "expected JSON array: {out}"
    );
    assert_eq!(
        out.matches("\"task\":").count(),
        1,
        "expected one entry: {out}"
    );
    assert!(out.contains("\"task\": \"build\""), "got: {out}");
    assert!(out.contains("\"up_to_date\": false"), "got: {out}");
    assert!(out.contains("\"sources_up_to_date\": false"), "got: {out}");
    assert!(
        out.contains("\"generates_up_to_date\": false"),
        "got: {out}"
    );
    assert!(
        json_str_field(&out, "checksum_file").is_some_and(|v| !v.is_empty()),
        "checksum_file empty: {out}"
    );
    assert!(
        out.contains("\"source_data\": ["),
        "source_data empty: {out}"
    );

    // Run the task.
    assert!(run(&dir, &["build"]).ok());

    // After running.
    let after = run(&dir, &["--status", "--json", "build"]);
    let out = after.stdout;
    assert!(out.contains("\"up_to_date\": true"), "got: {out}");
    assert!(out.contains("\"sources_up_to_date\": true"), "got: {out}");
    assert!(out.contains("\"generates_up_to_date\": true"), "got: {out}");
    assert!(
        json_str_field(&out, "sources_hash").is_some_and(|v| !v.is_empty()),
        "sources_hash empty: {out}"
    );
    assert!(
        json_str_field(&out, "generates_hash").is_some_and(|v| !v.is_empty()),
        "generates_hash empty: {out}"
    );
}

// Ports Go `TestStatusCommandNoSources`.
#[test]
fn status_command_no_sources() {
    let dir = stage("vars");
    let o = run(&dir, &["--status", "default"]);
    assert!(
        o.combined().contains("has no sources or generates"),
        "got: {}",
        o.combined()
    );
}

/// Writes a Taskfile + source files into a fresh staged temp dir.
fn stage_with(files: &[(&str, &str)], taskfile: &str) -> std::path::PathBuf {
    let dir = stage("dir");
    let _ = std::fs::remove_file(dir.join("Taskfile.yml"));
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    dir
}

// Ports Go `TestSetupRunsBeforeFingerprint`.
#[test]
fn setup_runs_before_fingerprint() {
    let dir = stage_with(
        &[("version.txt", "v1")],
        "version: '3'\n\
         tasks:\n\
        \x20 enforce-version:\n\
        \x20   cmds:\n\
        \x20     - echo \"v2\" > version.txt\n\
        \x20 build:\n\
        \x20   setup:\n\
        \x20     - enforce-version\n\
        \x20   sources:\n\
        \x20     - version.txt\n\
        \x20   cmds:\n\
        \x20     - cp version.txt output.txt\n\
        \x20   generates:\n\
        \x20     - output.txt\n",
    );

    // First run: setup modifies version.txt, then build runs.
    assert!(run(&dir, &["build"]).ok());
    let output = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    assert!(
        output.contains("v2"),
        "setup should have updated version.txt before build ran, got: {output:?}"
    );

    // Second run: setup runs again but content is identical → up to date.
    let second = run(&dir, &["build"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );
}

// Ports Go `TestSetupRunsEvenWhenUpToDate`.
#[test]
fn setup_runs_even_when_up_to_date() {
    let dir = stage_with(
        &[("source.txt", "hello")],
        "version: '3'\n\
         tasks:\n\
        \x20 track:\n\
        \x20   cmds:\n\
        \x20     - echo \"setup-ran\" >> setup.log\n\
        \x20 build:\n\
        \x20   setup:\n\
        \x20     - track\n\
        \x20   sources:\n\
        \x20     - source.txt\n\
        \x20   cmds:\n\
        \x20     - cp source.txt output.txt\n\
        \x20   generates:\n\
        \x20     - output.txt\n",
    );

    assert!(run(&dir, &["build"]).ok());
    assert!(run(&dir, &["build"]).ok());

    let log = std::fs::read_to_string(dir.join("setup.log")).unwrap();
    let lines: Vec<&str> = log.trim().split('\n').collect();
    assert_eq!(
        lines.len(),
        2,
        "setup should have run twice (once per invocation), got: {log:?}"
    );
}

// Ports Go `TestSetupDoesNotAffectFingerprint`.
#[test]
fn setup_does_not_affect_fingerprint() {
    let dir = stage_with(
        &[("version.txt", "v1"), ("source.txt", "hello")],
        "version: '3'\n\
         tasks:\n\
        \x20 enforce-version:\n\
        \x20   sources:\n\
        \x20     - version.txt\n\
        \x20   generates:\n\
        \x20     - resolved-version.txt\n\
        \x20   cmds:\n\
        \x20     - cp version.txt resolved-version.txt\n\
        \x20 build:\n\
        \x20   setup:\n\
        \x20     - enforce-version\n\
        \x20   sources:\n\
        \x20     - source.txt\n\
        \x20   cmds:\n\
        \x20     - cat source.txt resolved-version.txt > output.txt\n\
        \x20   generates:\n\
        \x20     - output.txt\n",
    );

    // First run: both setup and build execute.
    assert!(run(&dir, &["build"]).ok());
    let output = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    assert!(output.contains("v1"), "got: {output:?}");

    // Second run: everything up to date.
    let second = run(&dir, &["build"]);
    assert!(
        second.combined().contains("up to date"),
        "got: {}",
        second.combined()
    );

    // Change version.txt (setup task's source); parent source.txt is unchanged.
    // Parent must stay up to date since setup sources are not merged.
    std::fs::write(dir.join("version.txt"), "v2").unwrap();
    let third = run(&dir, &["build"]);
    assert!(
        third.combined().contains("Task \"build\" is up to date"),
        "parent should stay up to date when only setup source changes, got: {}",
        third.combined()
    );
}

// Ports Go `TestGeneratesWithoutSourcesAlwaysRuns`.
#[test]
fn generates_without_sources_always_runs() {
    let dir = stage_with(
        &[],
        "version: '3'\n\
         tasks:\n\
        \x20 build:\n\
        \x20   generates:\n\
        \x20     - output.txt\n\
        \x20   cmds:\n\
        \x20     - echo hello > output.txt\n",
    );

    // No sources → no fingerprint baseline → always executes.
    assert!(run(&dir, &["build"]).ok());
    assert!(dir.join("output.txt").exists());
    std::fs::remove_file(dir.join("output.txt")).unwrap();
    assert!(run(&dir, &["build"]).ok());
    assert!(
        dir.join("output.txt").exists(),
        "task with generates but no sources should re-run and recreate output"
    );
}

// Ports Go `TestSingleCmdDep`.
#[test]
fn single_cmd_dep() {
    let dir = stage("single_cmd_dep");
    let o = run(&dir, &["foo"]);
    assert!(o.ok(), "foo failed: {}", o.combined());
    assert_eq!(
        std::fs::read_to_string(dir.join("foo.txt")).unwrap(),
        "foo\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("bar.txt")).unwrap(),
        "bar\n"
    );
}

// Ports Go `TestDisplaysErrorOnVersion2Schema`.
#[test]
fn displays_error_on_version_2_schema() {
    let dir = stage("version/v2");
    let o = run(&dir, &[]);
    assert!(!o.ok(), "a v2 schema must be rejected");
    let out = o.combined();
    assert!(
        out.contains("version") && out.contains("v3"),
        "expected a schema-version error mentioning v3, got: {out}"
    );
}

// Ports Go `TestSupportedFileNames`.
#[test]
fn supported_file_names() {
    for name in [
        "Taskfile.yml",
        "Taskfile.yaml",
        "Taskfile.dist.yml",
        "Taskfile.dist.yaml",
    ] {
        // Each fixture dir holds a single Taskfile under its own name.
        let dir = stage(&format!("file_names/{name}"));
        let o = run(&dir, &["default"]);
        assert!(o.ok(), "{name}: {}", o.combined());
        let out = std::fs::read_to_string(dir.join("output.txt")).unwrap();
        assert_eq!(out.trim(), "hello", "{name}: unexpected output");
    }
}
