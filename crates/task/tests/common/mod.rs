//! Shared harness for the behavioral parity tests ported from Go `task_test.go`.
//!
//! Each test drives the real `task` binary against a copy of a shared
//! `testdata/` Taskfile and asserts on generated files, output, and exit codes.
//! The case is copied into a temp directory first so the source tree stays clean
//! and repeated runs start fresh.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The compiled `task` binary under test.
pub const BIN: &str = env!("CARGO_BIN_EXE_task");

/// Path to a shared `testdata/<case>` directory.
pub fn testdata(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(case)
}

/// Copies `testdata/<case>` into a fresh temp directory and returns its path.
pub fn stage(case: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dst = std::env::temp_dir().join(format!(
        "task-bt-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        case.replace('/', "_")
    ));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&testdata(case), &dst);
    dst
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            copy_dir(&entry.path(), &to);
        } else if ty.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(entry.path()).unwrap();
                std::os::unix::fs::symlink(target, &to).unwrap();
            }
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// The captured result of one binary invocation.
pub struct Run {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Run {
    /// stdout and stderr concatenated (the Go tests often point both at one
    /// buffer).
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Output captured with stdout and stderr merged in write order.
pub struct Merged {
    pub out: String,
    pub code: i32,
}

impl Merged {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Runs the binary capturing stdout and stderr **interleaved in write order**
/// (both fds share one file), matching Go's tests that point both streams at a
/// single buffer. Use this for order-sensitive assertions; `run` captures the
/// streams separately.
pub fn run_merged(dir: &Path, args: &[&str]) -> Merged {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "task-merged-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::File::create(&path).expect("create merge file");
    let clone = file.try_clone().expect("clone merge fd");
    let status = Command::new(BIN)
        .args(args)
        .current_dir(dir)
        // These fixtures are legacy Go-dialect Taskfiles; silence the migration
        // nudge so it does not pollute the asserted output. A dedicated test
        // exercises the warning itself.
        .env("TASK_NO_GO_DEPRECATION", "1")
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::from(clone))
        .status()
        .expect("spawn task binary");
    let out = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    Merged {
        out,
        code: status.code().unwrap_or(-1),
    }
}

/// Runs the binary in `dir` with `args`, capturing output. Output is piped (not
/// a TTY), so color is disabled and the text matches the Go tests' buffers.
pub fn run(dir: &Path, args: &[&str]) -> Run {
    let out = Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .env("TASK_NO_GO_DEPRECATION", "1")
        .output()
        .expect("spawn task binary");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}
