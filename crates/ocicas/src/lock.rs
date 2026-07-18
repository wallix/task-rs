//! A distributed build-once lock backed by a vk-registry server's HTTP lock API
//! — the same server that stores the OCI cache also hands out the lock, so a
//! deployment needs no separate Redis.
//!
//! A name-keyed lease is kept alive by a heartbeat and released only by its
//! owner, blocking on contention up to a timeout. The URL path is combined with
//! the lock name to form the server-side key.

use std::sync::Arc;
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

/// A lock client against a vk-registry `/lock` API.
pub struct Locker {
    /// `scheme://host`.
    base: String,
    /// Key prefix from the URL path.
    prefix: String,
    /// Contention timeout (`None` = [`DEFAULT_WAIT`]).
    timeout: Option<Duration>,
    /// Optional bearer token.
    token: Option<String>,
    http: reqwest::Client,
}

impl Locker {
    /// Build a locker for `scheme://host` (e.g. `http://reg:5000`) keying names
    /// under `prefix` (the URL path, empty for none). Fails only if the HTTP
    /// client cannot be built (a TLS backend that won't initialize).
    pub fn new(base: impl Into<String>, prefix: impl Into<String>) -> Result<Self> {
        // Only a connect timeout: a lock acquire is a deliberate long-poll
        // (up to POLL_CAP), so no read timeout — the per-request `.timeout()`
        // in `acquire` bounds the whole call instead.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(req_err)?;
        Ok(Locker {
            base: base.into(),
            prefix: prefix.into(),
            timeout: None,
            token: std::env::var("TASK_VK_LOCK_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            http,
        })
    }

    /// Set a custom contention timeout (`None` = default 1h).
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
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

    /// POST `/lock/{key}`. `wait` is how long the server may block before
    /// returning 409. Returns the owner on 200, `None` on 409.
    async fn acquire(&self, key: &str, wait: Duration) -> Result<Option<String>> {
        let endpoint = format!(
            "{}/lock/{}?ttl={}&wait={}",
            self.base,
            escape_path(key),
            LEASE_TTL.as_secs(),
            wait.as_secs()
        );
        let req = self
            .http
            .post(&endpoint)
            // the server holds the request up to `wait`; give the client slack.
            .timeout(wait.saturating_add(POLL_CAP))
            .header("X-Vk-Lock-Holder", holder_info(key));
        let resp = self.authorize(req).send().await.map_err(req_err)?;
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

    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// Start the heartbeat and return the held lease.
    fn hold(&self, key: String, owner: String) -> Lease {
        let stop = Arc::new(Notify::new());
        let hb = Heartbeat {
            http: self.http.clone(),
            base: self.base.clone(),
            key: key.clone(),
            owner: owner.clone(),
            token: self.token.clone(),
        };
        let stop_hb = stop.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_hb.notified() => break,
                    _ = tokio::time::sleep(HEARTBEAT_FREQ) => hb.renew().await,
                }
            }
        });
        Lease {
            http: self.http.clone(),
            base: self.base.clone(),
            key,
            owner,
            token: self.token.clone(),
            stop,
            handle: Some(handle),
        }
    }
}

/// The renew side of a held lease, owned by the heartbeat task.
struct Heartbeat {
    http: reqwest::Client,
    base: String,
    key: String,
    owner: String,
    token: Option<String>,
}

impl Heartbeat {
    async fn renew(&self) {
        let endpoint = format!(
            "{}/lock/{}/renew?ttl={}",
            self.base,
            escape_path(&self.key),
            LEASE_TTL.as_secs()
        );
        let mut req = self
            .http
            .post(&endpoint)
            .header("X-Vk-Lock-Owner", &self.owner);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        // Best-effort: a missed renew is recovered by the next tick.
        let _ = req.send().await;
    }
}

/// An acquired lock. The heartbeat renews the lease until [`Lease::unlock`],
/// which stops it and releases the lock server-side (release-if-owner). Dropping
/// without unlocking stops the heartbeat but cannot release (release is async);
/// the lease then expires server-side after [`LEASE_TTL`].
pub struct Lease {
    http: reqwest::Client,
    base: String,
    key: String,
    owner: String,
    token: Option<String>,
    stop: Arc<Notify>,
    handle: Option<JoinHandle<()>>,
}

impl Lease {
    /// Stop the heartbeat and release the lock.
    pub async fn unlock(mut self) -> Result<()> {
        self.stop.notify_one();
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
        let endpoint = format!("{}/lock/{}", self.base, escape_path(&self.key));
        let mut req = self
            .http
            .delete(&endpoint)
            .header("X-Vk-Lock-Owner", &self.owner);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await.map_err(req_err)?;
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

/// Percent-escape each path segment while keeping `/` a literal separator, so a
/// key like `task/demo:job` maps to a clean multi-segment path.
fn escape_path(key: &str) -> String {
    key.split('/')
        .map(escape_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn escape_segment(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn req_err(e: reqwest::Error) -> Error {
    Error::format(format!("vk lock: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_combines_prefix_and_name() {
        assert_eq!(make_key("task/demo", "build"), "task/demo:build");
        assert_eq!(make_key("", "build"), "build");
    }

    #[test]
    fn escapes_each_segment_keeping_slashes() {
        assert_eq!(escape_path("task/demo:job"), "task/demo%3Ajob");
        assert_eq!(escape_path("a b/c~d"), "a%20b/c~d");
        assert_eq!(escape_path("plain"), "plain");
    }
}
