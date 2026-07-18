//! Redis-backed distributed `cache.lock`.
//!
//! Ports the Go implementation: acquire with `SET key owner NX EX 30`, keep the
//! key alive with a 10s heartbeat (`EXPIRE` guarded by an owner check), and
//! release with an owner-checked `DEL`. Both the renew and release run as Lua so
//! a lock is only ever renewed or deleted by its actual owner. Acquisition
//! blocks, retrying every 5s until the contention timeout; repeated connection
//! failures give up after 30s so the caller can fall back to a local lock.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;

use super::error::CacheError;

/// Opens a single (non-multiplexed) async connection. The multiplexed connection
/// spawns a background driver that starves on the engine's current-thread
/// runtime (a ~40s stall), so a plain connection is used instead — which also
/// mirrors Go, where every lock operation dials its own connection.
#[allow(deprecated)]
async fn open(info: &redis::ConnectionInfo) -> Result<redis::aio::Connection, redis::RedisError> {
    let client = redis::Client::open(info.clone())?;
    client.get_async_connection().await
}

/// [`open`] bounded by [`CONNECT_TIMEOUT`], returning `None` on connect error or
/// timeout — for the heartbeat, whose renewals are best-effort and must never
/// stall on a dropped SYN (`get_async_connection` has no built-in bound).
#[allow(deprecated)]
async fn open_bounded(info: &redis::ConnectionInfo) -> Option<redis::aio::Connection> {
    tokio::time::timeout(CONNECT_TIMEOUT, open(info))
        .await
        .ok()
        .and_then(Result::ok)
}

/// Lock TTL in seconds; refreshed by the heartbeat.
const LOCK_TTL_SECS: usize = 30;
/// Interval between heartbeat renewals.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// Delay between acquire attempts while contended or disconnected.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Default maximum wait for a contended lock (1 hour).
const DEFAULT_ACQUIRE_MAX: Duration = Duration::from_secs(3600);
/// How long to keep retrying connection failures before giving up.
const CONNECT_GIVE_UP: Duration = Duration::from_secs(30);
/// Connect timeout for a single acquire attempt: `get_async_connection` has no
/// built-in bound, so a dropped SYN would stall ~2 min per attempt and defeat
/// the `CONNECT_GIVE_UP` budget above.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Connection timeout for the blocking release in `Drop`, so a slow or
/// unreachable Redis cannot stall task teardown.
const RELEASE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Release the lock only if we still own it.
const RELEASE_LUA: &str = r#"if redis.call("get",KEYS[1])==ARGV[1] then return redis.call("del",KEYS[1]) else return 0 end"#;
/// Renew the TTL only if we still own it.
const RENEW_LUA: &str = r#"if redis.call("get",KEYS[1])==ARGV[1] then return redis.call("expire",KEYS[1],ARGV[2]) else return 0 end"#;

/// Connection parameters plus the key prefix (the URL path) for a redis lock.
pub struct RedisLocker {
    conn_info: redis::ConnectionInfo,
    key_prefix: String,
    acquire_max: Duration,
}

impl RedisLocker {
    /// Builds a locker from the `redis://[user:pass@]host[:port]/prefix` parts.
    pub fn new(
        host: &str,
        username: &str,
        password: Option<String>,
        path: &str,
        timeout: Option<Duration>,
    ) -> Self {
        let (host, port) = split_host_port(host);
        let conn_info = redis::ConnectionInfo {
            addr: redis::ConnectionAddr::Tcp(host, port),
            redis: redis::RedisConnectionInfo {
                username: (!username.is_empty()).then(|| username.to_string()),
                password,
                ..Default::default()
            },
        };
        Self {
            conn_info,
            key_prefix: path.trim_start_matches('/').to_string(),
            acquire_max: timeout.unwrap_or(DEFAULT_ACQUIRE_MAX),
        }
    }

