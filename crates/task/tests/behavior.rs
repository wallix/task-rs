//! Behavioral parity tests ported from Go `task_test.go` (core execution
//! behaviors). See `common` for the harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::{empty_case_dir, run, run_with_env, stage};
#[cfg(unix)]
use std::path::Path;

// Ports Go `TestDry`.
#[test]
fn dry_run_prints_command_without_executing() {
    let dir = stage("dry");
    let _ = std::fs::remove_file(dir.join("file.txt"));
    let o = run(&dir, &["--dry", "build"]);
    assert!(o.ok(), "dry run failed: {}", o.combined());
    assert_eq!(o.combined().trim(), "task: [build] touch file.txt");
    assert!(
        !dir.join("file.txt").exists(),
        "dry run must not create the file"
    );
}

// Ports Go `TestCyclicDep`.
#[test]
fn cyclic_dependency_errors() {
    let dir = stage("cyclic");
    let o = run(&dir, &["task-1"]);
    assert!(!o.ok(), "a cyclic dependency must fail: {}", o.combined());
    // Failing is not enough: a stack overflow aborts the process, which `code`
    // reports as -1 and `ok()` accepts as a failure. The cycle has to be
    // reported, and named, by the task that detected it.
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    assert!(
        o.combined()
            .contains("Cyclic dependency detected: task-1 -> task-2 -> task-1"),
        "expected the cycle to be named, got: {}",
        o.combined()
    );
}

// Above the ~400 a release build tolerated before dependencies were queued on
// the runtime (a debug build managed ~30), so the depth guards the fix in either
// profile. Kept just past that threshold on purpose: each of the two chain
// tests writes and runs a Taskfile this long, and the cost is linear in it.
const CHAIN_DEPTH: usize = 600;

// Writes a Taskfile whose tasks form a `CHAIN_DEPTH`-long chain, linked either
// through `deps:` or through a nested `task:` command — the two call sites that
// queue a task on the runtime.
fn write_chain(dir: &std::path::Path, nested: bool) {
    let mut taskfile = String::from("version: '3'\n\ntasks:\n");
    for i in 1..=CHAIN_DEPTH {
        taskfile.push_str(&format!("  t{i}:\n"));
        if i == CHAIN_DEPTH {
            taskfile.push_str("    cmds: [\"true\"]\n");
        } else if nested {
            taskfile.push_str(&format!("    cmds:\n      - task: t{}\n", i + 1));
        } else {
            taskfile.push_str(&format!("    deps: [t{}]\n    cmds: [\"true\"]\n", i + 1));
        }
    }
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
}

// A chain of dependencies used to nest one future per level and abort the
// process; queued on the runtime, depth costs no stack.
#[test]
fn deep_dependency_chain_does_not_overflow_the_stack() {
    let dir = empty_case_dir("deep_deps");
    write_chain(&dir, false);

    let o = run(&dir, &["t1"]);
    // Removed before asserting, so a failure does not leave the tree behind.
    let _ = std::fs::remove_dir_all(&dir);
    // A stack overflow aborts, which the harness reports as -1.
    assert!(
        o.ok(),
        "a {CHAIN_DEPTH}-deep dependency chain should run, got code {}: {}",
        o.code,
        o.combined()
    );
}

