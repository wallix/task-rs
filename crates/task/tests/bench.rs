//! A fingerprinting benchmark, not run by `cargo test`: a tree of many small
//! files and a few large ones, timed for `--status` and an up-to-date run.
//!
//! ```text
//! TASK_BENCH_BASELINE=/path/to/older/task \
//!   cargo test --release -p task --test bench -- --ignored --nocapture
//! ```
//!
//! Optional: `TASK_BENCH_FILES` (default 200000), `TASK_BENCH_BIN` (default
//! this build's binary). Each binary is timed on all CPUs and pinned to one
//! (`taskset -c 0`), which caps fingerprinting at one thread. The tree goes
//! under `$TMPDIR`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const LARGE: usize = 4;
const LARGE_BYTES: usize = 64 << 20;

fn tree(files: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("task-bench-{files}"));
    let marker = dir.join(".complete");
    if marker.exists() {
        return dir;
    }
    let _ = std::fs::remove_dir_all(&dir);
    for i in 0..files {
        let sub = dir.join(format!("src/d{}/e{}", i % 100, (i / 100) % 20));
        if i < 2000 {
            std::fs::create_dir_all(&sub).unwrap();
        }
        let body = format!("file {i}\n").repeat(1 + i % 64);
        std::fs::write(sub.join(format!("f{i}.txt")), body).unwrap();
    }
    for i in 0..LARGE {
        let byte = u8::try_from(i).unwrap();
        std::fs::write(
            dir.join(format!("src/large{i}.bin")),
            vec![byte; LARGE_BYTES],
        )
        .unwrap();
    }
    std::fs::write(
        dir.join("Taskfile.yml"),
        "version: '3'\ntasks:\n  build:\n    sources: ['src/**/*']\n    generates: [out.txt]\n    cmds: ['echo built > out.txt']\n",
    )
    .unwrap();
    std::fs::write(&marker, "").unwrap();
    dir
}

/// Runs `bin` in `dir`, returning the wall time.
fn time(bin: &Path, dir: &Path, args: &[&str], one_cpu: bool) -> Duration {
    let mut cmd = if one_cpu {
        let mut c = Command::new("taskset");
        c.args(["-c", "0"]).arg(bin);
        c
    } else {
        Command::new(bin)
    };
    let start = Instant::now();
    let out = cmd.args(args).current_dir(dir).output().unwrap();
    let took = start.elapsed();
    assert!(
        out.status.success() || args.contains(&"--status"),
        "{bin:?} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    took
}

fn median(bin: &Path, dir: &Path, args: &[&str], one_cpu: bool) -> Duration {
    let mut runs: Vec<Duration> = (0..3).map(|_| time(bin, dir, args, one_cpu)).collect();
    runs.sort();
    runs[1]
}

#[test]
#[ignore = "benchmark: run explicitly with --ignored"]
fn fingerprint_benchmark() {
    let files = std::env::var("TASK_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let dir = tree(files);
    let current = std::env::var_os("TASK_BENCH_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_task")), PathBuf::from);
    let mut bins = vec![("current", current)];
    if let Some(base) = std::env::var_os("TASK_BENCH_BASELINE") {
        bins.insert(0, ("baseline", PathBuf::from(base)));
    }
    println!("{files} small files + {LARGE} x {} MiB", LARGE_BYTES >> 20);
    println!(
        "{:<10} {:<5} {:>12} {:>12} {:>12}",
        "binary", "cpus", "first run", "up to date", "--status"
    );
    for (name, bin) in &bins {
        for one_cpu in [true, false] {
            // Warm the page cache, then start from no saved state.
            let _ = std::fs::remove_dir_all(dir.join(".task"));
            let _ = time(bin, &dir, &["build"], one_cpu);
            let _ = std::fs::remove_dir_all(dir.join(".task"));
            let first = time(bin, &dir, &["build"], one_cpu);
            let fresh = median(bin, &dir, &["build"], one_cpu);
            let status = median(bin, &dir, &["--status", "build"], one_cpu);
            let cpus = if one_cpu { "1" } else { "all" };
            println!(
                "{name:<10} {cpus:<5} {:>11.2}s {:>11.2}s {:>11.2}s",
                first.as_secs_f64(),
                fresh.as_secs_f64(),
                status.as_secs_f64()
            );
        }
    }
}