    /// The Redis key for `name`: the URL path prefix joined with `name` by `:`
    /// (matching Go's URL-path append).
    fn key(&self, name: &str) -> String {
        if self.key_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}:{}", self.key_prefix, name)
        }
    }

    /// Acquires the lock for `name`, blocking until held or the timeout expires.
    /// `on_contention` fires once, the first time the lock is found held.
    pub async fn lock<F: FnOnce()>(
        &self,
        name: &str,
        on_contention: F,
    ) -> Result<RedisGuard, CacheError> {
        let key = self.key(name);
        if key.is_empty() {
            return Err(CacheError::msg("redis lock: empty key"));
        }
        let owner = format!("{}:{}", std::process::id(), now_nanos());

        let contention_deadline = Instant::now().checked_add(self.acquire_max);
        let mut connect_deadline = Instant::now().checked_add(CONNECT_GIVE_UP);
        let mut notify: Option<F> = Some(on_contention);

        loop {
            let mut conn = match tokio::time::timeout(CONNECT_TIMEOUT, open(&self.conn_info)).await
            {
                Ok(Ok(conn)) => {
                    // Only consecutive failures count toward the connect budget.
                    connect_deadline = Instant::now().checked_add(CONNECT_GIVE_UP);
                    conn
                }
                // A connect error, or a connect that timed out, both count as a
                // failure: retry until the connect budget is exhausted, then give
                // up so the caller can fall back to a local lock.
                res => {
                    if reached(connect_deadline) {
                        return Err(match res {
                            Ok(Err(e)) => map_err("connect", &key, e),
                            _ => CacheError::msg(format!(
                                "redis lock connect for {key:?}: timed out after {CONNECT_TIMEOUT:?}"
                            )),
                        });
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                    continue;
                }
            };

            let reply: redis::Value = redis::cmd("SET")
                .arg(&key)
                .arg(&owner)
                .arg("NX")
                .arg("EX")
                .arg(LOCK_TTL_SECS)
                .query_async(&mut conn)
                .await
                .map_err(|e| map_err("SET NX", &key, e))?;

            // A nil reply means the key already existed (held by someone else);
            // anything else is the "OK" acquisition.
            if !matches!(reply, redis::Value::Nil) {
                let (stop_tx, stop_rx) = oneshot::channel();
                tokio::spawn(heartbeat(
                    self.conn_info.clone(),
                    key.clone(),
                    owner.clone(),
                    stop_rx,
                ));
                return Ok(RedisGuard {
                    conn_info: self.conn_info.clone(),
                    key,
                    owner,
                    stop: Some(stop_tx),
                });
            }

            if let Some(cb) = notify.take() {
                cb();
            }
            if reached(contention_deadline) {
                return Err(CacheError::msg(format!(
                    "redis lock: timeout acquiring {key:?}"
                )));
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }
}

/// A held redis lock. Dropping stops the heartbeat and eagerly deletes the key
/// (owner-checked), so the lock is released as soon as the guard goes away.
pub struct RedisGuard {
    conn_info: redis::ConnectionInfo,
    key: String,
    owner: String,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for RedisGuard {
    fn drop(&mut self) {
        // End the heartbeat task, then release the lock eagerly with an
        // owner-checked DEL — matching Go's synchronous release. Drop cannot
        // await, so a short blocking connection is used; it is time-bounded and
        // best-effort, and the TTL reclaims the key if the release cannot run.
        drop(self.stop.take());
        let _ = release_blocking(&self.conn_info, &self.key, &self.owner);
    }
}

/// Owner-checked `DEL` over a short-lived blocking connection, for [`Drop`].
fn release_blocking(
    info: &redis::ConnectionInfo,
    key: &str,
    owner: &str,
) -> Result<(), redis::RedisError> {
    let client = redis::Client::open(info.clone())?;
    let mut conn = client.get_connection_with_timeout(RELEASE_CONNECT_TIMEOUT)?;
    redis::cmd("EVAL")
        .arg(RELEASE_LUA)
        .arg(1)
        .arg(key)
        .arg(owner)
        .query::<i64>(&mut conn)?;
    Ok(())
}

/// Periodically renews the lock TTL until the guard signals stop. Reuses one
/// connection, reopening it if a renewal fails.
async fn heartbeat(
    conn_info: redis::ConnectionInfo,
    key: String,
    owner: String,
    mut stop: oneshot::Receiver<()>,
) {
    let mut conn = open_bounded(&conn_info).await;
    let mut ticker = tokio::time::interval(HEARTBEAT);
    ticker.tick().await; // the first tick completes immediately; skip it
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                if conn.is_none() {
                    conn = open_bounded(&conn_info).await;
                }
                let Some(c) = conn.as_mut() else { continue };
                // Best-effort: a failed renewal drops the connection so the next
                // tick reopens it; if the key already lapsed it just stays gone.
                let renewed: Result<i64, _> = redis::cmd("EVAL")
                    .arg(RENEW_LUA)
                    .arg(1)
                    .arg(&key)
                    .arg(&owner)
                    .arg(LOCK_TTL_SECS)
                    .query_async(c)
                    .await;
                if renewed.is_err() {
                    conn = None;
                }
            }
        }
    }
}