// The same depth reached through nested `task:` commands rather than `deps:`.
#[test]
fn deep_nested_task_chain_does_not_overflow_the_stack() {
    let dir = empty_case_dir("deep_cmds");
    write_chain(&dir, true);

    let o = run(&dir, &["t1"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        o.ok(),
        "a {CHAIN_DEPTH}-deep chain of `task:` commands should run, got code {}: {}",
        o.code,
        o.combined()
    );
}

// Cancelling a task abandons the command it was running, and the shell
// interpreter gives no handle to stop it, so the engine sweeps the process tree
// once a run is torn down. Without that the command runs on past the run.
#[test]
fn a_cancelled_run_stops_the_command_it_abandoned() {
    let dir = stage("stop_on_cancel");
    let o = run(&dir, &["default"]);
    assert!(
        !o.ok(),
        "the failing dep should fail the run: {}",
        o.combined()
    );

    // Positive control: without proof the subshell ran at all, an absent marker
    // proves nothing about what stopped it. Polled rather than checked once, so
    // a slow spawn cannot turn a correct sweep into a red test.
    let started = dir.join("started.txt");
    for _ in 0..50 {
        if started.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        started.exists(),
        "the command never started, so the missing marker proves nothing: {}",
        o.combined()
    );
    // Long enough for the abandoned subshell to have finished its sleep and
    // written the marker, had anything still been running it.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(
        !dir.join("marker.txt").exists(),
        "the abandoned command kept running: {}",
        o.combined()
    );
}

// Three interrupts force a shutdown: the first two are only reported, and the
// third stops the commands and exits, whatever the run was about to report.
//
// The command ignores `SIGINT` on purpose. One that takes it is killed by the
// first — a terminal delivers it to the whole group — and the run then fails on
// the command's status, with nothing left for the third to force out.
#[cfg(unix)]
#[test]
fn the_third_interrupt_forces_shutdown_and_stops_the_commands() {
    let dir = stage("stop_on_cancel");
    let child = spawn_in_own_group(&dir, "stubborn");
    wait_for(&dir.join("started.txt"));

    // Delivered to the whole group, as a terminal's Ctrl-C is.
    for _ in 0..3 {
        signal_group(&child, "INT");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let o = child.wait_with_output().expect("wait for task");
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();

    // Named as Task v3 names it, which is why the signal is quoted.
    assert_eq!(
        out.matches("Signal received: \"interrupt\"\n").count(),
        2,
        "the first two signals are only reported, got: {out}{err}"
    );
    assert!(
        err.contains("Forcing shutdown"),
        "the third forces shutdown, got: {out}{err}"
    );
    // The run's own error must not win the race with the forced exit.
    assert_eq!(
        o.status.code(),
        Some(1),
        "a forced shutdown exits 1, got: {out}{err}"
    );

    // Long enough for the command to have written its marker, had it been left
    // running.
    std::thread::sleep(std::time::Duration::from_millis(3500));
    assert!(
        !dir.join("marker.txt").exists(),
        "the forced shutdown must stop the command: {out}{err}"
    );
}

// A prompt waits on the terminal, which used to wait on the runtime thread: the
// engine was not polled while it did, and every signal arriving at a prompt was
// lost. The read happens off that thread now, so the escalation still runs.
#[cfg(unix)]
#[test]
fn signals_are_handled_while_a_prompt_waits() {
    let dir = stage("stop_on_cancel");
    // stdin is a pipe nothing is ever written to, so the prompt waits for good.
    let child = spawn_in_own_group_with_stdin(&dir, "guarded");
    // The prompt is asked once its dependency has run, so this is the run
    // sitting at the prompt rather than a guess at how long getting there takes.
    wait_for(&dir.join("at_prompt.txt"));

    for _ in 0..3 {
        signal_group(&child, "INT");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let o = child.wait_with_output().expect("wait for task");
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();

    assert_eq!(
        out.matches("Signal received: \"interrupt\"\n").count(),
        2,
        "the signals must reach the handler, got: {out}{err}"
    );
    assert!(
        err.contains("Forcing shutdown"),
        "the third forces shutdown, got: {out}{err}"
    );
    // Without this the test would pass on a run stalled anywhere at all.
    assert!(
        err.contains("really?"),
        "the run must have been at its prompt, got: {out}{err}"
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "a forced shutdown exits 1, got: {out}{err}"
    );
    assert!(
        !dir.join("marker.txt").exists(),
        "the guarded command must not have run: {out}{err}"
    );
}

// One signal is reported and nothing else: the run continues to its own end.
// This is the documented cost of the escalation — a single `SIGTERM`, which is
// what a supervisor sends before its `SIGKILL`, no longer stops `task`.
//
// Sent to the pid rather than the group, as a supervisor sends it: the command
// never sees it, so what the run does next is the engine's decision alone.
#[cfg(unix)]
#[test]
fn one_signal_is_only_reported() {
    let dir = stage("stop_on_cancel");
    let child = spawn_in_own_group(&dir, "stubborn");
    wait_for(&dir.join("started.txt"));

    signal_pid(&child, "TERM");
    let o = child.wait_with_output().expect("wait for task");
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr).into_owned();

    assert_eq!(
        out.matches("Signal received: \"terminated\"\n").count(),
        1,
        "expected one report, got: {out}{err}"
    );
    assert_eq!(o.status.code(), Some(0), "the run finishes: {out}{err}");
    assert!(
        dir.join("marker.txt").exists(),
        "the command ran to its end: {out}{err}"
    );
}

// Spawns the binary as its own process-group leader, so signalling that group
// cannot reach the test harness.
#[cfg(unix)]
fn spawn_in_own_group(dir: &Path, task: &str) -> std::process::Child {
    spawn_signalable(dir, task, std::process::Stdio::null())
}

// The same, with stdin a pipe nothing is written to, so a prompt waits for good.
#[cfg(unix)]
fn spawn_in_own_group_with_stdin(dir: &Path, task: &str) -> std::process::Child {
    spawn_signalable(dir, task, std::process::Stdio::piped())
}

#[cfg(unix)]
fn spawn_signalable(dir: &Path, task: &str, stdin: std::process::Stdio) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(common::BIN)
        .arg(task)
        .current_dir(dir)
        .env("TASK_NO_GO_DEPRECATION", "1")
        .stdin(stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn task binary")
}

// Sends `signal` to the child's whole process group, as a terminal would.
#[cfg(unix)]
fn signal_group(child: &std::process::Child, signal: &str) {
    // `-s` and `--`: the negative pid is the process group, not an option.
    let status = std::process::Command::new("kill")
        .arg("-s")
        .arg(signal)
        .arg("--")
        .arg(format!("-{}", child.id()))
        .status()
        .expect("run kill");
    assert!(status.success(), "could not signal the task process group");
}

// Sends `signal` to the task process alone, as a supervisor does.
#[cfg(unix)]
fn signal_pid(child: &std::process::Child, signal: &str) {
    let status = std::process::Command::new("kill")
        .arg("-s")
        .arg(signal)
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "could not signal the task process");
}

