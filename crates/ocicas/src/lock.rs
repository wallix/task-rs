//! A distributed build-once lock backed by a vk-registry server's HTTP lock API
//! — the same server that stores the OCI cache also hands out the lock, so a
//! deployment needs no separate Redis.
//!
//! A name-keyed lease is kept alive by a heartbeat and released only by its
//! owner, blocking on contention up to a timeout. The URL path is combined with
//! the lock name to form the server-side key.
//!
//! The wire contract is the server's, not ours — every action is a **POST** to
//! a fixed `/lock/<action>` path with the key as a repeatable `?name=` query
//! param (see vk-registry's `lock.rs` `route`, which matches those paths
//! literally and 404s anything else, and its own reference client
//! `vk_registry::client::LockClient`):
//!
//! - `POST /lock/acquire?name=…&ttl=&wait=` (`X-Vk-Lock-Holder`) → 200 `{owner}` | 409
//! - `POST /lock/renew?name=…&ttl=`         (`X-Vk-Lock-Owner`)  → 200/409 `{renewed, of}`
//! - `POST /lock/release?name=…`            (`X-Vk-Lock-Owner`)  → 200 `{released}`
//!
//! Only one name per request is ever sent here: `task` locks one cache key at a
//! time, so the server's atomic multi-name batch is unused (a single name is
//! just a one-name batch).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};

/// Lease requested on acquire; renewed by the heartbeat below.
const LEASE_TTL: Duration = Duration::from_secs(30);
/// Heartbeat interval — comfortably under the lease so a renew never races expiry.
const HEARTBEAT_FREQ: Duration = Duration::from_secs(10);
/// Default contention timeout when none is configured.
const DEFAULT_WAIT: Duration = Duration::from_secs(3600);
/// Cap on a single long-poll acquire, so contention is re-driven periodically
/// rather than as one unbounded request.
const POLL_CAP: Duration = Duration::from_secs(30);
/// Connect timeout, so a dropped SYN to the lock registry fails fast instead of
/// stalling for the OS default (~2 min) before the acquire loop can react.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on a release, which the server answers immediately — so teardown never
/// hangs on a registry that goes quiet mid-request.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// How the lock client authenticates, matching the schemes the vk-registry
/// server gates its `/lock/` API with (its `auth::Auth`: none, Basic, or a
/// static bearer token).
#[derive(Clone, Default)]
enum LockAuth {
    /// No credentials (loopback / trusted network).
    #[default]
    None,
    /// HTTP Basic — from `vk://user:pass@host/...`.
    Basic { user: String, pass: String },
    /// Static bearer token — from `$TASK_VK_LOCK_TOKEN`.
    Bearer { token: String },
}

/// A lock client against a vk-registry `/lock` API.
pub struct Locker {
    /// `scheme://host`.
    base: String,
    /// Key prefix from the URL path.
    prefix: String,
    /// Contention timeout (`None` = [`DEFAULT_WAIT`]).
    timeout: Option<Duration>,
    /// Interval between lease renewals ([`HEARTBEAT_FREQ`]; a test may shorten
    /// it to observe the heartbeat's decisions without waiting on it).
    heartbeat: Duration,
    auth: LockAuth,
    http: reqwest::Client,
}

