//! `sandbox:` restricts a task's commands to its sources (fork addition, no Go
//! counterpart). Landlock is exercised only where the host offers it: a kernel
//! without it skips these tests rather than failing them. Any other probe
//! failure fails, as does an unavailable Landlock with
//! `TASK_TEST_SANDBOX_REQUIRED` set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::*;

/// Whether `mode` can sandbox a command on this host: one that reads a source
/// with an external program.
fn available(mode: &str) -> bool {
    let dir = stage("sandbox");
    let probe = run_with_env(&dir, &["probe"], &[("TASK_SANDBOX", mode)]);
    let _ = std::fs::remove_dir_all(&dir);
    if probe.ok() {
        return true;
    }
    let out = probe.combined();
    let unavailable = ["Landlock is unavailable", "is below"]
        .iter()
        .any(|m| out.contains(m));
    assert!(
        unavailable && std::env::var_os("TASK_TEST_SANDBOX_REQUIRED").is_none(),
        "TASK_SANDBOX={mode} failed its probe: {out}"
    );
    eprintln!("skipping: TASK_SANDBOX={mode} unavailable here: {out}");
    false
}

const ENFORCING: [&str; 1] = ["landlock"];

#[test]
fn sources_and_outputs_are_allowed() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["sources-and-outputs"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert_eq!(
            std::fs::read_to_string(dir.join("out/a.txt")).unwrap(),
            "a\n"
        );
        assert!(
            !r.combined().contains("sandbox:"),
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_fingerprinted_glob_allows_every_file_it_matches() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        std::fs::write(dir.join("src/b.txt"), "b\n").unwrap();
        let r = run_with_env(&dir, &["fingerprinted-sources"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn reading_outside_sources_fails() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["read-outside-sources"], &[("TASK_SANDBOX", mode)]);
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        assert!(
            r.combined().contains("Permission denied"),
            "{mode}: {}",
            r.combined()
        );
        assert!(r.combined().contains(HINT), "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// What a failed sandboxed command adds to its output.
const HINT: &str = "task: [read-outside-sources] ran sandboxed: a \"Permission denied\" \
                    above may be a file missing from sources";

#[test]
fn an_ignored_denial_is_not_pointed_at_the_sandbox() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        // A command's and a task's `ignore_error`, a deferred command, and
        // the same for a `task:` call.
        for task in [
            "ignored-denial",
            "task-ignored-denial",
            "deferred-denial",
            "call-ignored-denial",
            "deferred-call-denial",
        ] {
            let dir = stage("sandbox");
            let r = run_with_env(&dir, &[task], &[("TASK_SANDBOX", mode)]);
            let out = r.combined();
            assert!(r.ok(), "{mode} {task}: {out}");
            assert!(out.contains("Permission denied"), "{mode} {task}: {out}");
            assert!(!out.contains("ran sandboxed"), "{mode} {task}: {out}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[test]
fn landlock_mode_runs_sandboxed() {
    if !available("landlock") {
        return;
    }
    let dir = stage("sandbox");
    let r = run_with_env(
        &dir,
        &["--verbose", "probe"],
        &[("TASK_SANDBOX", "landlock")],
    );
    assert!(r.ok(), "{}", r.combined());
    assert!(
        r.combined().contains("task: [probe] sandboxed (Landlock("),
        "{}",
        r.combined()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_nested_task_process_runs_in_a_sandbox_of_its_own() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["nested"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert_eq!(
            std::fs::read_to_string(dir.join("out/nested.txt")).unwrap(),
            "a\n"
        );
        // A directory the inner task needs, which the outer rules keep it
        // from creating, is named.
        std::fs::remove_dir_all(dir.join("out")).unwrap();
        let r = run_with_env(&dir, &["nested-missing-dir"], &[("TASK_SANDBOX", mode)]);
        let out = std::fs::canonicalize(&dir).unwrap().join("out");
        let want = format!(
            "setting up the sandbox of \"nested-inner\": creating {}: ",
            out.display()
        );
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        assert!(r.combined().contains(&want), "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_called_tasks_deps_run_in_the_callers_sandbox() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["calls-task-with-dep"], &[("TASK_SANDBOX", mode)]);
        let out = r.combined();
        assert!(!r.ok(), "{mode}: expected a failure: {out}");
        assert!(out.contains("Permission denied"), "{mode}: {out}");
        assert!(!r.stdout.contains("called\n"), "{mode}: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_generates_directory_holding_sources_is_warned_about() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["generates-in-sources"], &[("TASK_SANDBOX", mode)]);
        let out = r.combined();
        assert!(r.ok(), "{mode}: {out}");
        assert!(
            out.contains("generates \"src/a.o\" makes") && out.contains("holds sources too"),
            "{mode}: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn an_excluded_read_is_denied() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        std::fs::write(dir.join("secret/other"), "other\n").unwrap();
        let r = run_with_env(&dir, &["reads-with-exclude"], &[("TASK_SANDBOX", mode)]);
        let out = r.combined();
        assert!(!r.ok(), "{mode}: expected a failure: {out}");
        assert!(r.stdout.contains("other\n"), "{mode}: {out}");
        assert!(
            out.contains("secret/key: Permission denied"),
            "{mode}: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn hard_linking_a_file_out_of_sources_fails() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["link-out-of-sources"], &[("TASK_SANDBOX", mode)]);
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        assert!(!dir.join("out/key").exists(), "{mode}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn creating_in_a_directory_with_other_files_fails() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["create-in-mixed-directory"],
            &[("TASK_SANDBOX", mode)],
        );
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        assert!(!dir.join("mixed/new").exists(), "{mode}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn reads_and_writes_widen_the_sandbox() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["reads-and-writes"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert!(dir.join("mixed/cache/x").exists(), "{mode}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn called_tasks_run_in_the_callers_sandbox_but_deps_do_not() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let called = run_with_env(&dir, &["outer"], &[("TASK_SANDBOX", mode)]);
        assert!(
            !called.ok(),
            "{mode}: expected a failure: {}",
            called.combined()
        );
        let dep = run_with_env(&dir, &["with-unsandboxed-dep"], &[("TASK_SANDBOX", mode)]);
        assert!(dep.ok(), "{mode}: {}", dep.combined());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn off_ignores_the_sandbox() {
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["read-outside-sources"], &[("TASK_SANDBOX", "off")]);
    assert!(r.ok(), "{}", r.combined());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_mode_is_rejected() {
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["probe"], &[("TASK_SANDBOX", "on")]);
    assert!(!r.ok());
    assert!(
        r.combined().contains("TASK_SANDBOX=\"on\""),
        "{}",
        r.combined()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ancestors_of_the_root_can_be_listed_but_not_changed() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["list-ancestors"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        let r = run_with_env(&dir, &["create-in-ancestor"], &[("TASK_SANDBOX", mode)]);
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        let mut name = dir.file_name().unwrap().to_os_string();
        name.push(".escaped");
        let escaped = dir.parent().unwrap().join(name);
        let leaked = escaped.exists();
        let _ = std::fs::remove_file(&escaped);
        assert!(!leaked, "{mode}: created {}", escaped.display());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_generated_file_at_the_root_is_not_writable_and_says_why() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["generates-at-root"], &[("TASK_SANDBOX", mode)]);
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        assert!(
            r.combined().contains("put the outputs in a subdirectory"),
            "{mode}: {}",
            r.combined()
        );
        assert!(!dir.join("app").exists(), "{mode}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn writes_at_or_above_the_root_are_ignored_with_a_warning() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["writes-at-root"], &[("TASK_SANDBOX", mode)]);
        assert!(
            !r.ok(),
            "{mode}: the root must stay restricted: {}",
            r.combined()
        );
        assert_eq!(
            r.combined()
                .matches("is the root directory or above it")
                .count(),
            2,
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_root_under_the_temporary_directory_gets_a_private_tmpdir() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["tmpdir"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        let tmp = std::fs::canonicalize(&dir).unwrap().join(".task/tmp");
        // A directory of the run's own, not the Taskfile's TMPDIR.
        assert!(
            r.stdout.contains(&format!("tmp={}/", tmp.display())),
            "{mode}: {}",
            r.combined()
        );
        assert!(r.stdout.contains("x\n"), "{mode}: {}", r.combined());
        let left: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(left.is_empty(), "{mode}: left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_system_temporary_directory_inside_the_root_is_replaced_too() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let system_tmp = dir.join("systmp");
        std::fs::create_dir(&system_tmp).unwrap();
        let r = run_with_env(
            &dir,
            &["tmpdir"],
            &[
                ("TASK_SANDBOX", mode),
                ("TMPDIR", system_tmp.to_str().unwrap()),
            ],
        );
        assert!(r.ok(), "{mode}: {}", r.combined());
        let tmp = std::fs::canonicalize(&dir).unwrap().join(".task/tmp");
        assert!(
            r.stdout.contains(&format!("tmp={}/", tmp.display())),
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_declared_dotenv_file_is_readable() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["dotenv"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert!(
            r.stdout.contains("SANDBOX_VAR=from-dotenv"),
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_false_condition_still_skips() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["if-false"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert!(!r.stdout.contains("ran\n"), "{mode}: {}", r.combined());
        assert!(
            !r.combined().contains("sandbox:"),
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn what_the_kernel_refuses_first_is_no_denial() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["nofollow"], &[("TASK_SANDBOX", mode)]);
        let out = r.combined();
        assert!(r.ok(), "{mode}: {out}");
        assert!(
            r.stdout.contains("refused\n")
                && r.stdout.contains("notdir\n")
                && r.stdout.contains("moved\n"),
            "{mode}: {out}"
        );
        assert!(!out.contains("sandbox:"), "{mode}: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_condition_runs_unsandboxed() {
    // Like a task-level `if`: under Landlock a denial would only make the
    // condition false, skipping the command as if it did not apply.
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["if-reads-outside-sources"],
            &[("TASK_SANDBOX", mode)],
        );
        assert!(
            r.ok() && r.stdout.contains("ran\n"),
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_called_sandboxed_task_runs_within_both_sandboxes() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["outer2"], &[("TASK_SANDBOX", mode)]);
        let out = r.combined();
        assert!(!r.ok(), "{mode}: expected a failure: {out}");
        assert!(r.stdout.contains("a\n"), "{mode}: {out}");
        assert!(
            out.contains("secret/key: Permission denied"),
            "{mode}: {out}"
        );
        // Alone, the inner task may read it.
        let r = run_with_env(&dir, &["inner2"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