// Waits for a file the running command creates, so a signal cannot arrive before
// there is a command to receive it.
#[cfg(unix)]
fn wait_for(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("{} never appeared", path.display());
}

// The headline mechanism: from the second signal on, the signal is passed to
// the commands. `stubborn` ignores `SIGINT`, so this sends `SIGTERM` by pid —
// the shape where the terminal delivered nothing to the command itself — and
// the command has to die of the relayed signal rather than run to its marker.
#[cfg(unix)]
#[test]
fn a_second_signal_is_passed_to_the_commands() {
    let dir = stage("stop_on_cancel");
    let child = spawn_in_own_group(&dir, "stubborn");
    wait_for(&dir.join("started.txt"));

    // Twice: the first is only reported, the second is relayed.
    for _ in 0..2 {
        signal_pid(&child, "TERM");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let o = child.wait_with_output().expect("wait for task");
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr).into_owned();

    assert_eq!(
        out.matches("Signal received: \"terminated\"\n").count(),
        2,
        "both signals are reported, got: {out}{err}"
    );
    // Its sleep is 3s; the relay must have cut it short well before that.
    assert!(
        !dir.join("marker.txt").exists(),
        "the relayed signal did not reach the command: {out}{err}"
    );
}

// A task's own `watch: true` enters the same loop `--watch` does, so the
// interrupt handler has to be skipped for it as well. Installing it there
// replaced the default disposition of both signals with a handler the blocked
// runtime never polls, leaving the process answering only to `SIGKILL`.
#[cfg(unix)]
#[test]
fn a_watch_task_still_dies_on_sigterm() {
    use std::os::unix::process::ExitStatusExt;

    let dir = stage("watch_signal");
    let mut child = spawn_in_own_group(&dir, "watched");
    // Let the watch loop reach its blocking receive.
    std::thread::sleep(std::time::Duration::from_millis(750));
    signal_pid(&child, "TERM");

    // Waited with a deadline rather than `wait_with_output`: the bug this
    // guards leaves the process deaf to the signal, and a hang is a worse
    // failure than a red assertion.
    let mut status = None;
    for _ in 0..100 {
        match child.try_wait().expect("poll task binary") {
            Some(s) => {
                status = Some(s);
                break;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("a watch task ignored SIGTERM");
    };
    // Terminated by that signal specifically — `code() == None` alone would
    // also accept the `SIGKILL` a stuck process eventually needs.
    assert_eq!(
        status.signal(),
        Some(15),
        "a watch task must die of SIGTERM, got {status:?}"
    );
}

// The sweep stops a process a task left running on purpose just as readily, so
// `TASK_NO_REAP` turns it off and restores Go's behavior of leaving everything
// running.
#[test]
fn task_no_reap_leaves_the_abandoned_command_running() {
    let dir = stage("stop_on_cancel");
    let o = run_with_env(&dir, &["default"], &[("TASK_NO_REAP", "1")]);
    assert!(
        !o.ok(),
        "the failing dep should still fail the run: {}",
        o.combined()
    );

    // The same wait as the sweeping case, where the marker never appears.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(
        dir.join("marker.txt").exists(),
        "the abandoned command should have been left running: {}",
        o.combined()
    );
}

// The other direction: a run that succeeds leaves a job a task started
// deliberately alone — even when it abandoned a task along the way.
//
// A deferred command's error is ignored, so deferring onto a `failfast:` task
// abandons its siblings while the run still returns success. Sweeping on
// "something was abandoned" alone stopped the deliberate job here; the sweep
// needs the run to be failing too.
#[test]
fn a_completed_run_leaves_a_deliberate_job_running() {
    let dir = stage("stop_on_cancel");
    let o = run(&dir, &["no_sweep"]);
    assert!(
        o.ok(),
        "a deferred failure must not fail the run: {}",
        o.combined()
    );
    // The job the run left behind on purpose has to write its marker.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(
        dir.join("survived.txt").exists(),
        "a completed run must leave a deliberate job running: {}",
        o.combined()
    );
}

