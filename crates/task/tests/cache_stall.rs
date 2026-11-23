//! A large cache entry can contain thousands of chunks. Retrying every batch
//! against a silent registry could leave task quiet for hours. These tests run
//! the binary against an in-process registry that stops answering and check
//! that the first failed chunk ends the save or restore. They also check that a
//! build-once lock held by a peer is reported while `task` waits on it (by the
//! same run, only with `-v`).
//!
//! The stall tests are ignored by default because the real retries take minutes.
//!
//! ```sh
//! cargo test -p task --test cache_stall -- --ignored --nocapture
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
#[path = "common/registry.rs"]
mod registry;
use common::*;
use registry::{Registry, length, read_request, respond};

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Parallel chunk transfers: `FETCH_CONCURRENCY` in `ocicas::store`.
const CONCURRENCY: usize = 8;
/// Four 30 s attempts plus backoff of 2, 4 and 8 s total about 134 s
/// (`READ_TIMEOUT`, `retry::RETRIES` and `retry::FIRST_PAUSE`). Allow extra
/// time for the build.
const ONE_REQUEST_BUDGET: Duration = Duration::from_secs(180);
/// Random output yields roughly two dozen chunks at about 1.3 MiB each,
/// enough to leave chunks queued after the first eight requests stall.
const OUTPUT_BYTES: usize = 32 << 20;

const TASKFILE: &str = r#"version: '3'

tasks:
  build:
    sources: [input.txt]
    generates: [output.bin]
    cache:
      url: 'oci://{{.REGISTRY}}/repo:build?plainhttp=1'
    cmds:
      - head -c OUTPUT_BYTES /dev/urandom > output.bin
"#;

/// A project whose `build` task writes a random, multi-chunk `output.bin`.
fn project(case: &str) -> std::path::PathBuf {
    let dir = empty_case_dir(case);
    let taskfile = TASKFILE.replace("OUTPUT_BYTES", &OUTPUT_BYTES.to_string());
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    std::fs::write(dir.join("input.txt"), "ED-209\n").unwrap();
    dir
}

/// Runs `task build` in `dir` with its own empty local chunk store, returning
/// the merged output, the exit code and how long the run took.
fn build(dir: &Path, registry: &str) -> (String, i32, Duration) {
    let cas = dir.join(".cas");
    let started = Instant::now();
    let out = Command::new(BIN)
        .arg("build")
        .current_dir(dir)
        .env("REGISTRY", registry)
        .env("TASK_CACHE_OCI_CAS_DIR", &cas)
        .output()
        .expect("spawn task binary");
    let elapsed = started.elapsed();
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (s, out.status.code().unwrap_or(-1), elapsed)
}

fn report(what: &str, registry: &Registry, out: &str, elapsed: Duration) {
    let (digests, requests) = registry.stalled();
    eprintln!(
        "--- {what}: {:.0} s, {} distinct blob(s) left unanswered over {requests} request(s)\n{}--- task output:\n{out}---",
        elapsed.as_secs_f64(),
        digests.len(),
        registry.timeline(),
    );
}

// The registry answers the restore (a miss) and then never answers a blob
// request: the save must give up once the first chunks have used their
// retries, not walk every remaining chunk through the same wait.
#[test]
#[ignore = "waits out the client's real retries: takes minutes"]
fn a_stalled_registry_fails_the_save_once() {
    let registry = Registry::start();
    registry.stall_blobs();

    let dir = project("cache_stall_save");
    let (out, code, elapsed) = build(&dir, registry.authority());
    report("save", &registry, &out, elapsed);

    assert_eq!(code, 0, "{out}");
    assert_eq!(
        std::fs::metadata(dir.join("output.bin")).unwrap().len(),
        OUTPUT_BYTES as u64
    );
    assert!(out.contains("WARNING"), "{out}");
    let (digests, _) = registry.stalled();
    assert!(
        digests.len() <= CONCURRENCY,
        "the save kept going after its first chunks failed: {} blobs tried",
        digests.len()
    );
    assert!(
        elapsed < ONE_REQUEST_BUDGET,
        "the save took {elapsed:?}, more than one request's retries"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// A healthy run saves the entry. A second checkout then finds the manifest and
// the index, but every chunk GET stalls: the restore must count as a miss once
// the first chunks have used their retries.
#[test]
#[ignore = "waits out the client's real retries: takes minutes"]
fn a_stalled_registry_fails_the_restore_once() {
    let registry = Registry::start();

    let first = project("cache_stall_restore_save");
    let (out, code, _) = build(&first, registry.authority());
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("saved to cache"), "{out}");

    // The index blob is the restore's first blob GET; the chunks follow.
    registry.allow_blob_gets(1);
    let second = project("cache_stall_restore");
    let (out, code, elapsed) = build(&second, registry.authority());
    report("restore", &registry, &out, elapsed);

    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("restored from cache"), "{out}");
    assert!(second.join("output.bin").exists(), "{out}");
    let (digests, _) = registry.stalled();
    assert!(!digests.is_empty(), "no chunk GET reached the registry");
    assert!(
        digests.len() <= CONCURRENCY,
        "the restore kept going after its first chunks failed: {} blobs tried",
        digests.len()
    );
    assert!(
        elapsed < ONE_REQUEST_BUDGET,
        "the restore took {elapsed:?}, more than one request's retries"
    );
    std::fs::remove_dir_all(&first).ok();
    std::fs::remove_dir_all(&second).ok();
}

