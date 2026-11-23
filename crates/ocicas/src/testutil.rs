//! Shared store and lock test scaffolding: a recording fake registry, an
//! in-memory one that stores what it is pushed, and the crypto provider
//! required by `reqwest` clients.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::cas::sha256_hex;

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
    pub(crate) accept_encoding: Option<String>,
    pub(crate) content_encoding: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
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
        Self::start_raw(
            responses
                .into_iter()
                .map(|(status, body)| {
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                })
                .collect(),
        )
    }

    /// Raw responses allow truncated bodies, dropped connections and upload locations.
    pub(crate) fn start_raw(responses: Vec<String>) -> Self {
        Self::start_bytes(responses.into_iter().map(String::into_bytes).collect())
    }

    /// [`FakeServer::start_raw`] for responses whose body is not text.
    pub(crate) fn start_bytes(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                serve_one(stream, &response, &recorder);
            }
        });
        FakeServer { addr, seen }
    }

    /// `host:port`.
    pub(crate) fn authority(&self) -> String {
        self.addr.to_string()
    }

    /// `http://host:port`.
    pub(crate) fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("lock").clone()
    }
}

/// Read and record one HTTP/1.1 request, then return a fixed response.
///
/// Recording precedes the response so `seen()` is updated before the client can
/// resume.
fn serve_one(stream: TcpStream, response: &[u8], recorder: &Mutex<Vec<Seen>>) {
    let Some(seen) = read_request(&stream) else {
        return;
    };
    recorder.lock().expect("lock").push(seen);

    let mut stream = stream;
    // Response delivery is best-effort: tests assert on `seen()`, so the client
    // may disconnect first.
    let _ = stream.write_all(response);
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
    let (mut accept_encoding, mut content_encoding, mut content_type) = (None, None, None);
    let mut length = 0;
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
            "accept-encoding" => accept_encoding = Some(value),
            "content-encoding" => content_encoding = Some(value),
            "content-type" => content_type = Some(value),
            "content-length" => length = value.parse().ok()?,
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        query,
        holder,
        owner,
        authorization,
        accept_encoding,
        content_encoding,
        content_type,
        body,
    })
}

/// A `200` response carrying `body`, under `encoding` when given.
pub(crate) fn blob_response(encoding: Option<&str>, body: &[u8]) -> Vec<u8> {
    let encoding: Vec<_> = encoding
        .map(|e| ("Content-Encoding", e))
        .into_iter()
        .collect();
    blob_response_with(&encoding, body)
}

