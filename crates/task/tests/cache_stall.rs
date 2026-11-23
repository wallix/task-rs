//! A registry that accepts requests and then never answers, and a build-once
//! lock held by a peer, each keep `task` silent for tens of minutes.
//!
//! Each request is bounded (30 s without an answer, retried three times, so
//! about 134 s), but an entry is thousands of ~1 MiB chunks, eight in flight at
//! a time: a save or restore that walked every remaining chunk through those
//! retries would take hours to fail. The tests below drive the real binary
//! against an in-process registry that stalls on demand, and assert that the
//! first failed round ends the transfer, and that waiting on a held lock is
//! said out loud.
//!
//! The stall tests wait on the client's real timeouts and take minutes, hence
//! `#[ignore]`:
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
use common::*;

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Parallel chunk transfers in `ocicas` (`FETCH_CONCURRENCY`).
const CONCURRENCY: usize = 8;
/// What one unanswered request costs before it is final: four 30 s attempts
/// and the 2 + 4 + 8 s pauses between them, plus slack for the build itself.
const ONE_REQUEST_BUDGET: Duration = Duration::from_secs(180);
/// Size of the generated output: about two dozen chunks (they average 1.3 MiB),
/// so three rounds of eight. A transfer loop only waits out another round when
/// more than two rounds' worth of chunks are queued.
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

#[derive(Default)]
struct State {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    /// Reference → (content type, body).
    manifests: Mutex<HashMap<String, (String, Vec<u8>)>>,
    uploads: Mutex<HashMap<String, Vec<u8>>>,
    next_upload: AtomicU64,
    /// Never answer any `/blobs/` request.
    stall_blobs: AtomicBool,
    /// Blob GETs still answered before they stall too.
    blob_gets_allowed: AtomicI64,
    /// (seconds since start, method, path) of every request left unanswered.
    stalled: Mutex<Vec<(u64, String, String)>>,
    stop: AtomicBool,
}

/// An in-memory OCI distribution registry for the repository `repo`, enough of
/// it for `oci-client` to push and pull, that can stop answering on demand.
struct Registry {
    authority: String,
    state: Arc<State>,
}

impl Registry {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let state = Arc::new(State::default());
        state.blob_gets_allowed.store(i64::MAX, Ordering::Relaxed);
        let shared = state.clone();
        let started = Instant::now();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let state = shared.clone();
                std::thread::spawn(move || handle(stream, &state, started));
            }
        });
        Registry { authority, state }
    }

    /// Distinct blobs the registry left unanswered, and the request count.
    fn stalled(&self) -> (BTreeSet<String>, usize) {
        let stalled = self.state.stalled.lock().unwrap();
        let digests = stalled.iter().map(|(_, _, path)| path.clone()).collect();
        (digests, stalled.len())
    }

    fn timeline(&self) -> String {
        let stalled = self.state.stalled.lock().unwrap();
        let mut per_second: Vec<(u64, usize)> = Vec::new();
        for (at, _, _) in stalled.iter() {
            match per_second.last_mut() {
                Some((second, n)) if second == at => *n += 1,
                _ => per_second.push((*at, 1)),
            }
        }
        per_second
            .iter()
            .map(|(at, n)| format!("  t+{at:>4}s: {n} request(s) left unanswered\n"))
            .collect()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    content_type: String,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut content_type = String::new();
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header.trim().is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "content-type" => content_type = value.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        query,
        content_type,
        body,
    })
}