// Failfast abandons the surviving siblings and waits for those aborts before the
// error propagates, so they never reach the `echo` a full run prints.
#[test]
fn failfast_aborts_siblings_before_returning() {
    let dir = stage("failfast/default");
    let o = run(&dir, &["--failfast", "default"]);
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    assert!(
        o.stdout.trim().is_empty(),
        "the aborted siblings should print nothing, got: {:?}",
        o.stdout
    );

    // Without it, the same fixture runs every sibling to completion.
    let dir = stage("failfast/default");
    let o = run(&dir, &["default"]);
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    for dep in ["dep1", "dep2", "dep3"] {
        assert!(
            o.stdout.contains(dep),
            "{dep} should have finished without failfast, got: {:?}",
            o.stdout
        );
    }
}

// The task-level `failfast: true` takes the same path as the flag.
#[test]
fn task_level_failfast_aborts_siblings() {
    let dir = stage("failfast/task");
    let o = run(&dir, &["default"]);
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    assert!(
        o.stdout.trim().is_empty(),
        "the aborted siblings should print nothing, got: {:?}",
        o.stdout
    );
}

// A wildcard task reaching another instance of its own pattern is two tasks, not
// a repeat: the path is keyed by the resolved name, not the pattern.
#[test]
fn wildcard_instances_on_one_path_are_not_cyclic() {
    let dir = stage("wildcard_not_cyclic");
    let o = run(&dir, &["x-2"]);
    assert!(o.ok(), "x-2 -> x-1 is not a cycle: {}", o.combined());
    assert!(
        o.combined().contains("x 1") && o.combined().contains("x 2"),
        "both instances should run: {}",
        o.combined()
    );
    // The other direction: one instance calling itself does repeat.
    let o = run(&dir, &["y-7"]);
    assert!(!o.ok(), "y-7 calling itself is a cycle: {}", o.combined());
    assert!(
        o.combined()
            .contains("Cyclic dependency detected: y-7 -> y-7"),
        "expected the resolved name in the cycle, got: {}",
        o.combined()
    );
}