#[test]
fn a_dropped_registry_refuses_connections() {
    let registry = Registry::start();
    let authority = registry.authority().to_string();
    drop(registry);

    let mut stream = match TcpStream::connect(&authority) {
        Err(err) => {
            return assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "{err}");
        }
        // A parallel test's `LockServer` may have reused the port.
        Ok(stream) => stream,
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(b"GET /v2/ HTTP/1.1\r\n\r\n").unwrap();
    let mut answer = String::new();
    // The new listener may reset the connection.
    let _ = stream.read_to_string(&mut answer);
    assert!(
        !answer.ends_with("\r\n\r\n{}"),
        "the registry still answers its ping: {answer}"
    );
}

/// How long [`LockServer`] holds an acquire before refusing it: a long poll
/// like the one `ocicas::lock` asks for (up to its `POLL_CAP`, 30 s), cut
/// short so a test's `lock_timeout` can be seconds.
const ACQUIRE_HOLD: Duration = Duration::from_secs(1);
/// How often a held acquire looks at the lock again.
const ACQUIRE_POLL: Duration = Duration::from_millis(20);
/// The `lock_timeout` of the fallback test: several [`ACQUIRE_HOLD`]s, so the
/// wait is refused more than once before it is given up.
const FALLBACK_LOCK_TIMEOUT: Duration = Duration::from_secs(4);

/// A vk-registry lock API with one peer holding every lock until the test
/// releases it. An acquire is refused while it does, after holding the request
/// for up to [`ACQUIRE_HOLD`]; once released, acquires are granted.
struct LockServer {
    addr: std::net::SocketAddr,
    url: String,
    held: Arc<AtomicBool>,
    /// Ends the held acquires and the accept loop.
    stop: Arc<AtomicBool>,
}

impl LockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let held = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = (held.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let (held, stop) = shared.clone();
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::spawn(move || {
                    let Some(req) = read_request(&stream) else {
                        return;
                    };
                    let body: &[u8] = match req.path.as_str() {
                        "/lock/acquire" => {
                            let hold = if req.query.contains("wait=0") {
                                Duration::ZERO
                            } else {
                                ACQUIRE_HOLD
                            };
                            let asked = Instant::now();
                            while held.load(Ordering::Relaxed)
                                && !stop.load(Ordering::Relaxed)
                                && asked.elapsed() < hold
                            {
                                std::thread::sleep(ACQUIRE_POLL);
                            }
                            if held.load(Ordering::Relaxed) {
                                return respond(stream, 409, &[length(2)], b"{}");
                            }
                            br#"{"owner":"murphy"}"#
                        }
                        "/lock/renew" => br#"{"renewed":1,"of":1}"#,
                        _ => br#"{"released":1}"#,
                    };
                    respond(stream, 200, &[length(body.len())], body);
                });
            }
        });
        LockServer {
            addr,
            url: format!("vk://{addr}/locks"),
            held,
            stop,
        }
    }

    fn release(&self) {
        self.held.store(false, Ordering::Relaxed);
    }
}

impl Drop for LockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wakes the accept loop so that it sees the flag. Best-effort.
        let _ = TcpStream::connect(self.addr);
    }
}

/// A project whose `build` task takes its build-once lock from `lock_url`,
/// waiting `lock_timeout` on a held one. Its cache store is in the project.
fn locked_project(case: &str, lock_url: &str, lock_timeout: &str) -> std::path::PathBuf {
    let dir = empty_case_dir(case);
    let cache = dir.join("store");
    std::fs::create_dir_all(&cache).unwrap();
    let taskfile = format!(
        r#"version: '3'
tasks:
  build:
    sources: [input.txt]
    generates: [output.txt]
    cache:
      url: 'file://{}/build-{{{{.CHECKSUM}}}}.zip'
      lock: '{lock_url}'
      lock_timeout: {lock_timeout}
    cmds:
      - cp input.txt output.txt
"#,
        cache.display()
    );
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    std::fs::write(dir.join("input.txt"), "Clarence Boddicker\n").unwrap();
    dir
}

/// Read a running `task build` line by line to check when it reports contention.
struct Live {
    /// Taken by [`Live::finish`]; a test that ends before it kills the child.
    child: Option<std::process::Child>,
    started: Instant,
    lines: std::sync::mpsc::Receiver<String>,
    /// Every line read so far.
    seen: String,
}

