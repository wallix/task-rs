//! An in-memory OCI distribution registry for the cache tests.
//!
//! Test targets import this module directly with
//! `#[path = "common/registry.rs"] mod registry;`.
// Each target uses its own subset.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    /// Advertise vk-registry's transparent-zstd scheme.
    transparent_zstd: AtomicBool,
    /// Blob GETs received that accept zstd.
    zstd_blob_gets: AtomicU64,
    /// (seconds since start, method, path) of every request left unanswered.
    stalled: Mutex<Vec<(u64, String, String)>>,
    /// Ends the stalled requests and the accept loop.
    stop: AtomicBool,
}

/// Supports `oci-client` pushes and pulls for `repo` and can stall blob requests.
pub struct Registry {
    authority: String,
    state: Arc<State>,
    accept: Option<JoinHandle<()>>,
}

impl Registry {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let state = Arc::new(State::default());
        state.blob_gets_allowed.store(i64::MAX, Ordering::Relaxed);
        let shared = state.clone();
        let started = Instant::now();
        let accept = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                let state = shared.clone();
                std::thread::spawn(move || handle(stream, &state, started));
            }
        });
        Registry {
            authority,
            state,
            accept: Some(accept),
        }
    }

    /// The `host:port` the registry listens on.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Never answer any `/blobs/` request from now on.
    pub fn stall_blobs(&self) {
        self.state.stall_blobs.store(true, Ordering::Relaxed);
    }

    /// Answer `n` more blob GETs, then leave them unanswered too.
    pub fn allow_blob_gets(&self, n: i64) {
        self.state.blob_gets_allowed.store(n, Ordering::Relaxed);
    }

    /// Simulate a registry upgrade by advertising transparent-zstd. The mock
    /// still serves blobs as uploaded, so it can restore older entries and
    /// accept new saves. Restoring a new save requires encoding support.
    pub fn advertise_transparent_zstd(&self) {
        self.state.transparent_zstd.store(true, Ordering::Relaxed);
    }

    /// How many blob GETs accepted zstd.
    pub fn zstd_blob_gets(&self) -> u64 {
        self.state.zstd_blob_gets.load(Ordering::Relaxed)
    }

    /// Whether any blob was pushed.
    pub fn has_blobs(&self) -> bool {
        !self.state.blobs.lock().unwrap().is_empty()
    }

    /// The references manifests were pushed under, sorted.
    pub fn manifest_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .state
            .manifests
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        tags.sort();
        tags
    }

    /// Whether any manifest was pushed.
    pub fn has_manifests(&self) -> bool {
        !self.state.manifests.lock().unwrap().is_empty()
    }

    /// Distinct stalled request paths and the total number of stalled requests.
    pub fn stalled(&self) -> (BTreeSet<String>, usize) {
        let stalled = self.state.stalled.lock().unwrap();
        let digests = stalled.iter().map(|(_, _, path)| path.clone()).collect();
        (digests, stalled.len())
    }

    pub fn timeline(&self) -> String {
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
        // Connect to wake the blocked accept loop. If the connection fails,
        // joining the thread could hang.
        if TcpStream::connect(&self.authority).is_ok()
            && let Some(accept) = self.accept.take()
            && let Err(panic) = accept.join()
            && !std::thread::panicking()
        {
            // A test that is already failing keeps its own panic.
            std::panic::resume_unwind(panic);
        }
    }
}

pub struct Request {
    method: String,
    pub path: String,
    pub query: String,
    content_type: String,
    accept_encoding: String,
    body: Vec<u8>,
}

pub fn read_request(stream: &TcpStream) -> Option<Request> {
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
    let mut accept_encoding = String::new();
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
            "accept-encoding" => accept_encoding = value.trim().to_string(),
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
        accept_encoding,
        body,
    })
}

pub fn respond(mut stream: TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
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

pub fn length(n: usize) -> (&'static str, String) {
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
        let mut headers = vec![length(2)];
        if state.transparent_zstd.load(Ordering::Relaxed) {
            headers.push(("x-virtkit-transparent-zstd", "1".to_string()));
        }
        return respond(stream, 200, &headers, b"{}");
    }
    if is_blob_get && req.accept_encoding.split(',').any(|c| c.trim() == "zstd") {
        state.zstd_blob_gets.fetch_add(1, Ordering::Relaxed);
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
