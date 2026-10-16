//! Runs fingerprint readings off the engine's thread.
//!
//! A long walk or hash on the single-threaded engine would stall other tasks,
//! including the interrupt watcher. Read-only jobs run on the blocking pool,
//! acquiring the process-wide hashing budget ([`super::pool::budget`]) before
//! spawning. They neither exceed that budget nor wait for it inside the pool.
//! The engine uses their results to write fingerprint state and publish cache
//! entries.
//!
//! Dropping the awaiting future cancels the job at its next entry, directory
//! or read and discards its result. Blocked system calls are not interrupted.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::pool::budget;

thread_local! {
    /// The cancellation flag of the job running on this thread.
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// The current job's cancellation flag, for threads it spawns.
pub(crate) fn cancel_flag() -> Option<Arc<AtomicBool>> {
    CANCEL.with(|c| c.borrow().clone())
}

/// Whether the job running on this thread was cancelled.
pub(crate) fn cancelled() -> bool {
    CANCEL.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    })
}

/// The error a cancelled reading ends with. Nobody awaits it.
pub(crate) fn interrupted() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "fingerprinting cancelled")
}

/// Cancels the job when the future awaiting it is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Clears this thread's flag when the job ends, panicking or not: pool
/// threads are reused.
struct ClearOnDrop;

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        CANCEL.with(|c| *c.borrow_mut() = None);
    }
}

/// Runs the read-only `job` on the blocking pool once the hashing budget
/// admits it, or inline outside a runtime. A panic in `job` resumes here.
pub(crate) async fn off_thread<T, F>(job: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return job();
    }
    // The job's own thread counts against the budget; its helpers take what
    // is left without waiting.
    let permit = budget().acquire().await.map_err(std::io::Error::other)?;
    let flag = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&flag));
    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if flag.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        CANCEL.with(|c| *c.borrow_mut() = Some(Arc::clone(&flag)));
        let _clear = ClearOnDrop;
        let result = job();
        if flag.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        result
    });
    match handle.await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(std::io::Error::other(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Readings complete on a pool smaller than the number of readings, and
    /// on more readings than the budget admits at once.
    #[test]
    fn completes_on_a_one_thread_pool() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        let sum = local.block_on(&rt, async {
            let jobs = (0..64u64).map(|i| off_thread(move || Ok(i)));
            let mut sum = 0;
            for r in futures_join(jobs.collect()).await {
                sum += r.unwrap();
            }
            sum
        });
        assert_eq!(sum, (0..64).sum::<u64>());
    }

    async fn futures_join<F>(fs: Vec<F>) -> Vec<F::Output>
    where
        F: std::future::Future + 'static,
        F::Output: 'static,
    {
        let handles: Vec<_> = fs.into_iter().map(tokio::task::spawn_local).collect();
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }

    /// A job dropped before it starts never runs; one dropped while running
    /// sees the flag.
    #[test]
    fn dropped_jobs_are_cancelled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let saw_cancel = Arc::new(AtomicBool::new(false));
        rt.block_on(async {
            let (ran2, saw) = (Arc::clone(&ran), Arc::clone(&saw_cancel));
            // Occupies the only pool thread until cancelled.
            let busy = off_thread(move || {
                ran2.fetch_add(1, Ordering::SeqCst);
                while !cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                saw.store(true, Ordering::SeqCst);
                Ok(())
            });
            let mut busy = std::pin::pin!(busy);
            // Started, so the pool's only thread is taken.
            while ran.load(Ordering::SeqCst) == 0 {
                let _ = tokio::time::timeout(Duration::from_millis(1), busy.as_mut()).await;
            }
            let ran3 = Arc::clone(&ran);
            let queued = off_thread(move || {
                ran3.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            let both = async { tokio::join!(busy.as_mut(), queued) };
            assert!(
                tokio::time::timeout(Duration::from_millis(50), both)
                    .await
                    .is_err()
            );
        });
        drop(rt);
        assert!(saw_cancel.load(Ordering::SeqCst));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runs_inline_outside_a_runtime() {
        assert_eq!(
            crate::fingerprint::testutil::now(off_thread(|| Ok(7))).unwrap(),
            7
        );
    }
}