/// Splits a `host` or `host:port` (and bracketed IPv6) into host and port,
/// defaulting to 6379.
fn split_host_port(host: &str) -> (String, u16) {
    if let Some(rest) = host.strip_prefix('[') {
        // Bracketed IPv6: [::1] or [::1]:6379.
        if let Some((addr, port)) = rest.split_once("]:") {
            return (addr.to_string(), port.parse().unwrap_or(6379));
        }
        return (rest.trim_end_matches(']').to_string(), 6379);
    }
    match host.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(6379)),
        None => (host.to_string(), 6379),
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Whether `deadline` (if representable) has been reached.
fn reached(deadline: Option<Instant>) -> bool {
    deadline.map(|d| Instant::now() >= d).unwrap_or(false)
}

fn map_err(op: &str, key: &str, e: redis::RedisError) -> CacheError {
    CacheError::msg(format!("redis lock {op} for {key:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_host_port_cases() {
        assert_eq!(split_host_port("h"), ("h".to_string(), 6379));
        assert_eq!(split_host_port("h:6380"), ("h".to_string(), 6380));
        assert_eq!(split_host_port("[::1]:6381"), ("::1".to_string(), 6381));
        assert_eq!(split_host_port("[::1]"), ("::1".to_string(), 6379));
    }

    #[test]
    fn key_joins_prefix_and_name() {
        let l = RedisLocker::new("h", "", None, "/locks", None);
        assert_eq!(l.key("t:hash"), "locks:t:hash");
        // An empty path prefix uses the name alone.
        let l = RedisLocker::new("h", "", None, "/", None);
        assert_eq!(l.key("t"), "t");
    }

    /// Live acquire/exclusion/release against the Redis at `$TASK_TEST_REDIS_ADDR`
    /// (`host:port`). Skipped when the variable is unset so CI without a Redis
    /// stays green.
    #[tokio::test]
    async fn mutual_exclusion_and_release() {
        let Ok(addr) = std::env::var("TASK_TEST_REDIS_ADDR") else {
            return;
        };
        let name = format!("excl-{}", now_nanos());
        let locker = RedisLocker::new(&addr, "", None, "/task-test-lock", None);

        let held = locker.lock(&name, || {}).await.expect("first acquire");

        // A second acquire with a short timeout must fail while the first holds it.
        let contended = RedisLocker::new(
            &addr,
            "",
            None,
            "/task-test-lock",
            Some(Duration::from_millis(1)),
        );
        assert!(
            contended.lock(&name, || {}).await.is_err(),
            "second acquire should time out while held"
        );

        // Dropping releases the lock (owner-checked DEL) and stops the heartbeat.
        drop(held);

        // After release the lock is acquirable again.
        drop(
            locker
                .lock(&name, || {})
                .await
                .expect("re-acquire after release"),
        );
    }
}
