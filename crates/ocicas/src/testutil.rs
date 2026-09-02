//! Shared store and lock test scaffolding: a recording fake registry and the
//! crypto provider required by `reqwest` clients.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One request as the fake server saw it.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub(crate) method: String,
    /// Path only, without the query string.
    pub(crate) path: String,
    /// Raw query string.
    pub(crate) query: String,
    /// The `/lock/` API's `X-Vk-Lock-Holder` header.
    pub(crate) holder: Option<String>,
    /// The `/lock/` API's `X-Vk-Lock-Owner` header.
    pub(crate) owner: Option<String>,
    pub(crate) authorization: Option<String>,
}

/// A fake vk-registry that returns fixed responses and records request paths,
/// queries, and headers for wire-format assertions.
pub(crate) struct FakeServer {
    addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeServer {
    /// Serves `responses.len()` requests, one per entry; then stops.
    pub(crate) fn start(responses: Vec<(u16, &'static str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                serve_one(stream, status, body, &recorder);
            }
        });
        FakeServer { addr, seen }
    }

    /// `http://host:port`.
    pub(crate) fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("lock").clone()
    }
}

/// Read and record one bodyless HTTP/1.1 request, then return a fixed response.
///
/// Recording precedes the response so `seen()` is updated before the client can
/// resume.
fn serve_one(stream: TcpStream, status: u16, body: &str, recorder: &Mutex<Vec<Seen>>) {
    let Some(seen) = read_request(&stream) else {
        return;
    };
    recorder.lock().expect("lock").push(seen);

    let mut stream = stream;
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Response delivery is best-effort: tests assert on `seen()`, so the client
    // may disconnect first.
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
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

    let (mut holder, mut owner, mut authorization) = (None, None, None);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 || h.trim().is_empty() {
            break;
        }
        let Some((name, value)) = h.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match name.to_ascii_lowercase().as_str() {
            "x-vk-lock-holder" => holder = Some(value),
            "x-vk-lock-owner" => owner = Some(value),
            "authorization" => authorization = Some(value),
            _ => {}
        }
    }
    Some(Seen {
        method,
        path,
        query,
        holder,
        owner,
        authorization,
    })
}

/// Install the ring provider before building a `reqwest` client, as the `task`
/// binary does at startup for `rustls-no-provider`.
pub(crate) fn install_crypto() {
    // Fails only when a provider is already installed, which is the goal.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
