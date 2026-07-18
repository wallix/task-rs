//! Behavioral integration tests for the execution engine, driving real
//! Taskfiles in temporary directories. These port a representative subset of
//! the Go `task_test.go`/`executor_test.go` cases that do not require the CLI
//! binary: running a simple task, dependency ordering, up-to-date skipping,
//! dry-run, and a dynamic (`sh:`) variable.
//!
//! Command output goes to the process streams (the output styles are
//! `Rc`-based, so they cannot be captured through a public API without the CLI
//! wiring). These tests therefore assert on observable side effects — files
//! created and their contents — rather than on stdout.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use taskcore::call::Call;
use taskcore::executor::{Executor, PromptError, Prompter};

/// A prompter that answers every variable prompt with a fixed value.
struct FixedPrompter(String);

impl Prompter for FixedPrompter {
    fn confirm(&self, _message: &str) -> Result<bool, PromptError> {
        Ok(true)
    }
    fn prompt(&self, _name: &str, _enum_values: &[String]) -> Result<String, PromptError> {
        Ok(self.0.clone())
    }
}

/// Creates a unique temporary directory for a test.
fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut d = std::env::temp_dir();
    d.push(format!(
        "taskcore-exec-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        n
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Writes a Taskfile into `dir` and returns the directory path as a string.
fn write_taskfile(dir: &Path, contents: &str) -> String {
    fs::write(dir.join("Taskfile.yml"), contents).unwrap();
    dir.to_string_lossy().into_owned()
}

/// Runs an async body on a current-thread runtime with a `LocalSet`, as the CLI
/// must (the engine is single-threaded).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, fut)
}

/// Sets up an executor for `dir`, runs the named tasks, and returns the result.
async fn setup_and_run(dir: &str, tasks: &[&str]) -> Result<(), taskcore::executor::ExecutorError> {
    setup_and_run_with(dir, tasks, |e| e).await
}

/// As [`setup_and_run`] but applies `configure` to the executor before setup.
async fn setup_and_run_with<F>(
    dir: &str,
    tasks: &[&str],
    configure: F,
) -> Result<(), taskcore::executor::ExecutorError>
where
    F: FnOnce(Executor) -> Executor,
{
    let mut e = configure(Executor::new().with_dir(dir).with_silent(true));
    e.setup().await?;
    let e = Rc::new(e);
    let calls: Vec<Call> = tasks.iter().map(|t| Call::new(*t)).collect();
    e.run(&calls).await
}

#[test]
fn prompts_for_missing_required_var() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  greet:
    requires:
      vars: [NAME]
    cmds:
      - echo "hi {{.NAME}}" > out.txt
"#,
    );
    block_on(async {
        let mut e = Executor::new()
            .with_dir(&d)
            .with_silent(true)
            .with_interactive(true)
            .with_assume_term(true)
            .with_prompter(Box::new(FixedPrompter("world".to_string())));
        e.setup().await.unwrap();
        let e = Rc::new(e);
        // NAME is required but unset: it is prompted (answered "world") rather
        // than erroring, and the task runs with the supplied value.
        e.run(&[Call::new("greet")]).await.unwrap();
        let out = fs::read_to_string(dir.join("out.txt")).unwrap();
        assert_eq!(out.trim(), "hi world");
    });
}

#[test]
fn missing_required_var_errors_without_a_prompter() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  greet:
    requires:
      vars: [NAME]
    cmds:
      - echo hi > out.txt
"#,
    );
    // Non-interactive (no prompter): the missing required var is an error.
    let result = block_on(setup_and_run(&d, &["greet"]));
    assert!(
        result.is_err(),
        "missing required var must error when not prompting"
    );
    assert!(!dir.join("out.txt").exists());
}

#[test]
fn status_reports_without_running_the_task() {
    let dir = temp_dir();
    fs::write(dir.join("src.txt"), "input").unwrap();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  build:
    sources: ['src.txt']
    generates: ['out.txt']
    cmds: ['cp src.txt out.txt']
"#,
    );
    block_on(async {
        let mut e = Executor::new().with_dir(&d).with_silent(true);
        e.setup().await.unwrap();
        let e = Rc::new(e);
        let calls = vec![Call::new("build")];

        // --status is informational: it succeeds but does not run the task.
        e.status(&calls, false).await.unwrap();
        e.status(&calls, true).await.unwrap();
        assert!(
            !dir.join("out.txt").exists(),
            "status must not run the task"
        );

        // After a real run it still reports (now up to date).
        e.run(&calls).await.unwrap();
        assert!(dir.join("out.txt").exists());
        e.status(&calls, false).await.unwrap();
    });
}

#[test]
fn runs_a_simple_task() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  default:
    cmds:
      - echo hello > out.txt
"#,
    );
    block_on(setup_and_run(&d, &["default"])).unwrap();
    let out = fs::read_to_string(dir.join("out.txt")).unwrap();
    assert_eq!(out.trim(), "hello");
}