impl Locker {
    /// Build a locker for `scheme://host` (e.g. `http://reg:5000`) keying names
    /// under `prefix` (the URL path, empty for none). Fails only if the HTTP
    /// client cannot be built (a TLS backend that won't initialize).
    ///
    /// Authentication defaults to `$TASK_VK_LOCK_TOKEN` as a bearer token when
    /// set; [`Locker::with_basic_auth`] overrides it with credentials carried
    /// in the URL.
    ///
    /// # Panics
    ///
    /// `reqwest` is built `rustls-no-provider`, so a process-wide rustls crypto
    /// provider must already be installed (the `task` binary installs ring at
    /// startup); building a client without one panics inside `reqwest`.
    pub fn new(base: impl Into<String>, prefix: impl Into<String>) -> Result<Self> {
        // Only a connect timeout: a lock acquire is a deliberate long-poll (up
        // to POLL_CAP), so no client-wide read timeout — every request sets its
        // own `.timeout()` instead, sized to what that action may legitimately
        // take.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(req_err)?;
        let auth = std::env::var("TASK_VK_LOCK_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|token| LockAuth::Bearer { token })
            .unwrap_or_default();
        Ok(Locker {
            base: base.into(),
            prefix: prefix.into(),
            timeout: None,
            heartbeat: HEARTBEAT_FREQ,
            auth,
            http,
        })
    }

    /// Set a custom contention timeout (`None` = default 1h).
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Renew at `freq` instead of every [`HEARTBEAT_FREQ`], so a test can drive
    /// the heartbeat's classification in milliseconds.
    #[cfg(test)]
    fn with_heartbeat(mut self, freq: Duration) -> Self {
        self.heartbeat = freq;
        self
    }

    /// Authenticate with `user`/`pass` (from the `cache.lock` URL) instead of
    /// the `$TASK_VK_LOCK_TOKEN` bearer default. An empty user leaves the
    /// default in place, so a credential-less URL still picks up the env token.
    pub fn with_basic_auth(mut self, user: &str, pass: Option<&str>) -> Self {
        if !user.is_empty() {
            self.auth = LockAuth::Basic {
                user: user.to_string(),
                pass: pass.unwrap_or_default().to_string(),
            };
        }
        self
    }

    /// Combine the URL path prefix with the lock name.
    fn key(&self, name: &str) -> String {
        make_key(&self.prefix, name)
    }

    /// Acquire the lock for `name`, blocking until acquired or the timeout
    /// expires. `on_contention` fires once, the first time the lock is held.
    pub async fn lock<F: FnOnce()>(&self, name: &str, on_contention: F) -> Result<Lease> {
        let key = self.key(name);
        let wait = self.timeout.unwrap_or(DEFAULT_WAIT);

        // A non-blocking probe first, so on_contention fires only on real contention.
        if let Some(owner) = self.acquire(&key, Duration::ZERO).await? {
            return Ok(self.hold(key, owner));
        }
        on_contention();

        let start = Instant::now();
        loop {
            let remaining = wait.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::format(format!("vk lock: timeout acquiring {key:?}")));
            }
            let w = remaining.min(POLL_CAP);
            if let Some(owner) = self.acquire(&key, w).await? {
                return Ok(self.hold(key, owner));
            }
        }
    }

    /// `POST /lock/acquire?name=<key>`. `wait` is how long the server may block
    /// before returning 409. Returns the owner token on 200, `None` on 409.
    async fn acquire(&self, key: &str, wait: Duration) -> Result<Option<String>> {
        let resp = authorize(
            self.http
                .post(lock_url(&self.base, "acquire"))
                .query(&[
                    ("name", key),
                    ("ttl", &LEASE_TTL.as_secs().to_string()),
                    ("wait", &wait.as_secs().to_string()),
                ])
                // the server holds the request up to `wait`; give the client slack.
                .timeout(wait.saturating_add(POLL_CAP))
                .header("X-Vk-Lock-Holder", holder_info(key)),
            &self.auth,
        )
        .send()
        .await
        .map_err(req_err)?;
        match resp.status() {
            reqwest::StatusCode::OK => {
                #[derive(Deserialize)]
                struct Body {
                    owner: String,
                }
                let body: Body = resp.json().await.map_err(req_err)?;
                Ok(Some(body.owner))
            }
            reqwest::StatusCode::CONFLICT => Ok(None),
            s => Err(Error::format(format!(
                "vk lock: acquire {key:?}: unexpected status {s}"
            ))),
        }
    }

    /// Start the heartbeat and return the held lease.
    fn hold(&self, key: String, owner: String) -> Lease {
        let stop = Arc::new(Notify::new());
        let lost = Arc::new(AtomicBool::new(false));
        let hb = Heartbeat {
            http: self.http.clone(),
            base: self.base.clone(),
            key: key.clone(),
            owner: owner.clone(),
            auth: self.auth.clone(),
        };
        let stop_hb = stop.clone();
        let lost_hb = lost.clone();
        // The last moment the server confirmed we hold the lease: the acquire
        // that just granted it, which is when the server's TTL started. Taken
        // here rather than inside the task, so a late first poll cannot push the
        // give-up deadline out.
        let mut last_ok = Instant::now();
        let freq = self.heartbeat;
        let handle = tokio::spawn(async move {
            // Fixed rate, not sleep-after-renew: the give-up rule is only
            // sampled on ticks, so a slow renew must not delay the next one.
            let mut ticker = tokio::time::interval(freq);
            ticker.tick().await; // the first tick completes immediately; skip it
            loop {
                tokio::select! {
                    _ = stop_hb.notified() => break,
                    _ = ticker.tick() => {
                        let renewed = hb.renew().await;
                        if renewed == Renewed::Held {
                            last_ok = Instant::now();
                        }
                        if lease_lost(&renewed, last_ok.elapsed()) {
                            lost_hb.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        });
        Lease {
            http: self.http.clone(),
            base: self.base.clone(),
            key,
            owner,
            auth: self.auth.clone(),
            stop,
            lost,
            handle: Some(handle),
        }
    }
}

/// The URL of one `/lock/<action>` endpoint. The action is a fixed path segment
/// — the lock name travels as a `?name=` query param, never in the path.
fn lock_url(base: &str, action: &str) -> String {
    format!("{}/lock/{action}", base.trim_end_matches('/'))
}

fn authorize(req: reqwest::RequestBuilder, auth: &LockAuth) -> reqwest::RequestBuilder {
    match auth {
        LockAuth::None => req,
        LockAuth::Basic { user, pass } => req.basic_auth(user, Some(pass)),
        LockAuth::Bearer { token } => req.bearer_auth(token),
    }
}

/// Whether the heartbeat should give the lease up, given what the latest renew
/// established and how long since the server last confirmed we hold it.
///
/// A single failed renew is not loss: the lease may well still be ours, and the
/// next tick is due long before it lapses. Only the server disowning us, or a
/// whole [`LEASE_TTL`] elapsing with no renew *confirmed*, settles it. The
/// latter is "must be assumed expired" rather than proof — a renew whose request
/// landed but whose reply was lost did extend the lease — but assuming the worst
/// is the safe direction: it withholds a cache write we may not have earned.
fn lease_lost(renewed: &Renewed, since_last_ok: Duration) -> bool {
    match renewed {
        Renewed::Held => false,
        Renewed::Lost => true,
        Renewed::Unknown => since_last_ok >= LEASE_TTL,
    }
}

/// What one renew established about the lease.
#[derive(Debug, PartialEq, Eq)]
enum Renewed {
    /// The server confirmed we still hold it.
    Held,
    /// The server disowned us — the lease lapsed and a peer may hold the name.
    Lost,
    /// Could not be established (transport error, or a status that says nothing
    /// about ownership). The lease may well still be ours.
    Unknown,
}

/// The renew side of a held lease, owned by the heartbeat task.
struct Heartbeat {
    http: reqwest::Client,
    base: String,
    key: String,
    owner: String,
    auth: LockAuth,
}

impl Heartbeat {
    /// `POST /lock/renew?name=<key>&ttl=`. The server answers 200 `{renewed,
    /// of}` when every name was renewed and 409 with the same body when some
    /// were not — for our single name that is exactly "still held" vs "lost".
    async fn renew(&self) -> Renewed {
        let sent = authorize(
            self.http
                .post(lock_url(&self.base, "renew"))
                .query(&[
                    ("name", self.key.as_str()),
                    ("ttl", &LEASE_TTL.as_secs().to_string()),
                ])
                // A renew that outlives one heartbeat interval is a lost renew,
                // not a slow one: the next tick supersedes it. Unbounded, it
                // would also stall the heartbeat task — and with it the
                // `handle.await` in `unlock` — for as long as the server keeps
                // the connection open without answering.
                .timeout(HEARTBEAT_FREQ)
                .header("X-Vk-Lock-Owner", &self.owner),
            &self.auth,
        )
        .send()
        .await;
        let Ok(resp) = sent else {
            return Renewed::Unknown;
        };
        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::CONFLICT {
            // 401/404/5xx: says nothing about whether we still own the lease.
            return Renewed::Unknown;
        }
        #[derive(Deserialize)]
        struct Body {
            renewed: usize,
        }
        // Trust the count over the status: it is the server's own answer to
        // "how many of these names are still yours".
        match resp.json::<Body>().await {
            Ok(b) if b.renewed >= 1 => Renewed::Held,
            Ok(_) => Renewed::Lost,
            // A success status we cannot parse — treat as unestablished rather
            // than dropping a lock the server may well still consider ours.
            Err(_) => Renewed::Unknown,
        }
    }
}

/// An acquired lock. The heartbeat renews the lease until [`Lease::unlock`],
/// which stops it and releases the lock server-side (release-if-owner). Dropping
/// without unlocking stops the heartbeat but cannot release (release is async);
/// the lease then expires server-side after [`LEASE_TTL`].
///
/// If the heartbeat ever establishes that the lease is gone — the server
/// disowned us, or a whole [`LEASE_TTL`] passed with no renew getting through —
/// [`Lease::is_lost`] latches true, because from that moment a peer may hold
/// the same name and whatever this holder is protecting is no longer exclusive.
/// A caller that writes shared state under the lock must check it before the
/// write.
pub struct Lease {
    http: reqwest::Client,
    base: String,
    key: String,
    owner: String,
    auth: LockAuth,
    stop: Arc<Notify>,
    lost: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Lease {
    /// Whether the lease is known to be gone (see [`Lease`]). Never un-sets:
    /// once exclusivity has been broken, later re-acquiring it would be a
    /// different hold, not this one.
    ///
    /// `false` is "no evidence of loss", not proof of holding — a renew that
    /// cannot reach the server leaves it false until the lease could have
    /// expired.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Stop the heartbeat and release the lock
    /// (`POST /lock/release?name=<key>`, release-if-owner).
    ///
    /// Errors if the server does not answer success: `/lock/` sits behind the
    /// registry's auth gate, so bad credentials — or a proxy that rewrites the
    /// path — answer 401/404 here, and reporting that as a successful release
    /// would hide a lock left held for the rest of its [`LEASE_TTL`]. A lease
    /// given up on is not released at all: the name may be someone else's by
    /// now, and a lease we did in fact still hold expires on its own.
    pub async fn unlock(mut self) -> Result<()> {
        self.stop.notify_one();
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
        // Not released: the name may be someone else's by now, and the
        // server's release-if-owner check would refuse anyway. The cost is that
        // a lease we actually still held (renew requests landing, replies lost)
        // stays locked until it expires — a bounded LEASE_TTL, and the
        // conservative side of the trade.
        if self.is_lost() {
            return Ok(());
        }
        let resp = authorize(
            self.http
                .post(lock_url(&self.base, "release"))
                .query(&[("name", self.key.as_str())])
                .timeout(RELEASE_TIMEOUT)
                .header("X-Vk-Lock-Owner", &self.owner),
            &self.auth,
        )
        .send()
        .await
        .map_err(req_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::format(format!(
                "vk lock: release {:?}: unexpected status {status}",
                self.key
            )));
        }
        // The body's `released` count would report 0 for a lease that had
        // already lapsed, but release runs at teardown — after everything
        // `is_lost` guards — so it is a diagnostic, not a signal, and is not
        // read here.
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

/// The identity served on the lock: a single line (it travels in an HTTP header).
fn holder_info(key: &str) -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let acquired = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "pid={}; host={host}; key={key}; acquired={acquired}",
        std::process::id()
    )
}

/// Combine the URL path prefix with the lock name (redis-locker compatible).
fn make_key(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}:{name}")
    }
}

fn req_err(e: reqwest::Error) -> Error {
    Error::format(format!("vk lock: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;

    #[test]
    fn key_combines_prefix_and_name() {
        assert_eq!(make_key("task/demo", "build"), "task/demo:build");
        assert_eq!(make_key("", "build"), "build");
    }

    #[test]
    fn lock_url_is_a_fixed_action_path() {
        // The action is the path; the key never appears in it (it goes as
        // `?name=`). A base with a trailing slash must not double it.
        assert_eq!(
            lock_url("http://reg:5000", "acquire"),
            "http://reg:5000/lock/acquire"
        );
        assert_eq!(
            lock_url("http://reg:5000/", "release"),
            "http://reg:5000/lock/release"
        );
    }

    /// One request as the fake server saw it.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        /// Path only, without the query string.
        path: String,
        /// Raw query string.
        query: String,
        holder: Option<String>,
        owner: Option<String>,
        authorization: Option<String>,
    }

    /// A fake vk-registry lock endpoint: routes exactly like the real server
    /// (`vk-registry/src/lock.rs` `route`) — POST-only, a literal match on
    /// `/lock/<action>`, names as `?name=` — and 404s anything else, so a client
    /// that talks the wrong protocol fails here exactly as it would in
    /// production. Records every request for assertions.
    struct FakeLockServer {
        addr: std::net::SocketAddr,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl FakeLockServer {
        /// Serves `responses.len()` requests, one per entry, as
        /// `(status, body)`; then stops.
        fn start(responses: Vec<(u16, &'static str)>) -> Self {
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
            FakeLockServer { addr, seen }
        }

        fn base(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().expect("lock").clone()
        }
    }

    /// Read one HTTP/1.1 request (headers only — these requests carry no body),
    /// record it, then write back a fixed response.
    ///
    /// The request is recorded *before* the response goes out, so a client that
    /// has received its reply is guaranteed to be visible in `seen()`. Recording
    /// afterwards races the client (which may return, and the test assert, first).
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

    /// reqwest is built with `rustls-no-provider`; the `task` binary installs
    /// the ring provider at startup, so a test that builds a client must too.
    fn install_crypto() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn locker(base: String) -> Locker {
        install_crypto();
        Locker::new(base, "task/demo").expect("locker")
    }

    /// One renew against a stub, for the classification table below.
    async fn renew_once(status: u16, body: &'static str) -> Renewed {
        install_crypto();
        let server = FakeLockServer::start(vec![(status, body)]);
        Heartbeat {
            http: reqwest::Client::new(),
            base: server.base(),
            key: "k".to_string(),
            owner: "owner-token".to_string(),
            auth: LockAuth::None,
        }
        .renew()
        .await
    }

    /// How a renew's reply maps onto "do we still hold this?". Getting `Lost`
    /// wrong in either direction is a correctness bug: a false `Lost` throws
    /// away a good build, a missed one lets two runs write the same cache entry.
    #[tokio::test]
    async fn renew_classifies_the_servers_answer() {
        // The server renewed every name we asked about.
        assert_eq!(
            renew_once(200, r#"{"renewed":1,"of":1}"#).await,
            Renewed::Held
        );
        // It disowned us — 409 is how the server reports a partial batch, which
        // for our single name means the lease is gone.
        assert_eq!(
            renew_once(409, r#"{"renewed":0,"of":1}"#).await,
            Renewed::Lost
        );
        // Says nothing about ownership: not evidence of loss.
        assert_eq!(
            renew_once(401, r#"{"error":"unauthorized"}"#).await,
            Renewed::Unknown
        );
        assert_eq!(renew_once(503, "").await, Renewed::Unknown);
        // A success we cannot parse must not be read as loss.
        assert_eq!(renew_once(200, "not json").await, Renewed::Unknown);
    }

    /// An unreachable server is `Unknown`, never `Lost` — the lease may still
    /// be ours, and only elapsed time can settle it.
    #[tokio::test]
    async fn renew_against_an_unreachable_server_is_unknown() {
        install_crypto();
        // Bind and drop, so the port is (almost certainly) closed.
        let addr = TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr");
        let got = Heartbeat {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(200))
                .build()
                .expect("client"),
            base: format!("http://{addr}"),
            key: "k".to_string(),
            owner: "o".to_string(),
            auth: LockAuth::None,
        }
        .renew()
        .await;
        assert_eq!(got, Renewed::Unknown);
    }

    /// When the heartbeat gives the lease up. The `Unknown` row is the one that
    /// matters: a transient failure must not drop a lock we still hold, but once
    /// the lease could have expired we must stop claiming exclusivity.
    #[test]
    fn lease_is_given_up_only_when_settled() {
        let short = LEASE_TTL / 2;
        // Confirmed held — never lost, however long ago the previous confirmation.
        assert!(!lease_lost(&Renewed::Held, short));
        assert!(!lease_lost(&Renewed::Held, LEASE_TTL * 2));
        // Disowned — lost immediately, even if we renewed a moment ago.
        assert!(lease_lost(&Renewed::Lost, Duration::ZERO));
        // Unable to tell: hold on until the lease could not have survived.
        assert!(!lease_lost(&Renewed::Unknown, Duration::ZERO));
        assert!(!lease_lost(&Renewed::Unknown, short));
        assert!(lease_lost(&Renewed::Unknown, LEASE_TTL));
        assert!(lease_lost(&Renewed::Unknown, LEASE_TTL * 2));
    }

    /// A freshly acquired lease is held, and reports so.
    #[tokio::test]
    async fn a_fresh_lease_is_not_lost() {
        let server = FakeLockServer::start(vec![(200, r#"{"owner":"owner-token"}"#)]);
        let lease = locker(server.base())
            .lock("build", || {})
            .await
            .expect("acquire");
        assert!(!lease.is_lost());
    }

    /// The whole lifecycle against the real endpoint shapes: acquire, then
    /// release. Each must be a POST to its fixed `/lock/<action>` path with the
    /// key as `?name=` — the contract the previous implementation got wrong
    /// (it used `POST /lock/{key}` and `DELETE /lock/{key}`, which the real
    /// server 404s).
    #[tokio::test]
    async fn acquire_and_release_use_the_server_endpoints() {
        let server = FakeLockServer::start(vec![
            (
                200,
                r#"{"owner":"owner-token","names":["task/demo:build"],"ttl":30}"#,
            ),
            (200, r#"{"released":1}"#),
        ]);
        let lease = locker(server.base())
            .lock("build", || {
                panic!("uncontended lock must not report contention")
            })
            .await
            .expect("acquire");
        lease.unlock().await.expect("release");

        let seen = server.seen();
        assert_eq!(seen.len(), 2, "{seen:#?}");

        let acquire = &seen[0];
        assert_eq!(acquire.method, "POST");
        assert_eq!(acquire.path, "/lock/acquire");
        assert!(
            acquire.query.contains("name=task%2Fdemo%3Abuild"),
            "the key travels as a percent-encoded ?name=: {:?}",
            acquire.query
        );
        assert!(acquire.query.contains("ttl=30"), "{:?}", acquire.query);
        assert!(acquire.query.contains("wait=0"), "{:?}", acquire.query);
        assert!(acquire.holder.is_some(), "acquire sends X-Vk-Lock-Holder");
        assert_eq!(acquire.owner, None);

        let release = &seen[1];
        assert_eq!(release.method, "POST", "release is POST, not DELETE");
        assert_eq!(release.path, "/lock/release");
        assert!(
            release.query.contains("name=task%2Fdemo%3Abuild"),
            "{:?}",
            release.query
        );
        assert_eq!(
            release.owner.as_deref(),
            Some("owner-token"),
            "release presents the owner token back"
        );
    }

    /// A 409 is contention, not an error: `lock` reports it through
    /// `on_contention` and keeps polling rather than failing.
    #[tokio::test]
    async fn contention_retries_until_granted() {
        let server = FakeLockServer::start(vec![
            (
                409,
                r#"{"error":"locks held","blockers":[{"name":"task/demo:build","holder":"peer"}]}"#,
            ),
            (200, r#"{"owner":"owner-token"}"#),
        ]);
        let contended = Arc::new(Mutex::new(false));
        let flag = contended.clone();
        let lease = locker(server.base())
            .with_timeout(Some(Duration::from_secs(30)))
            .lock("build", move || *flag.lock().expect("lock") = true)
            .await
            .expect("acquire after contention");
        assert!(
            *contended.lock().expect("lock"),
            "409 must fire on_contention"
        );
        // Two acquires: the zero-wait probe (409) then the long-poll (200).
        let seen = server.seen();
        assert_eq!(seen.len(), 2, "{seen:#?}");
        assert!(seen.iter().all(|s| s.path == "/lock/acquire"));
        // Dropping (rather than unlocking) must not call release.
        drop(lease);
    }

    /// An unexpected status — what the old wrong-endpoint client got back from
    /// every request — is a hard error, not silently treated as contention.
    #[tokio::test]
    async fn unknown_action_404_is_an_error() {
        let server = FakeLockServer::start(vec![(404, r#"{"error":"unknown lock action"}"#)]);
        // `Lease` is not `Debug` (it holds a client and a task handle), so
        // unwrap the error by hand rather than through `expect_err`.
        let Err(err) = locker(server.base()).lock("build", || {}).await else {
            panic!("404 must fail");
        };
        assert!(err.to_string().contains("404"), "{err}");
    }

    /// Renew must be a POST to `/lock/renew` with the key as `?name=` and the
    /// owner token — not the old `POST /lock/{key}/renew`, which the real
    /// server 404s. Driven through `Heartbeat` directly: [`HEARTBEAT_FREQ`] is
    /// far too long to wait for the spawned task to tick.
    #[tokio::test]
    async fn renew_uses_the_server_endpoint() {
        let server = FakeLockServer::start(vec![(200, r#"{"renewed":1,"of":1}"#)]);
        let l = locker(server.base());
        Heartbeat {
            http: l.http.clone(),
            base: l.base.clone(),
            key: l.key("build"),
            owner: "owner-token".to_string(),
            auth: LockAuth::None,
        }
        .renew()
        .await;

        let seen = server.seen();
        assert_eq!(seen.len(), 1, "{seen:#?}");
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/lock/renew");
        assert!(
            seen[0].query.contains("name=task%2Fdemo%3Abuild"),
            "{:?}",
            seen[0].query
        );
        assert!(seen[0].query.contains("ttl=30"), "{:?}", seen[0].query);
        assert_eq!(seen[0].owner.as_deref(), Some("owner-token"));
    }

    /// A release the server refuses — here the 401 an expired credential gets
    /// from the registry's auth gate — must surface, not be reported as a
    /// successful unlock.
    #[tokio::test]
    async fn release_reports_a_refused_status() {
        let server = FakeLockServer::start(vec![
            (200, r#"{"owner":"owner-token"}"#),
            (401, r#"{"error":"unauthorized"}"#),
        ]);
        let lease = locker(server.base())
            .lock("build", || {})
            .await
            .expect("acquire");
        let Err(err) = lease.unlock().await else {
            panic!("a refused release must fail");
        };
        assert!(err.to_string().contains("401"), "{err}");
    }

    /// The latch, end to end: a server that disowns the lease on renew makes
    /// `is_lost()` true with no action from the holder — and the heartbeat then
    /// stops, so no further renews go out.
    #[tokio::test]
    async fn the_heartbeat_latches_a_disowned_lease() {
        let server = FakeLockServer::start(vec![
            (200, r#"{"owner":"owner-token"}"#),
            (409, r#"{"renewed":0,"of":1}"#),
            // Deliberately never served: the fake stops accepting once its
            // responses run out, so without a spare entry a renew after the
            // latch would be refused rather than recorded, and the count below
            // could not tell a stopped heartbeat from a running one.
            (200, r#"{"renewed":1,"of":1}"#),
        ]);
        let lease = locker(server.base())
            .with_heartbeat(Duration::from_millis(10))
            .lock("build", || {})
            .await
            .expect("acquire");
        for _ in 0..200 {
            if lease.is_lost() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            lease.is_lost(),
            "a 409 renew must latch the lease as lost: {:#?}",
            server.seen()
        );
        // Acquire + the one renew that disowned us, and nothing after it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.seen().len(), 2, "{:#?}", server.seen());
    }

    /// A lease given up on sends no release: the name may be someone else's,
    /// and `unlock` must still report success (there is nothing left to fail).
    #[tokio::test]
    async fn a_lost_lease_is_not_released() {
        let server = FakeLockServer::start(vec![(200, r#"{"owner":"owner-token"}"#)]);
        let lease = locker(server.base())
            .lock("build", || {})
            .await
            .expect("acquire");
        lease.lost.store(true, Ordering::SeqCst);
        lease.unlock().await.expect("a lost lease unlocks cleanly");
        assert_eq!(
            server.seen().len(),
            1,
            "only the acquire went out; no release follows a lost lease"
        );
    }

    /// URL-carried Basic credentials reach the wire; without them the bearer
    /// default applies.
    #[tokio::test]
    async fn basic_auth_is_sent_when_configured() {
        let server = FakeLockServer::start(vec![(200, r#"{"owner":"o"}"#)]);
        let lease = locker(server.base())
            .with_basic_auth("ci", Some("s3cret"))
            .lock("build", || {})
            .await
            .expect("acquire");
        let auth = server.seen()[0].authorization.clone();
        // base64("ci:s3cret")
        assert_eq!(auth.as_deref(), Some("Basic Y2k6czNjcmV0"));
        drop(lease);
    }

    #[test]
    fn empty_basic_user_keeps_the_default_auth() {
        install_crypto();
        let l = Locker::new("http://x", "")
            .expect("locker")
            .with_basic_auth("", None);
        assert!(matches!(l.auth, LockAuth::None | LockAuth::Bearer { .. }));
    }
}