/// A `200` response carrying `body` and the header fields `fields`, in order.
pub(crate) fn blob_response_with(fields: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let fields: String = fields
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\n{fields}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// How a [`Registry`] answers, set by a test while it runs.
#[derive(Default)]
pub(crate) struct Behaviour {
    /// Advertise and accept the transparent-zstd scheme, as a vk-registry does.
    pub(crate) transparent: AtomicBool,
    /// Serve every blob decoded, whatever the request accepts: a proxy that
    /// undoes the encoding.
    pub(crate) decode: AtomicBool,
}

/// A stored blob: its bytes as pushed, and whether they are a zstd frame of
/// the content the digest names.
#[derive(Clone)]
struct Blob {
    data: Vec<u8>,
    zstd: bool,
}

#[derive(Default)]
struct RegistryState {
    behaviour: Behaviour,
    /// Ends the accept loop.
    stop: AtomicBool,
    blobs: Mutex<HashMap<String, Blob>>,
    /// Reference → (content type, body).
    manifests: Mutex<HashMap<String, (String, Vec<u8>)>>,
    uploads: Mutex<HashMap<String, Vec<u8>>>,
    next_upload: AtomicU64,
    seen: Mutex<Vec<Seen>>,
}

/// An in-memory OCI registry for the repository `repo`: enough of the
/// distribution API for a store to push and pull, and of vk-registry's
/// transparent-zstd scheme, where a blob pushed as a frame is served as one to
/// a request that accepts zstd, and decoded to any other.
pub(crate) struct Registry {
    addr: std::net::SocketAddr,
    state: Arc<RegistryState>,
    /// Taken by `Drop`.
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Registry {
    pub(crate) fn start(transparent: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(RegistryState::default());
        state
            .behaviour
            .transparent
            .store(transparent, Ordering::Relaxed);
        let shared = state.clone();
        let accept = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                let state = shared.clone();
                std::thread::spawn(move || registry_serve(stream, &state));
            }
        });
        Registry {
            addr,
            state,
            accept: Some(accept),
        }
    }

    /// `host:port`.
    pub(crate) fn authority(&self) -> String {
        self.addr.to_string()
    }

    pub(crate) fn behaviour(&self) -> &Behaviour {
        &self.state.behaviour
    }

    /// The `GET`s of blobs under `digests` received so far.
    pub(crate) fn blob_gets<'a>(&self, digests: impl IntoIterator<Item = &'a str>) -> Vec<Seen> {
        let paths: Vec<String> = digests
            .into_iter()
            .map(|d| format!("/v2/repo/blobs/{d}"))
            .collect();
        let seen = self.state.seen.lock().expect("lock");
        seen.iter()
            .filter(|r| r.method == "GET" && paths.contains(&r.path))
            .cloned()
            .collect()
    }

    /// The stored bytes of the blob `digest` and whether they are a zstd frame.
    pub(crate) fn blob(&self, digest: &str) -> Option<(Vec<u8>, bool)> {
        let blobs = self.state.blobs.lock().expect("lock");
        blobs.get(digest).map(|b| (b.data.clone(), b.zstd))
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        // Connect to wake the blocked accept loop. If the connection fails,
        // joining the thread could hang.
        if TcpStream::connect(self.addr).is_ok()
            && let Some(accept) = self.accept.take()
            && let Err(panic) = accept.join()
            && !std::thread::panicking()
        {
            // A test that is already failing keeps its own panic.
            std::panic::resume_unwind(panic);
        }
    }
}