fn respond(mut stream: TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    // Best-effort: the client may already have given up on this connection.
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn length(n: usize) -> (&'static str, String) {
    ("Content-Length", n.to_string())
}

fn handle(stream: TcpStream, state: &State, started: Instant) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(req) = read_request(&stream) else {
        return;
    };
    let is_blob = req.path.starts_with("/v2/repo/blobs/");
    let is_blob_get = req.method == "GET" && req.path.starts_with("/v2/repo/blobs/sha256:");
    let stall = is_blob
        && (state.stall_blobs.load(Ordering::Relaxed)
            || (is_blob_get && state.blob_gets_allowed.fetch_sub(1, Ordering::Relaxed) <= 0));
    if stall {
        state.stalled.lock().unwrap().push((
            started.elapsed().as_secs(),
            req.method.clone(),
            req.path.clone(),
        ));
        // The connection stays open and silent, as through a proxy whose
        // upstream is gone, until the test is over.
        while !state.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        return;
    }

    let method = req.method.as_str();
    if req.path == "/v2/" {
        return respond(stream, 200, &[length(2)], b"{}");
    }
    if let Some(reference) = req.path.strip_prefix("/v2/repo/manifests/") {
        if method == "PUT" {
            state
                .manifests
                .lock()
                .unwrap()
                .insert(reference.to_string(), (req.content_type, req.body));
            return respond(
                stream,
                201,
                &[("Location", req.path.clone()), length(0)],
                b"",
            );
        }
        let found = state.manifests.lock().unwrap().get(reference).cloned();
        return match found {
            Some((content_type, body)) => {
                let headers = [("Content-Type", content_type), length(body.len())];
                respond(
                    stream,
                    200,
                    &headers,
                    if method == "HEAD" { b"" } else { &body },
                )
            }
            None => {
                let body = br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"missing"}]}"#;
                let headers = [
                    ("Content-Type", "application/json".to_string()),
                    length(body.len()),
                ];
                respond(stream, 404, &headers, body)
            }
        };
    }
    if req.path == "/v2/repo/blobs/uploads/" {
        let id = state.next_upload.fetch_add(1, Ordering::Relaxed);
        let location = format!("/v2/repo/blobs/uploads/{id}");
        state
            .uploads
            .lock()
            .unwrap()
            .insert(location.clone(), Vec::new());
        return respond(stream, 202, &[("Location", location), length(0)], b"");
    }
    if req.path.starts_with("/v2/repo/blobs/uploads/") {
        let mut uploads = state.uploads.lock().unwrap();
        let upload = uploads.entry(req.path.clone()).or_default();
        upload.extend_from_slice(&req.body);
        if method == "PATCH" {
            let headers = [
                ("Location", req.path.clone()),
                ("Range", format!("0-{}", upload.len().saturating_sub(1))),
                length(0),
            ];
            return respond(stream, 202, &headers, b"");
        }
        let digest = req
            .query
            .split('&')
            .find_map(|pair| pair.strip_prefix("digest="))
            .unwrap_or_default()
            .replace("%3A", ":");
        let data = uploads.remove(&req.path).unwrap_or_default();
        state.blobs.lock().unwrap().insert(digest.clone(), data);
        let location = format!("/v2/repo/blobs/{digest}");
        return respond(stream, 201, &[("Location", location), length(0)], b"");
    }
    if let Some(digest) = req.path.strip_prefix("/v2/repo/blobs/") {
        let found = state.blobs.lock().unwrap().get(digest).cloned();
        return match found {
            Some(body) => {
                let headers = [
                    ("Content-Type", "application/octet-stream".to_string()),
                    length(body.len()),
                ];
                respond(
                    stream,
                    200,
                    &headers,
                    if method == "HEAD" { b"" } else { &body },
                )
            }
            None => respond(stream, 404, &[length(0)], b""),
        };
    }
    respond(stream, 404, &[length(0)], b"");
}

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
    registry.state.stall_blobs.store(true, Ordering::Relaxed);

    let dir = project("cache_stall_save");
    let (out, code, elapsed) = build(&dir, &registry.authority);
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
}

// A healthy run saves the entry. A second checkout then finds the manifest and
// the index, but every chunk GET stalls: the restore must count as a miss once
// the first chunks have used their retries.
#[test]
#[ignore = "waits out the client's real retries: takes minutes"]
fn a_stalled_registry_fails_the_restore_once() {
    let registry = Registry::start();

    let first = project("cache_stall_restore_save");
    let (out, code, _) = build(&first, &registry.authority);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("saved to cache"), "{out}");

    // The index blob is the restore's first blob GET; the chunks follow.
    registry.state.blob_gets_allowed.store(1, Ordering::Relaxed);
    let second = project("cache_stall_restore");
    let (out, code, elapsed) = build(&second, &registry.authority);
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
}

/// A vk-registry lock API with one peer holding every lock until the test
/// releases it. An acquire is refused while it does, after holding the request
/// for up to a second; once released, acquires are granted.
struct LockServer {
    url: String,
    held: Arc<AtomicBool>,
}