// A self-call with nothing changed between the turns is a cycle, and is named
// as one. Before the check existed this shape did not run either: it deadlocked
// on its own task lock, or exhausted the stack when there was no lock to take.
#[test]
fn self_call_with_the_same_body_is_cyclic() {
    let dir = stage("self_call_same_body");
    let o = run(&dir, &["touchy"]);
    assert!(!o.ok(), "expected a cycle error: {}", o.combined());
    assert!(
        o.combined()
            .contains("Cyclic dependency detected: touchy -> touchy"),
        "expected the cycle to be named, got: {}",
        o.combined()
    );
}

// The reported path is ordered outermost first. A two-task cycle reads the same
// in both directions, so only three deep pins the order through the binary.
#[test]
fn cycle_is_reported_outermost_first() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["c-1"]);
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    assert!(
        o.combined()
            .contains("Cyclic dependency detected: c-1 -> c-2 -> c-3 -> c-1"),
        "expected the whole path, outermost first, got: {}",
        o.combined()
    );
}

// A cycle closed through `setup:` rather than through a command: a distinct
// code path to the one the `task:` command takes.
#[test]
fn cycle_through_setup_is_detected() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["s-1"]);
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    assert!(
        o.combined()
            .contains("Cyclic dependency detected: s-1 -> s-2 -> s-1"),
        "expected the cycle to be named, got: {}",
        o.combined()
    );
}

// A cycle that leaves the root Taskfile and comes back through an include: the
// path is keyed by the namespaced name, which is what closes it.
#[test]
fn cycle_across_included_taskfiles_is_detected() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["i-1"]);
    assert_eq!(o.code, 201, "expected a task failure: {}", o.combined());
    assert!(
        o.combined()
            .contains("Cyclic dependency detected: i-1 -> sub:i-2 -> i-1"),
        "expected the namespaced cycle, got: {}",
        o.combined()
    );
}

// A task reached twice on separate paths — a diamond, a shared setup — is not a
// repeat on any one path, so it still runs.
#[test]
fn diamond_with_a_shared_setup_is_not_cyclic() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["top"]);
    assert!(o.ok(), "a diamond is not a cycle: {}", o.combined());
    for task in ["base", "shared", "left", "right", "top"] {
        assert!(
            o.combined().contains(&format!("[{task}]")),
            "{task} should have run: {}",
            o.combined()
        );
    }
}

// Two tasks sharing a short name in different namespaces are distinct
// identities: one calling the other is a chain, not a repeat.
#[test]
fn same_task_name_in_two_namespaces_is_not_cyclic() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["names"]);
    assert!(
        o.ok(),
        "one:build -> two:build is not a cycle: {}",
        o.combined()
    );
    assert!(
        o.combined().contains("one build") && o.combined().contains("two build"),
        "both tasks should run: {}",
        o.combined()
    );
}

// Ports Go `TestInternalTask`.
#[test]
fn internal_tasks_run_indirectly_but_not_directly() {
    let dir = stage("internal_task");
    assert_eq!(run(&dir, &["--silent", "task-1"]).stdout, "Hello, World!\n");
    assert_eq!(run(&dir, &["--silent", "task-2"]).stdout, "Hello, World!\n");
    // task-3 is internal and cannot be called directly.
    assert!(!run(&dir, &["--silent", "task-3"]).ok());
}

