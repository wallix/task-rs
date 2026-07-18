//! CLI-level tests for `--migrate`: converting a Go-dialect Taskfile to native
//! Jinja, previewing versus writing in place, and running the migrated file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh temp dir holding a `Taskfile.yml` with the given contents.
fn taskfile_dir(contents: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "task-migrate-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Taskfile.yml"), contents).unwrap();
    dir
}

const GO_TASKFILE: &str = "version: '3'\n\n# header\nvars:\n  DIR: '{{ joinPath .ROOT_DIR \"bin\" }}'\n  FMT: '{{if .CI}}ci{{else}}pkg{{end}}'\ntasks:\n  build:\n    cmds:\n      - 'echo dir={{.DIR}} fmt={{.FMT}}'\n";

#[test]
fn migrate_preview_does_not_touch_file() {
    let dir = taskfile_dir(GO_TASKFILE);
    let r = common::run(&dir, &["--migrate"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    // The preview goes to stdout and includes the marker and converted syntax.
    assert!(r.stdout.contains("version: '3'\ntemplater: jinja\n"));
    assert!(r.stdout.contains(r#"{{ joinPath(ROOT_DIR, "bin") }}"#));
    assert!(r.stdout.contains("{% if CI %}ci{% else %}pkg{% endif %}"));
    // The file on disk is unchanged.
    let on_disk = std::fs::read_to_string(dir.join("Taskfile.yml")).unwrap();
    assert_eq!(on_disk, GO_TASKFILE);
}

#[test]
fn migrate_write_applies_and_runs() {
    let dir = taskfile_dir(GO_TASKFILE);
    let w = common::run(&dir, &["--migrate", "--write"]);
    assert!(w.ok(), "stderr: {}", w.stderr);

    let on_disk = std::fs::read_to_string(dir.join("Taskfile.yml")).unwrap();
    assert!(on_disk.contains("templater: jinja"));
    assert!(on_disk.contains("# header"), "comments preserved");

    // The migrated file runs in Jinja mode and renders correctly. `FMT` selects
    // the `else` branch via `{% if CI %}`, which reads the process environment;
    // clear `CI` so the assertion is deterministic under CI runners that set it.
    let out = std::process::Command::new(common::BIN)
        .args(["build"])
        .current_dir(&dir)
        .env("TASK_NO_GO_DEPRECATION", "1")
        .env_remove("CI")
        .output()
        .expect("spawn task binary");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("fmt=pkg"), "output: {combined}");
}

/// Runs the binary without the harness's deprecation-suppression env, so the
/// migration nudge is visible.
fn run_with_warnings(dir: &std::path::Path, args: &[&str]) -> (String, i32) {
    let out = std::process::Command::new(common::BIN)
        .args(args)
        .current_dir(dir)
        .env_remove("TASK_NO_GO_DEPRECATION")
        .output()
        .expect("spawn task binary");
    (
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn go_dialect_warns_jinja_does_not() {
    let go = taskfile_dir(GO_TASKFILE);
    let (stderr, code) = run_with_warnings(&go, &["build"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("deprecated Go template dialect") && stderr.contains("--migrate"),
        "expected deprecation warning, got: {stderr}"
    );

    // After migrating, the file is Jinja and no longer warns.
    let jinja = taskfile_dir(GO_TASKFILE);
    assert!(common::run(&jinja, &["--migrate", "--write"]).ok());
    let (stderr, code) = run_with_warnings(&jinja, &["build"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !stderr.contains("deprecated"),
        "migrated file should not warn, got: {stderr}"
    );
}

#[test]
fn migrate_is_idempotent() {
    let dir = taskfile_dir(GO_TASKFILE);
    assert!(common::run(&dir, &["--migrate", "--write"]).ok());
    // A second migration detects the marker and leaves the file alone.
    let again = common::run(&dir, &["--migrate", "--write"]);
    assert!(again.ok());
    assert!(again.stderr.contains("already declares"));
}