impl LockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let held = Arc::new(AtomicBool::new(true));
        let shared = held.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let held = shared.clone();
                std::thread::spawn(move || {
                    let Some(req) = read_request(&stream) else {
                        return;
                    };
                    let body: &[u8] = match req.path.as_str() {
                        "/lock/acquire" => {
                            let polls = if req.query.contains("wait=0") { 0 } else { 50 };
                            for _ in 0..polls {
                                if !held.load(Ordering::Relaxed) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(20));
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
            url: format!("vk://{addr}/locks"),
            held,
        }
    }

    fn release(&self) {
        self.held.store(false, Ordering::Relaxed);
    }
}

/// A project whose `build` task takes its build-once lock from `lock_url`,
/// waiting `lock_timeout` on a held one.
fn locked_project(case: &str, lock_url: &str, lock_timeout: &str) -> std::path::PathBuf {
    let cache = empty_case_dir(&format!("{case}_store"));
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
    let dir = empty_case_dir(case);
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    std::fs::write(dir.join("input.txt"), "Clarence Boddicker\n").unwrap();
    dir
}

/// A running `task build` whose output is read as it is written, so a test
/// can tell what the user has been told so far from what is said at the end.
struct Live {
    child: std::process::Child,
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
            child,
            started: Instant::now(),
            lines,
            seen: String::new(),
        }
    }

    /// Waits for a line containing `needle` and returns when it was read,
    /// since the start. The child is still running then: its output is not
    /// over. Lines read on the way are not searched again.
    fn wait_for(&mut self, needle: &str) -> Duration {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(line) = self.lines.recv_timeout(left) else {
                // Not left waiting on a lock nobody will release.
                let _ = self.child.kill();
                panic!("no line containing {needle:?} in:\n{}", self.seen);
            };
            self.seen.push_str(&line);
            self.seen.push('\n');
            if line.contains(needle) {
                return self.started.elapsed();
            }
        }
    }

    /// Waits for the child to exit; returns everything it wrote and its code.
    fn finish(mut self) -> (String, i32) {
        let code = self.child.wait().unwrap().code().unwrap_or(-1);
        for line in self.lines.iter() {
            self.seen.push_str(&line);
            self.seen.push('\n');
        }
        eprintln!("--- task output:\n{}---", self.seen);
        (self.seen, code)
    }
}

// A peer holds the build-once lock and only lets go once the task has said,
// at default verbosity, that it is waiting, for which task and for how long at
// most: with the default timeout an hour otherwise passes in silence. The task
// then gets the remote lock and says how long that took.
#[test]
fn waiting_on_a_held_lock_is_said_before_it_is_acquired() {
    let server = LockServer::start();
    let dir = locked_project("cache_stall_lock", &server.url, "90s");

    let mut task = Live::build(&dir);
    task.wait_for(
        r#"task: "build" waits for its build-once lock, held by another run (up to 90s)"#,
    );
    assert!(
        task.child.try_wait().unwrap().is_none(),
        "the task did not wait for the held lock"
    );
    assert!(!dir.join("output.txt").exists(), "built without the lock");

    server.release();
    task.wait_for(r#"task: "build" acquired its build-once lock after "#);
    let (out, code) = task.finish();
    assert_eq!(code, 0, "{out}");
    assert!(dir.join("output.txt").exists(), "{out}");
    assert!(!out.contains("WARNING"), "{out}");
}

// A peer holds the build-once lock for longer than `lock_timeout`. The wait
// is announced when it starts, not along with its outcome, and the task then
// warns that it falls back to a local lock, and builds.
#[test]
fn a_lock_held_past_its_timeout_falls_back_with_a_warning() {
    let server = LockServer::start();
    let dir = locked_project("cache_stall_lock_fallback", &server.url, "4s");

    let mut task = Live::build(&dir);
    let announced = task
        .wait_for(r#"task: "build" waits for its build-once lock, held by another run (up to 4s)"#);
    let warned = task.wait_for(r#"task: WARNING: remote lock for "build" failed: "#);
    assert!(
        task.seen.ends_with("falling back to a local lock\n"),
        "{}",
        task.seen
    );
    assert!(
        warned.saturating_sub(announced) >= Duration::from_secs(3),
        "the wait was announced at {announced:?} and given up at {warned:?}: not while waiting"
    );
    let (out, code) = task.finish();
    assert_eq!(code, 0, "{out}");
    assert!(dir.join("output.txt").exists(), "{out}");
}

// Two deps of one run reach the same task, so the second waits on the lock
// the first holds. That is the run's own work, not a peer's: nothing is said
// about it unless asked, and never that another run holds the lock.
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

    let quiet = run("Bob Morton\n", &[]);
    assert!(!quiet.contains("build-once lock"), "{quiet}");

    let verbose = run("Anne Lewis\n", &["-v"]);
    assert!(
        verbose.contains(r#"task: "build" waits for its build-once lock, held by this run"#),
        "{verbose}"
    );
    assert!(!verbose.contains("another run"), "{verbose}");
}
