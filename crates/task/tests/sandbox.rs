//! `sandbox:` restricts a task's commands to its sources (fork addition, no Go
//! counterpart). Each backend is exercised only where the host offers it: a
//! kernel without Landlock, or a container that filters `seccomp`, skips that
//! backend's tests rather than failing them. Any other probe failure fails, as
//! does an unavailable backend with `TASK_TEST_SANDBOX_REQUIRED` set.
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
    let unavailable = [
        "Landlock is unavailable",
        "is below",
        "seccomp is unavailable",
    ]
    .iter()
    .any(|m| out.contains(m));
    assert!(
        unavailable && std::env::var_os("TASK_TEST_SANDBOX_REQUIRED").is_none(),
        "TASK_SANDBOX={mode} failed its probe: {out}"
    );
    eprintln!("skipping: TASK_SANDBOX={mode} unavailable here: {out}");
    false
}

const ENFORCING: [&str; 2] = ["landlock", "seccomp"];

/// Whether a command's `if` can run sandboxed under `mode`: under Landlock,
/// it needs the seccomp supervisor too.
fn condition_available(mode: &str) -> bool {
    available(mode) && (mode != "landlock" || available("seccomp"))
}

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
fn sources_of_a_referenced_task_are_allowed() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["sources-of-another-task"],
            &[("TASK_SANDBOX", mode)],
        );
        assert!(r.ok(), "{mode}: {}", r.combined());
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
        if mode == "landlock" {
            assert!(r.combined().contains(HINT), "{mode}: {}", r.combined());
        }
        if mode == "seccomp" {
            assert!(
                r.combined().contains("sandbox: denied read of ")
                    && r.combined().contains("secret/key"),
                "{}",
                r.combined()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn seccomp_reads_paths_through_proc_without_process_vm_readv() {
    // Only debug builds take `/proc/<pid>/mem` on this switch, as on a kernel
    // without `process_vm_readv`.
    if !cfg!(debug_assertions) || !available("seccomp") {
        return;
    }
    let env = [
        ("TASK_SANDBOX", "seccomp"),
        ("TASK_TEST_SECCOMP_PROC_MEM", "1"),
    ];
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["sources-and-outputs"], &env);
    assert!(r.ok(), "{}", r.combined());
    let r = run_with_env(&dir, &["read-outside-sources"], &env);
    assert!(!r.ok(), "expected a failure: {}", r.combined());
    let out = r.combined();
    assert!(
        out.contains("sandbox: denied read of ") && out.contains("secret/key"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a failed sandboxed command adds to its output.
const HINT: &str = "task: [read-outside-sources] ran sandboxed: a \"Permission denied\" \
                    above may be a file missing from sources";

#[test]
fn an_ignored_denial_is_not_pointed_at_the_sandbox() {
    // Under seccomp a denial fails the task even when ignored: see
    // `a_denial_never_passes_as_an_ignored_error_under_seccomp`.
    for mode in ["landlock"].into_iter().filter(|m| available(m)) {
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
            if task == "ignored-denial" {
                assert!(r.stdout.contains("second\n"), "{mode}: {out}");
            }
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
fn a_condition_denied_by_the_sandbox_fails_rather_than_skips() {
    // Under Landlock too: the condition runs under the seccomp supervisor,
    // which reports the denial instead of only making the condition false.
    for mode in ["landlock", "seccomp", "audit"]
        .into_iter()
        .filter(|m| condition_available(m))
    {
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["if-reads-outside-sources"],
            &[("TASK_SANDBOX", mode)],
        );
        let out = r.combined();
        assert!(
            out.contains("read of ") && out.contains("secret/key"),
            "{mode}: {out}"
        );
        if mode != "audit" {
            assert!(!r.ok(), "{mode}: {out}");
            assert!(
                out.contains("if condition of \"if-reads-outside-sources\" was denied"),
                "{mode}: {out}"
            );
            assert_not_recorded(&dir, "if-reads-outside-sources", mode);
        } else {
            assert!(r.ok() && out.contains("ran\n"), "{out}");
        }
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
        if mode == "seccomp" {
            assert!(
                r.combined().contains("sandbox: denied link of ")
                    && r.combined().contains("secret/key"),
                "{}",
                r.combined()
            );
        }
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
fn a_directory_holding_only_a_symlink_to_a_source_is_writable() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["create-beside-a-symlink"],
            &[("TASK_SANDBOX", mode)],
        );
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert!(dir.join("linked/pkg/.pybuild").is_dir(), "{mode}");
        assert_eq!(r.stdout, "conf\n", "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
        // Nor does it let a new link read what may not be read: a hard link
        // fails (Landlock with EXDEV), a symlink is checked on its target.
        for (task, want) in [
            (
                "hard-link-beside-a-symlink",
                "ln: failed to create hard link",
            ),
            (
                "symlink-beside-a-symlink",
                "cat: linked/pkg/config/s: Permission denied",
            ),
        ] {
            let dir = stage("sandbox");
            let r = run_with_env(&dir, &[task], &[("TASK_SANDBOX", mode)]);
            let out = r.combined();
            assert!(!r.ok(), "{mode} {task}: expected a failure: {out}");
            assert!(out.contains(want), "{mode} {task}: {out}");
            assert!(!r.stdout.contains("other"), "{mode} {task}: {out}");
            assert!(!dir.join("linked/pkg/config/h").exists(), "{mode} {task}");
            let _ = std::fs::remove_dir_all(&dir);
        }
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
fn a_nested_task_process_under_seccomp_warns_its_sandbox_is_not_enforced() {
    if !available("seccomp") {
        return;
    }
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["outer-process"], &[("TASK_SANDBOX", "seccomp")]);
    assert!(r.ok(), "{}", r.combined());
    assert_eq!(
        r.combined().matches("sandboxes are not enforced").count(),
        1,
        "{}",
        r.combined()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn audit_reports_without_denying() {
    if !available("audit") {
        return;
    }
    let dir = stage("sandbox");
    let r = run_with_env(
        &dir,
        &["read-outside-sources"],
        &[("TASK_SANDBOX", "audit")],
    );
    assert!(r.ok(), "{}", r.combined());
    let out = r.combined();
    assert!(out.contains("s\n"), "the read must go through:\n{out}");
    assert!(
        out.contains("sandbox: would deny read of ") && out.contains("secret/key"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
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
        if mode == "seccomp" {
            assert!(
                r.combined().contains("sandbox: denied create in "),
                "{}",
                r.combined()
            );
        }
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

/// Runs `task` again and checks the failed run left no up-to-date record.
fn assert_not_recorded(dir: &std::path::Path, task: &str, mode: &str) {
    let again = run_with_env(dir, &[task], &[("TASK_SANDBOX", mode)]);
    assert!(
        !again.combined().contains("up to date"),
        "{mode}: {}",
        again.combined()
    );
}

#[test]
fn a_false_condition_still_skips() {
    for mode in ENFORCING.into_iter().filter(|m| condition_available(m)) {
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
fn a_denial_the_command_swallowed_still_fails_under_seccomp() {
    if !available("seccomp") {
        return;
    }
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["swallowed-denial"], &[("TASK_SANDBOX", "seccomp")]);
    let out = r.combined();
    assert!(!r.ok(), "{out}");
    assert!(
        out.contains("a command of \"swallowed-denial\" was denied by the sandbox"),
        "{out}"
    );
    assert_not_recorded(&dir, "swallowed-denial", "seccomp");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn audit_reports_a_swallowed_denial_without_failing() {
    if !available("audit") {
        return;
    }
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["swallowed-denial"], &[("TASK_SANDBOX", "audit")]);
    let out = r.combined();
    assert!(r.ok(), "{out}");
    assert!(out.contains("sandbox: would deny read of "), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_denial_never_passes_as_an_ignored_error_under_seccomp() {
    if !available("seccomp") {
        return;
    }
    for task in [
        "ignored-denial",
        "task-ignored-denial",
        "call-ignored-denial",
        "deferred-denial",
        "deferred-call-denial",
        // Denied twice: the repeat must count too.
        "repeated-denial",
    ] {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &[task], &[("TASK_SANDBOX", "seccomp")]);
        let out = r.combined();
        assert!(!r.ok(), "{task}: {out}");
        assert!(out.contains("was denied by the sandbox"), "{task}: {out}");
        // The denial stops the task like a failure that is not ignored.
        if task == "ignored-denial" {
            assert!(!r.stdout.contains("second"), "{out}");
        }
        // The deferred command's repeat is shown only as such.
        if task == "repeated-denial" {
            assert_eq!(out.matches("denied read of ").count(), 1, "{out}");
            assert!(
                out.contains("denied again an access already reported above"),
                "{out}"
            );
        }
        assert_not_recorded(&dir, task, "seccomp");
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
fn a_pipe_opened_through_dev_fd_is_no_denial() {
    for mode in ["landlock", "seccomp", "audit"]
        .into_iter()
        .filter(|m| available(m))
    {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["process-substitution"], &[("TASK_SANDBOX", mode)]);
        let out = r.combined();
        assert!(r.ok(), "{mode}: {out}");
        assert!(
            r.stdout.contains("one\ntwo\nthree\nfour\n"),
            "{mode}: {out}"
        );
        assert!(!out.contains("sandbox:"), "{mode}: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A command making a user and mount namespace, with `python3` to open a file
/// `O_PATH`, or `None` (saying so) where either is missing.
fn unshare() -> Option<&'static str> {
    let works = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let found = ["unshare", "busybox unshare"].into_iter().find(|u| {
        let mut words = u.split(' ');
        let cmd = words.next().unwrap();
        let args: Vec<&str> = words.chain(["-Urm", "true"]).collect();
        works(cmd, &args)
    });
    if found.is_none() || !works("python3", &["-c", ""]) {
        eprintln!("skipping: no unprivileged user namespace or no python3 here");
        return None;
    }
    found
}

#[test]
fn a_file_reopened_from_another_mount_namespace_is_checked() {
    let Some(unshare) = unshare() else {
        return;
    };
    for mode in ["landlock", "seccomp", "audit"]
        .into_iter()
        .filter(|m| available(m))
    {
        let dir = stage("sandbox");
        let env = [("TASK_SANDBOX", mode), ("UNSHARE", unshare)];
        let r = run_with_env(&dir, &["other-namespace"], &env);
        let out = r.combined();
        if mode == "audit" {
            assert!(r.ok(), "{mode}: {out}");
            assert!(
                out.contains("sandbox: would deny read of ") && out.contains("secret/key"),
                "{mode}: {out}"
            );
        } else {
            assert!(!r.ok(), "{mode}: expected a failure: {out}");
            assert!(!r.stdout.contains("read:"), "{mode}: {out}");
        }
        if mode == "seccomp" {
            assert!(
                out.contains("sandbox: denied read of ") && out.contains("secret/key"),
                "{mode}: {out}"
            );
        }
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

/// Asserts `task`'s `mkdir` was denied creating an entry in `parent`. Whether
/// `mkdir` names the path depends on which one runs (the shell's builtin or
/// coreutils); the seccomp supervisor names the directory.
fn assert_denied_create(r: &Run, mode: &str, task: &str, parent: &std::path::Path) {
    let out = r.combined();
    assert!(
        out.lines()
            .any(|l| l.starts_with("mkdir: ") && l.ends_with("Permission denied")),
        "{mode}: {out}"
    );
    if mode == "seccomp" {
        let want = format!(
            "task: [{task}] sandbox: denied create in {} (not allowed",
            parent.display()
        );
        assert!(out.contains(&want), "{mode}: {out}");
    } else {
        let want = format!("task: [{task}] ran sandboxed");
        assert!(out.contains(&want), "{mode}: {out}");
    }
}

#[test]
fn writes_naming_a_task_make_its_outputs_writable() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        // Denied without the entry: the callee cannot create gen/.
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["calls-gen"], &[("TASK_SANDBOX", mode)]);
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        assert_denied_create(&r, mode, "gen", &std::fs::canonicalize(&dir).unwrap());
        assert!(!dir.join("gen").exists(), "{mode}");
        let _ = std::fs::remove_dir_all(&dir);
        // A direct call, a sandboxed callee, a nested task process, and a
        // reference through the callee's own generates.
        for (task, out) in [
            ("calls-gen-writes", "gen/out/a.txt"),
            ("calls-sandboxed-gen-writes", "sbx-gen/a.txt"),
            ("nested-writes", "out/nested.txt"),
            ("calls-gen-transitive-writes", "gen/out/a.txt"),
        ] {
            let dir = stage("sandbox");
            let r = run_with_env(&dir, &[task], &[("TASK_SANDBOX", mode)]);
            assert!(r.ok(), "{mode} {task}: {}", r.combined());
            assert!(r.stdout.ends_with("a\n"), "{mode} {task}: {}", r.combined());
            assert_eq!(std::fs::read_to_string(dir.join(out)).unwrap(), "a\n");
            // Under seccomp, a nested task process warns its own sandbox is
            // not enforced.
            assert!(
                (mode, task) == ("seccomp", "nested-writes") || !r.combined().contains("sandbox:"),
                "{mode} {task}: {}",
                r.combined()
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[test]
fn writes_naming_a_task_follow_its_vars() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["calls-gen-lang-writes"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
        // Granting another directory than the one written in.
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["calls-gen-lang-writes", "WRITES_LANG=de"],
            &[("TASK_SANDBOX", mode)],
        );
        assert!(!r.ok(), "{mode}: expected a failure: {}", r.combined());
        let mixed = std::fs::canonicalize(&dir).unwrap().join("mixed");
        assert_denied_create(&r, mode, "gen-lang", &mixed);
        assert!(dir.join("mixed/de").is_dir(), "{mode}");
        assert!(!dir.join("mixed/fr").exists(), "{mode}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn writes_naming_a_task_do_not_warn_about_a_directory_holding_sources() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["writes-next-to-sources"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert_eq!(
            std::fs::read_to_string(dir.join("src/gen.txt")).unwrap(),
            "gen\n"
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
fn writes_naming_a_bad_task_fail() {
    // Resolved on compiling, sandboxing on or off.
    for (task, code, want) in [
        (
            "writes-unknown-task",
            200,
            "task: writes-unknown-task: sandbox.writes: task \"no-such-task\" does not exist",
        ),
        (
            "writes-empty-task-name",
            1,
            "task: writes-empty-task-name: sandbox.writes: `task: {{.UNSET}}` rendered to an \
             empty task name",
        ),
        (
            "writes-cycle",
            204,
            "task: Cyclic dependency detected: writes-cycle-gen -> writes-cycle-wrapper -> \
             writes-cycle-gen",
        ),
    ] {
        for mode in ["off", "landlock"] {
            let dir = stage("sandbox");
            let r = run_with_env(&dir, &[task], &[("TASK_SANDBOX", mode)]);
            assert!(!r.ok(), "{task}: expected a failure: {}", r.combined());
            assert_eq!(r.code, code, "{task}: {}", r.combined());
            assert!(r.stderr.contains(want), "{task}: {}", r.combined());
            assert!(!r.stdout.contains("never"), "{task}: {}", r.combined());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[test]
fn writes_naming_a_task_leave_the_fingerprint_alone() {
    let dir = stage("sandbox");
    let r = run_with_env(&dir, &["fingerprinted"], &[("TASK_SANDBOX", "off")]);
    assert!(r.ok(), "{}", r.combined());
    let checksum = checksum_file(&dir, "fingerprinted").unwrap();
    let before = std::fs::read_to_string(&checksum).unwrap();
    let taskfile = dir.join("Taskfile.yml");
    let yaml = std::fs::read_to_string(&taskfile).unwrap();
    let declared = "    generates: [fp/a.txt]\n    sandbox: true\n";
    assert!(yaml.contains(declared));
    // The referenced task's own fields do not count either.
    let gen_fields = "  gen:\n    sources: [src/**]\n    generates: [gen/out/*.txt]\n";
    assert!(yaml.contains(gen_fields));
    std::fs::write(
        &taskfile,
        yaml.replace(
            declared,
            "    generates: [fp/a.txt]\n    sandbox:\n      writes: [{task: gen}]\n",
        )
        .replace(
            gen_fields,
            "  gen:\n    sources: [src/**, secret/**]\n    generates: [gen/other/*.txt]\n",
        ),
    )
    .unwrap();
    let r = run_with_env(
        &dir,
        &["--status", "fingerprinted"],
        &[("TASK_SANDBOX", "off")],
    );
    assert!(r.ok(), "{}", r.combined());
    assert!(
        r.stdout
            .starts_with("task: \"fingerprinted\" is up to date\n"),
        "{}",
        r.combined()
    );
    assert_eq!(checksum_file(&dir, "fingerprinted").unwrap(), checksum);
    assert_eq!(std::fs::read_to_string(&checksum).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writes_naming_the_task_itself_grant_its_own_generates() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["writes-itself"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert_eq!(
            std::fs::read_to_string(dir.join("self/a.txt")).unwrap(),
            "a\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn writes_naming_a_task_without_generates_warn() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(&dir, &["writes-nothing"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert!(
            r.stderr.contains(
                "sandbox: \"writes-nothing\": sandbox.writes {task: probe} grants nothing: \
                 \"probe\" has no generates"
            ),
            "{mode}: {}",
            r.combined()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn generates_outside_the_root_are_left_alone() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("sandbox");
        let r = run_with_env(
            &dir,
            &["generates-outside-the-root"],
            &[("TASK_SANDBOX", mode)],
        );
        assert!(r.ok(), "{mode}: {}", r.combined());
        assert!(r.stdout.contains("ran\n"), "{mode}: {}", r.combined());
        assert!(
            !r.combined().contains("sandbox:"),
            "{mode}: {}",
            r.combined()
        );
        assert!(!std::path::Path::new("/opt/task-sandbox-test").exists());
        assert!(!std::path::Path::new("/proc/sys/task-sandbox-test").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_file_another_folded_task_excludes_is_readable() {
    for mode in ENFORCING.into_iter().filter(|m| available(m)) {
        let dir = stage("ref_follows_runs/exclusions");
        let r = run_with_env(&dir, &["sandboxed"], &[("TASK_SANDBOX", mode)]);
        assert!(r.ok(), "{mode}: {}", r.combined());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