impl Live {
    fn build(dir: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("build")
            .current_dir(dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn task binary");
        let (tx, lines) = std::sync::mpsc::channel();
        let follow = |pipe: Box<dyn Read + Send>, tx: std::sync::mpsc::Sender<String>| {
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    // The receiver outlives the child; a failed test drops it.
                    let _ = tx.send(line);
                }
            });
        };
        follow(Box::new(child.stdout.take().unwrap()), tx.clone());
        follow(Box::new(child.stderr.take().unwrap()), tx);
        Live {
            child: Some(child),
            started: Instant::now(),
            lines,
            seen: String::new(),
        }
    }

    /// Consume lines through the next match and return the elapsed time since
    /// startup. Previously consumed lines are skipped.
    fn wait_for(&mut self, needle: &str) -> Duration {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(line) = self.lines.recv_timeout(left) else {
                // The child is killed on the way out, not left waiting on a
                // lock nobody will release.
                panic!("no line containing {needle:?} in:\n{}", self.seen);
            };
            self.seen.push_str(&line);
            self.seen.push('\n');
            if line.contains(needle) {
                return self.started.elapsed();
            }
        }
    }

    /// Whether the child has not exited yet.
    fn running(&mut self) -> bool {
        let child = self.child.as_mut().expect("the child is not finished");
        child.try_wait().unwrap().is_none()
    }

    /// Waits for the child to exit; returns everything it wrote and its code.
    fn finish(mut self) -> (String, i32) {
        let mut child = self.child.take().expect("the child is not finished");
        let code = child.wait().unwrap().code().unwrap_or(-1);
        for line in self.lines.iter() {
            self.seen.push_str(&line);
            self.seen.push('\n');
        }
        eprintln!("--- task output:\n{}---", self.seen);
        (std::mem::take(&mut self.seen), code)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort: it may have exited already.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// Release the peer's lock only after the default output names the waiting
// task and its limit. Acquisition must then report elapsed time.
#[test]
fn waiting_on_a_held_lock_is_said_before_it_is_acquired() {
    let server = LockServer::start();
    let dir = locked_project("cache_stall_lock", &server.url, "90s");

    let mut task = Live::build(&dir);
    task.wait_for(
        r#"task: "build" waits for its build-once lock, held by another run (up to 90s)"#,
    );
    assert!(task.running(), "the task did not wait for the held lock");
    assert!(!dir.join("output.txt").exists(), "built without the lock");

    server.release();
    task.wait_for(r#"task: "build" acquired its build-once lock after "#);
    let (out, code) = task.finish();
    assert_eq!(code, 0, "{out}");
    assert!(dir.join("output.txt").exists(), "{out}");
    assert!(!out.contains("WARNING"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

// Keep the peer's lock past the timeout to check the order of the wait
// message, fallback warning, and local build.
#[test]
fn a_lock_held_past_its_timeout_falls_back_with_a_warning() {
    let server = LockServer::start();
    let limit = format!("{}s", FALLBACK_LOCK_TIMEOUT.as_secs());
    let dir = locked_project("cache_stall_lock_fallback", &server.url, &limit);

    let mut task = Live::build(&dir);
    let announced = task.wait_for(&format!(
        r#"task: "build" waits for its build-once lock, held by another run (up to {limit})"#
    ));
    let warned = task.wait_for(r#"task: WARNING: remote lock for "build" failed: "#);
    assert!(
        task.seen.ends_with("falling back to a local lock\n"),
        "{}",
        task.seen
    );
    // The timeout runs from the announcement; one refused acquire of slack.
    assert!(
        warned.saturating_sub(announced) >= FALLBACK_LOCK_TIMEOUT - ACQUIRE_HOLD,
        "the wait was announced at {announced:?} and given up at {warned:?}: not while waiting"
    );
    let (out, code) = task.finish();
    assert_eq!(code, 0, "{out}");
    assert!(dir.join("output.txt").exists(), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

// With the default `run: always`, both paths through the diamond reach build.
// A one-second sleep spans several 100 ms file-lock retries. First verify
// contention with -v, then change the input to force a fresh build without -v.
#[test]
fn a_lock_held_by_the_same_run_is_only_said_in_verbose_mode() {
    let dir = empty_case_dir("cache_stall_lock_same_run");
    let taskfile = r#"version: '3'
tasks:
  default:
    deps: [lewis, morton]
  lewis:
    deps: [build]
  morton:
    deps: [build]
  build:
    sources: [input.txt]
    generates: [output.txt]
    cmds:
      - sleep 1
      - cp input.txt output.txt
"#;
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    let run = |input: &str, args: &[&str]| {
        std::fs::write(dir.join("input.txt"), input).unwrap();
        let out = Command::new(BIN)
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("spawn task binary");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{err}");
        err
    };

    let verbose = run("Anne Lewis\n", &["-v"]);
    assert!(
        verbose.contains(r#"task: "build" waits for its build-once lock, held by this run"#),
        "{verbose}"
    );
    assert!(!verbose.contains("another run"), "{verbose}");

    let quiet = run("Bob Morton\n", &[]);
    assert!(!quiet.contains("build-once lock"), "{quiet}");
    // The second path found the first one's build, as in the verbose run,
    // where it contended.
    assert!(quiet.contains(r#"Task "build" is up to date"#), "{quiet}");
    std::fs::remove_dir_all(&dir).ok();
}
