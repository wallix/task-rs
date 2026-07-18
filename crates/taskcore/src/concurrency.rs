//! A parallelism limiter for concurrent task execution.
//!
//! The Go executor uses a buffered channel as a counting semaphore to cap how
//! many tasks run at once. This ports that limiter to [`tokio::sync::Semaphore`]:
//! callers acquire a permit before running and drop it (via the returned guard)
//! when done. A limiter with no configured limit imposes no cap.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Caps the number of tasks running concurrently. Cloning shares the same
/// underlying permit pool, so clones cooperate on one global limit.
#[derive(Clone, Default)]
pub struct ConcurrencyLimiter {
    /// The permit pool, or `None` when unlimited.
    semaphore: Option<Arc<Semaphore>>,
}

impl ConcurrencyLimiter {
    /// Creates an unlimited limiter that never blocks.
    pub fn unlimited() -> Self {
        Self { semaphore: None }
    }

    /// Creates a limiter allowing at most `limit` concurrent holders. A `limit`
    /// of zero yields an unlimited limiter, matching the Go executor which
    /// leaves the semaphore unset when concurrency is not configured.
    pub fn new(limit: usize) -> Self {
        if limit == 0 {
            return Self::unlimited();
        }
        Self {
            semaphore: Some(Arc::new(Semaphore::new(limit))),
        }
    }

    /// Acquires a permit, waiting until one is free. The returned guard releases
    /// the permit when dropped. An unlimited limiter returns immediately.
    pub async fn acquire(&self) -> ConcurrencyGuard {
        match &self.semaphore {
            None => ConcurrencyGuard { _permit: None },
            Some(sem) => {
                // The semaphore is never closed, so acquisition cannot fail.
                let permit = Arc::clone(sem).acquire_owned().await.ok();
                ConcurrencyGuard { _permit: permit }
            }
        }
    }
}

/// Holds a concurrency permit for as long as it is alive. Dropping it frees the
/// slot for another waiter.
pub struct ConcurrencyGuard {
    _permit: Option<OwnedSemaphorePermit>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn unlimited_never_blocks() {
        let limiter = ConcurrencyLimiter::unlimited();
        let _a = limiter.acquire().await;
        let _b = limiter.acquire().await;
        // Reaching here means neither acquisition blocked.
    }

    #[tokio::test]
    async fn zero_limit_is_unlimited() {
        let limiter = ConcurrencyLimiter::new(0);
        let _a = limiter.acquire().await;
        let _b = limiter.acquire().await;
    }

    #[tokio::test]
    async fn caps_concurrent_holders() {
        let limiter = ConcurrencyLimiter::new(2);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let limiter = limiter.clone();
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                let _guard = limiter.acquire().await;
                let now = running.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                running.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }
}