fn registry_respond(mut stream: TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
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

fn registry_serve(stream: TcpStream, state: &RegistryState) {
    let Some(req) = read_request(&stream) else {
        return;
    };
    state.seen.lock().expect("lock").push(req.clone());
    let length = |n: usize| ("Content-Length", n.to_string());
    let transparent = state.behaviour.transparent.load(Ordering::Relaxed);
    let (method, head) = (req.method.as_str(), req.method == "HEAD");
    // Each endpoint answers the methods a store uses on it and no other.
    let allowed: &[&str] = if req.path == "/v2/" {
        &["GET"]
    } else if req.path.starts_with("/v2/repo/manifests/") {
        &["GET", "HEAD", "PUT"]
    } else if req.path == "/v2/repo/blobs/uploads/" {
        &["POST"]
    } else if req.path.starts_with("/v2/repo/blobs/uploads/") {
        &["PATCH", "PUT"]
    } else if req.path.starts_with("/v2/repo/blobs/") {
        &["GET", "HEAD"]
    } else {
        return registry_respond(stream, 404, &[length(0)], b"");
    };
    if !allowed.contains(&method) {
        let headers = [("Allow", allowed.join(", ")), length(0)];
        return registry_respond(stream, 405, &headers, b"");
    }

    if req.path == "/v2/" {
        let mut headers = vec![length(2)];
        if transparent {
            headers.push(("x-virtkit-transparent-zstd", "1".to_string()));
        }
        return registry_respond(stream, 200, &headers, b"{}");
    }
    if let Some(reference) = req.path.strip_prefix("/v2/repo/manifests/") {
        if method == "PUT" {
            let manifest = (req.content_type.unwrap_or_default(), req.body);
            let mut manifests = state.manifests.lock().expect("lock");
            manifests.insert(reference.to_string(), manifest);
            let headers = [("Location", req.path.clone()), length(0)];
            return registry_respond(stream, 201, &headers, b"");
        }
        let found = state
            .manifests
            .lock()
            .expect("lock")
            .get(reference)
            .cloned();
        return match found {
            Some((content_type, body)) => {
                let headers = [("Content-Type", content_type), length(body.len())];
                registry_respond(stream, 200, &headers, if head { b"" } else { &body })
            }
            None => {
                let body = br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"missing"}]}"#;
                let headers = [
                    ("Content-Type", "application/json".to_string()),
                    length(body.len()),
                ];
                registry_respond(stream, 404, &headers, body)
            }
        };
    }
    if req.path == "/v2/repo/blobs/uploads/" {
        let id = state.next_upload.fetch_add(1, Ordering::Relaxed);
        let location = format!("/v2/repo/blobs/uploads/{id}");
        let mut uploads = state.uploads.lock().expect("lock");
        uploads.insert(location.clone(), Vec::new());
        return registry_respond(stream, 202, &[("Location", location), length(0)], b"");
    }
    if req.path.starts_with("/v2/repo/blobs/uploads/") {
        let mut uploads = state.uploads.lock().expect("lock");
        // Only a session a `POST` opened takes data.
        let Some(upload) = uploads.get_mut(&req.path) else {
            return registry_respond(stream, 404, &[length(0)], b"");
        };
        upload.extend_from_slice(&req.body);
        if method == "PATCH" {
            let headers = [
                ("Location", req.path.clone()),
                ("Range", format!("0-{}", upload.len().saturating_sub(1))),
                length(0),
            ];
            return registry_respond(stream, 202, &headers, b"");
        }
        let digest = req
            .query
            .split('&')
            .find_map(|pair| pair.strip_prefix("digest="))
            .unwrap_or_default()
            .replace("%3A", ":");
        let data = uploads.remove(&req.path).unwrap_or_default();
        let zstd = req.content_encoding.as_deref() == Some("zstd");
        // For a transparent upload, the digest covers the decoded bytes.
        let content = if zstd && transparent {
            zstd::stream::decode_all(data.as_slice()).ok()
        } else if zstd {
            None
        } else {
            Some(data.clone())
        };
        if content.map(|c| sha256_hex(&c)) != Some(digest.clone()) {
            return registry_respond(stream, 400, &[length(0)], b"");
        }
        let mut blobs = state.blobs.lock().expect("lock");
        blobs.insert(digest.clone(), Blob { data, zstd });
        let location = format!("/v2/repo/blobs/{digest}");
        return registry_respond(stream, 201, &[("Location", location), length(0)], b"");
    }
    if let Some(digest) = req.path.strip_prefix("/v2/repo/blobs/") {
        let Some(blob) = state.blobs.lock().expect("lock").get(digest).cloned() else {
            return registry_respond(stream, 404, &[length(0)], b"");
        };
        let accepts = req
            .accept_encoding
            .is_some_and(|a| a.split(',').any(|c| c.trim() == "zstd"));
        let as_frame = blob.zstd && accepts && !state.behaviour.decode.load(Ordering::Relaxed);
        let body = if blob.zstd && !as_frame {
            match zstd::stream::decode_all(blob.data.as_slice()) {
                Ok(decoded) => decoded,
                Err(_) => return registry_respond(stream, 500, &[length(0)], b""),
            }
        } else {
            blob.data
        };
        let mut headers = vec![
            ("Content-Type", "application/octet-stream".to_string()),
            length(body.len()),
        ];
        if as_frame {
            headers.push(("Content-Encoding", "zstd".to_string()));
        }
        return registry_respond(stream, 200, &headers, if head { b"" } else { &body });
    }
    registry_respond(stream, 404, &[length(0)], b"");
}

/// Install the ring provider before building a `reqwest` client, as the `task`
/// binary does at startup for `rustls-no-provider`.
pub(crate) fn install_crypto() {
    // Fails only when a provider is already installed, which is the goal.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