// Ports Go `TestGenerates`.
#[test]
fn generates_creates_files_and_reports_up_to_date_on_rerun() {
    let dir = stage("generates");
    for task in ["rel.txt", "abs.txt", "my text file.txt"] {
        let first = run(&dir, &[task]);
        assert!(first.ok(), "{task}: {}", first.combined());
        assert!(dir.join("sub/src.txt").exists(), "source should exist");
        assert!(dir.join(task).exists(), "dest {task:?} should exist");
        assert!(
            !first.combined().contains("up to date"),
            "{task:?} should not be up to date on first run"
        );

        let second = run(&dir, &[task]);
        assert!(
            second.combined().contains("up to date"),
            "{task:?} should be up to date on rerun: {}",
            second.combined()
        );
    }
}

// Ports Go `TestTaskIgnoreErrors`.
#[test]
fn ignore_errors_controls_task_and_command_failure() {
    let dir = stage("ignore_errors");
    assert!(run(&dir, &["task-should-pass"]).ok());
    assert!(!run(&dir, &["task-should-fail"]).ok());
    assert!(run(&dir, &["cmd-should-pass"]).ok());
    assert!(!run(&dir, &["cmd-should-fail"]).ok());
}

// Ports Go `TestDisplaysErrorOnVersion1Schema`.
#[test]
fn version_1_schema_is_rejected() {
    let dir = stage("version/v1");
    let o = run(&dir, &[]);
    assert!(!o.ok(), "a v1 schema must be rejected");
    assert!(
        o.combined().contains("chema version") || o.combined().contains("version"),
        "expected a schema-version error, got: {}",
        o.combined()
    );
}

// The sprig helpers are reachable in Go function position, not only after a
// pipe, and take the subject as their last argument there.
#[test]
fn sprig_helpers_work_in_function_position() {
    let dir = stage("template_funcs");
    let o = run(&dir, &["funcs"]);
    assert!(o.ok(), "output: {}", o.combined());
    let out = o.combined();
    for want in [
        "suffix=dir/fr",
        "prefix=fr.po",
        "split=dir,fr.po",
        "has=truefalse",
        "first=dir",
        "default=fallback",
        "title=Hello Wide World",
    ] {
        assert!(out.contains(want), "expected {want:?} in: {out}");
    }

    // The pipeline spelling renders the same text as the function spelling,
    // including the five that mean something else as a minijinja builtin filter.
    let piped = run(&dir, &["pipes"]);
    assert!(piped.ok(), "output: {}", piped.combined());
    for want in [
        "suffix=dir/fr",
        "prefix=fr.po",
        "split=dir,fr.po",
        "default=fallback",
        "title=HELLO World",
        "first=dir",
        "last=fr.po",
        "join=dir/fr.po",
    ] {
        assert!(
            piped.combined().contains(want),
            "expected {want:?} in: {}",
            piped.combined()
        );
    }
}

// A file written natively in Jinja keeps standard Jinja meaning: only the Go
// dialect gets sprig's, and it gets it by translating a pipe into a call rather
// than by overriding the builtin filters.
#[test]
fn jinja_filters_keep_their_standard_meaning() {
    let dir = stage("template_funcs");
    let o = run(&dir, &["--taskfile", "jinja.yml", "jinja"]);
    assert!(o.ok(), "output: {}", o.combined());
    let out = o.combined();
    for want in [
        // Empty is not undefined, so the fallback does not fire...
        "default=\n",
        // ...nor does it for a legitimate zero.
        "zero=0",
        // Undefined still takes it.
        "missing=fallback",
        // Jinja's `title` lowercases the tail.
        "title=Hello World",
        // The sprig meaning stays reachable in function position.
        "sprig_default=fallback",
        "sprig_title=HELLO World",
        "sprig_join=dir/fr.po",
    ] {
        assert!(out.contains(want), "expected {want:?} in: {out}");
    }
}