#[test]
fn dependencies_run_before_the_task() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  default:
    deps: [dep]
    cmds:
      - echo main >> order.txt
  dep:
    cmds:
      - echo dep >> order.txt
"#,
    );
    block_on(setup_and_run(&d, &["default"])).unwrap();
    let order = fs::read_to_string(dir.join("order.txt")).unwrap();
    let lines: Vec<&str> = order.lines().collect();
    assert_eq!(lines, vec!["dep", "main"]);
}

#[test]
fn up_to_date_task_is_skipped_on_second_run() {
    let dir = temp_dir();
    fs::write(dir.join("src.txt"), "input").unwrap();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  build:
    sources:
      - src.txt
    generates:
      - build.txt
    cmds:
      - echo built >> runs.txt
      - cp src.txt build.txt
"#,
    );
    // First run builds and records one run.
    block_on(setup_and_run(&d, &["build"])).unwrap();
    let after_first = fs::read_to_string(dir.join("runs.txt")).unwrap();
    assert_eq!(after_first.lines().count(), 1);

    // Second run with unchanged sources is up to date: the command must not run
    // again, so the run counter stays at one. A fresh executor mirrors a second
    // CLI invocation.
    block_on(setup_and_run(&d, &["build"])).unwrap();
    let after_second = fs::read_to_string(dir.join("runs.txt")).unwrap();
    assert_eq!(
        after_second.lines().count(),
        1,
        "up-to-date task should be skipped on the second run"
    );
}

#[test]
fn changed_source_reruns_the_task() {
    let dir = temp_dir();
    fs::write(dir.join("src.txt"), "input").unwrap();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  build:
    sources:
      - src.txt
    generates:
      - build.txt
    cmds:
      - echo built >> runs.txt
      - cp src.txt build.txt
"#,
    );
    block_on(setup_and_run(&d, &["build"])).unwrap();
    // Change the source so the fingerprint no longer matches.
    fs::write(dir.join("src.txt"), "changed").unwrap();
    block_on(setup_and_run(&d, &["build"])).unwrap();
    let runs = fs::read_to_string(dir.join("runs.txt")).unwrap();
    assert_eq!(runs.lines().count(), 2, "changed source should re-run");
}

#[test]
fn dry_run_does_not_execute_commands() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  default:
    cmds:
      - echo hello > out.txt
"#,
    );
    block_on(setup_and_run_with(&d, &["default"], |e| e.with_dry(true))).unwrap();
    assert!(
        !dir.join("out.txt").exists(),
        "dry-run must not create files"
    );
}

#[test]
fn dynamic_sh_variable_is_available() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
vars:
  GREETING:
    sh: echo hi-from-sh
tasks:
  default:
    cmds:
      - echo '{{.GREETING}}' > out.txt
"#,
    );
    block_on(setup_and_run(&d, &["default"])).unwrap();
    let out = fs::read_to_string(dir.join("out.txt")).unwrap();
    assert_eq!(out.trim(), "hi-from-sh");
}

#[test]
fn unknown_task_is_reported() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  default:
    cmds:
      - echo hi
"#,
    );
    let err = block_on(setup_and_run(&d, &["nope"])).unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn cmd_calls_another_task() {
    let dir = temp_dir();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  default:
    cmds:
      - task: child
      - echo parent >> chain.txt
  child:
    cmds:
      - echo child >> chain.txt
"#,
    );
    block_on(setup_and_run(&d, &["default"])).unwrap();
    let chain = fs::read_to_string(dir.join("chain.txt")).unwrap();
    let lines: Vec<&str> = chain.lines().collect();
    assert_eq!(lines, vec!["child", "parent"]);
}

#[test]
fn export_and_import_cache_roundtrip() {
    let dir = temp_dir();
    fs::write(dir.join("src.txt"), "input").unwrap();
    let d = write_taskfile(
        &dir,
        r#"
version: '3'
tasks:
  build:
    sources:
      - src.txt
    generates:
      - build.txt
    cmds:
      - cp src.txt build.txt
"#,
    );
    // Build so the fingerprint state and generated file exist.
    block_on(setup_and_run(&d, &["build"])).unwrap();
    assert!(dir.join("build.txt").exists());

    let zip = dir.join("cache.zip");
    block_on(async {
        let mut e = Executor::new().with_dir(&d).with_silent(true);
        e.setup().await.unwrap();
        let e = Rc::new(e);
        e.export_cache(&zip, &[Call::new("build")]).await.unwrap();
    });
    assert!(zip.exists(), "export should create the archive");

    // Remove the generated file, then import to restore it.
    fs::remove_file(dir.join("build.txt")).unwrap();
    block_on(async {
        let mut e = Executor::new().with_dir(&d).with_silent(true);
        e.setup().await.unwrap();
        let e = Rc::new(e);
        e.import_cache(&zip, &[Call::new("build")]).await.unwrap();
    });
    assert!(
        dir.join("build.txt").exists(),
        "import should restore the generated file"
    );
}
