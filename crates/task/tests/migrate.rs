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
    // The warning must occupy a line of its own instead of running into
    // whatever is written next. Checked as an explicit newline rather than via
    // `lines()`, which would also accept an unterminated final line.
    assert!(
        stderr.contains("will be removed in a future release)\n"),
        "warning should be newline-terminated, got: {stderr}"
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

/// A Go-dialect Taskfile whose template holds a dot inside a string literal —
/// the shape that used to lose the dot and render `report.` instead of
/// `report`, both when run directly and after `--migrate --write`.
const DOTTED_LITERAL_TASKFILE: &str = "version: '3'\n\nvars:\n  NAME: 'report.tar.gz'\ntasks:\n  strip:\n    cmds:\n      - 'echo out={{ .NAME | replace \".tar.gz\" \"\" }}'\n";

#[test]
fn dot_inside_a_string_literal_renders_and_migrates() {
    // The Go dialect renders the literal as written.
    let go = taskfile_dir(DOTTED_LITERAL_TASKFILE);
    let r = common::run(&go, &["strip"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(combined.contains("out=report"), "output: {combined}");
    assert!(!combined.contains("out=report."), "dot leaked: {combined}");

    // Migration keeps the literal intact, and the converted file still runs.
    let jinja = taskfile_dir(DOTTED_LITERAL_TASKFILE);
    let w = common::run(&jinja, &["--migrate", "--write"]);
    assert!(w.ok(), "stderr: {}", w.stderr);
    let on_disk = std::fs::read_to_string(jinja.join("Taskfile.yml")).unwrap();
    assert!(
        on_disk.contains(r#"replace(".tar.gz", "")"#),
        "migrated to: {on_disk}"
    );
    let r = common::run(&jinja, &["strip"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(combined.contains("out=report"), "output: {combined}");
    assert!(!combined.contains("out=report."), "dot leaked: {combined}");
}

/// A Go-dialect Taskfile with an escaped quote inside a template string — the
/// shape whose literal used to close at the `\"`, merging the next argument in
/// and failing with "missing argument".
const ESCAPED_QUOTE_TASKFILE: &str = "version: '3'\n\nvars:\n  P: 'a\"b'\ntasks:\n  strip:\n    cmds:\n      - 'echo out={{ .P | replace \"\\\"\" \"-\" }}'\n";

#[test]
fn escaped_quote_in_a_string_literal_renders_and_migrates() {
    // The Go dialect renders it instead of erroring on a merged argument.
    let go = taskfile_dir(ESCAPED_QUOTE_TASKFILE);
    let r = common::run(&go, &["strip"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(combined.contains("out=a-b"), "output: {combined}");

    // Migration keeps both literals separate, and the converted file runs.
    let jinja = taskfile_dir(ESCAPED_QUOTE_TASKFILE);
    let w = common::run(&jinja, &["--migrate", "--write"]);
    assert!(w.ok(), "stderr: {}", w.stderr);
    let on_disk = std::fs::read_to_string(jinja.join("Taskfile.yml")).unwrap();
    assert!(
        on_disk.contains(r#"replace("\"", "-")"#),
        "migrated to: {on_disk}"
    );
    let r = common::run(&jinja, &["strip"]);
    assert!(r.ok(), "stderr: {}", r.stderr);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(combined.contains("out=a-b"), "output: {combined}");
}
