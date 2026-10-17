//! A fingerprinting benchmark, not run by `cargo test`: a tree of many small
//! files and a few large ones, timed for `--status` and an up-to-date run.
//!
//! ```text
//! TASK_BENCH_BASELINE=4.8.1=/path/to/task,older=/path/to/task \
//!   cargo test --release -p task --test bench -- --ignored --nocapture
//! ```
//!
//! Optional: `TASK_BENCH_FILES` (default 200000), `TASK_BENCH_BIN` (default
//! this build's binary), `TASK_BENCH_TASKS` (default `build,scan,retain,drop`).
//! Each binary is timed on all CPUs and pinned to one (`taskset -c 0`), which
//! caps fingerprinting at one thread, for each task: `build` has `generates`
//! and takes the build-once lock, `scan` has none, and `retain` and `drop`
//! weigh exclusions that keep nearly every file or drop most. The tree goes
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

const TASKFILE: &str = "version: '3'\ntasks:\n  build:\n    sources: ['src/**/*']\n    generates: [out.txt]\n    cmds: ['echo built > out.txt']\n  scan:\n    sources: ['src/**/*']\n    cmds: ['true']\n";

/// Tasks whose exclusions keep nearly every file (20 absent paths, a suffix
/// and a tree that match nothing) or drop most (a pruned tree and a
/// wildcard).
fn exclusion_tasks() -> String {
    let mut retain = String::from("  retain:\n    sources:\n      - 'src/**/*'\n");
    for i in 0..20 {
        retain.push_str(&format!("      - exclude: src/absent{i}.txt\n"));
    }
    retain.push_str("      - exclude: '**/*.pyc'\n      - exclude: 'src/**/node_modules/**'\n");
    retain.push_str("    cmds: ['true']\n");
    let drop = concat!(
        "  drop:\n    sources:\n      - 'src/**/*'\n",
        "      - exclude: 'src/d[1-9]*/**'\n      - exclude: 'src/**/f*7.txt'\n",
        "    cmds: ['true']\n",
    );
    format!("{retain}{drop}")
}

fn tree(files: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("task-bench-{files}"));
    let marker = dir.join(".complete");
    if !marker.exists() {
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
        std::fs::write(&marker, "").unwrap();
    }
    std::fs::write(
        dir.join("Taskfile.yml"),
        format!("{TASKFILE}{}", exclusion_tasks()),
    )
    .unwrap();
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
    let mut bins: Vec<(String, PathBuf)> = std::env::var("TASK_BENCH_BASELINE")
        .unwrap_or_default()
        .split(',')
        .filter(|b| !b.is_empty())
        .map(|b| match b.split_once('=') {
            Some((name, path)) => (name.to_string(), PathBuf::from(path)),
            None => ("baseline".to_string(), PathBuf::from(b)),
        })
        .collect();
    bins.push(("current".to_string(), current));
    let tasks =
        std::env::var("TASK_BENCH_TASKS").unwrap_or_else(|_| "build,scan,retain,drop".to_string());
    println!("{files} small files + {LARGE} x {} MiB", LARGE_BYTES >> 20);
    println!(
        "{:<10} {:<6} {:<5} {:>12} {:>12} {:>12}",
        "binary", "task", "cpus", "first run", "up to date", "--status"
    );
    for (name, bin) in &bins {
        for task in tasks.split(',') {
            for one_cpu in [true, false] {
                // Warm the page cache, then start from no saved state.
                let _ = std::fs::remove_dir_all(dir.join(".task"));
                let _ = time(bin, &dir, &[task], one_cpu);
                let _ = std::fs::remove_dir_all(dir.join(".task"));
                let first = time(bin, &dir, &[task], one_cpu);
                let fresh = median(bin, &dir, &[task], one_cpu);
                let status = median(bin, &dir, &["--status", task], one_cpu);
                let cpus = if one_cpu { "1" } else { "all" };
                println!(
                    "{name:<10} {task:<6} {cpus:<5} {:>11.2}s {:>11.2}s {:>11.2}s",
                    first.as_secs_f64(),
                    fresh.as_secs_f64(),
                    status.as_secs_f64()
                );
            }
        }
    }
}