// The Go dialect keeps sprig's meaning after a pipe, and `--migrate` has to
// carry that meaning into the converted file: the five helpers whose Jinja
// builtin means something else are translated to a call, not to a filter, so
// the migrated Taskfile renders exactly what the Go one did.
#[test]
fn migration_preserves_sprig_meaning_after_a_pipe() {
    let go = stage("template_funcs");
    let before = run(&go, &["pipes"]);
    assert!(before.ok(), "output: {}", before.combined());

    let migrated = stage("template_funcs");
    let w = run(&migrated, &["--migrate", "--write"]);
    assert!(w.ok(), "stderr: {}", w.stderr);
    let on_disk = std::fs::read_to_string(migrated.join("Taskfile.yml")).unwrap();
    // A call with the subject last, not `EMPTY | default("fallback")`, which
    // would mean "only if undefined" once the file is Jinja.
    assert!(
        on_disk.contains(r#"default("fallback", EMPTY)"#),
        "migrated to: {on_disk}"
    );
    // A helper whose builtin already matches sprig stays an idiomatic filter.
    assert!(on_disk.contains("| trimSuffix("), "migrated to: {on_disk}");

    let after = run(&migrated, &["pipes"]);
    assert!(after.ok(), "output: {}", after.combined());
    for line in before.combined().lines().filter(|l| l.contains('=')) {
        assert!(
            after.combined().contains(line),
            "migration changed {line:?}; after: {}",
            after.combined()
        );
    }
}

// The same diamond through `deps:`, which run in parallel. Each branch gets its
// own copy of the path, so `p-shared` on one branch is invisible to the other;
// a shared mutable set would reject this, and unpredictably so.
#[test]
fn parallel_diamond_is_not_cyclic() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["p-top"]);
    assert!(
        o.ok(),
        "a parallel diamond is not a cycle: {}",
        o.combined()
    );
    for task in ["p-shared", "p-left", "p-right", "p-top"] {
        assert!(
            o.combined().contains(&format!("[{task}]")),
            "{task} should have run: {}",
            o.combined()
        );
    }
}

// A cycle closed while compiling `sources: [{from: deps}]` recurses during
// compilation, before the call path is consulted. Without its own guard it
// overflowed the stack and aborted the process instead of naming the cycle.
#[test]
fn cycle_through_from_deps_globs_is_detected() {
    let dir = stage("cycle_paths");
    let o = run(&dir, &["g-1"]);
    assert!(!o.ok(), "expected a cycle error: {}", o.combined());
    assert_ne!(o.code, 134, "the process must not abort: {}", o.combined());
    assert!(
        o.combined().contains("Cyclic dependency detected: "),
        "expected the cycle to be named, got: {}",
        o.combined()
    );
}

// A task that calls itself with different vars is not a cycle: each turn
// compiles to a different command, so the path key differs. Task v3 allows
// this recursion idiom and so does task-rs.
#[test]
fn self_call_with_different_vars_is_not_cyclic() {
    let dir = stage("self_call_progresses");
    let o = run(&dir, &["countdown"]);
    assert!(o.ok(), "the countdown should run: {}", o.combined());
    for want in ["n=3", "n=2", "n=1"] {
        assert!(
            o.combined().contains(want),
            "expected {want:?} in: {}",
            o.combined()
        );
    }
    assert!(
        !o.combined().contains("Cyclic"),
        "must not report a cycle: {}",
        o.combined()
    );
}

// Two tasks expanding `sources: [{from: deps}]` over one shared dep compile
// concurrently, and that compile yields on an `sh:` var. The glob guard is
// carried per compilation path, so neither sees the other's in-flight entry;
// a single shared stack reported a cycle here that does not exist.
#[test]
fn parallel_from_deps_globs_are_not_cyclic() {
    let dir = stage("glob_from_deps_parallel");
    let o = run(&dir, &["top"]);
    assert!(o.ok(), "nothing here is cyclic: {}", o.combined());
    assert!(
        !o.combined().contains("Cyclic"),
        "must not report a cycle: {}",
        o.combined()
    );
}
