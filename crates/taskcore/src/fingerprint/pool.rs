//! Digests a checksum's entries on a bounded set of threads.
//!
//! Each entry is digested independently and the digests are combined in
//! entry order afterwards, so the checksum does not depend on how many
//! threads ran or in which order they finished. All the hashing in the
//! process shares one budget of [`parallelism`] threads.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Semaphore, SemaphorePermit};

use super::checksum::{BUF_LEN, Entry, Kind, digest};

/// Entries below this many per thread are not worth another thread.
const PER_THREAD: usize = 64;

/// How many threads may hash at once, process-wide.
pub(crate) fn parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// The process-wide hashing budget: one permit per hashing thread.
pub(crate) fn budget() -> &'static Semaphore {
    static BUDGET: OnceLock<Semaphore> = OnceLock::new();
    BUDGET.get_or_init(|| Semaphore::new(parallelism()))
}

/// How many threads digest a set of entries.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Workers {
    /// The calling thread, plus the helpers the budget has free for the job.
    Auto,
    /// Exactly this many, outside the budget (tests).
    #[cfg(test)]
    Exactly(usize),
}

/// Digests `entries`, returning their kinds and digests in order, or the
/// error of the first failing entry in that order.
pub(crate) fn digest_all(
    entries: &[Entry],
    workers: Workers,
) -> std::io::Result<Vec<(Kind, u128)>> {
    digest_in(entries, workers, &|i, _| i)
}

/// [`digest_all`], claiming the `i`th entry handed out as `order(i, len)`: a
/// permutation, so tests can vary the completion order.
pub(crate) fn digest_in(
    entries: &[Entry],
    workers: Workers,
    order: &(dyn Fn(usize, usize) -> usize + Sync),
) -> std::io::Result<Vec<(Kind, u128)>> {
    let wanted = entries.len().div_ceil(PER_THREAD).clamp(1, parallelism());
    let (threads, _helpers) = match workers {
        Workers::Auto => {
            let helpers = take_helpers(wanted.saturating_sub(1));
            let n = helpers.as_ref().map_or(0, |p| p.num_permits());
            (n.saturating_add(1), helpers)
        }
        #[cfg(test)]
        Workers::Exactly(n) => (n.max(1), None),
    };

    // Helper threads do not see the job's thread-local flag: hand it over.
    let cancel = super::offload::cancel_flag();
    let cancel = cancel.as_deref();
    let next = AtomicUsize::new(0);
    // The lowest failing index so far: entries above it cannot change the
    // result, entries below it are still digested.
    let first_err = AtomicUsize::new(usize::MAX);
    let work = || {
        let mut buf = vec![0u8; BUF_LEN];
        let mut out = Vec::new();
        loop {
            let claim = next.fetch_add(1, Ordering::Relaxed);
            if claim >= entries.len() || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                break;
            }
            let i = order(claim, entries.len());
            let Some(e) = entries.get(i) else { continue };
            if i > first_err.load(Ordering::Relaxed) {
                continue;
            }
            let r = digest(&e.path, &mut buf, cancel);
            if r.is_err() {
                first_err.fetch_min(i, Ordering::Relaxed);
            }
            out.push((i, r));
        }
        out
    };

    let results = if threads <= 1 {
        work()
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = (1..threads)
                .filter_map(|_| std::thread::Builder::new().spawn_scoped(s, work).ok())
                .collect();
            let mut all = work();
            for h in handles {
                match h.join() {
                    Ok(part) => all.extend(part),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            all
        })
    };

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(super::offload::interrupted());
    }
    let mut slots: Vec<Option<std::io::Result<(Kind, u128)>>> = std::iter::repeat_with(|| None)
        .take(entries.len())
        .collect();
    for (i, r) in results {
        if let Some(slot) = slots.get_mut(i) {
            *slot = Some(r);
        }
    }
    let mut out = Vec::with_capacity(entries.len());
    for slot in slots {
        match slot {
            Some(Ok(d)) => out.push(d),
            Some(Err(e)) => return Err(e),
            // Only entries above a failure are skipped, and that failure
            // came first.
            None => return Err(std::io::Error::other("fingerprint: entry not digested")),
        }
    }
    Ok(out)
}

/// Takes up to `n` helper permits without waiting: as many as are free.
fn take_helpers(n: usize) -> Option<SemaphorePermit<'static>> {
    let free = budget().available_permits().min(n);
    (1..=free)
        .rev()
        .find_map(|k| budget().try_acquire_many(u32::try_from(k).ok()?).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::checksum::{Datum, checksum_files_with, entries};
    use crate::fingerprint::testutil::{join, tmp, write_file};
    use twox_hash::XxHash3_128;

    /// A seeded xorshift, for reproducible trees.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// A tree of `n` files of varied sizes (empty ones included) and a few
    /// symlinks, returned as glob results.
    fn tree(seed: u64, n: usize) -> (String, Vec<String>) {
        let dir = tmp();
        let mut rng = Rng(seed);
        let mut files = Vec::new();
        for i in 0..n {
            let rel = format!("d{}/f{i}", rng.next() % 7);
            let len = (rng.next() % 3000) as usize;
            let body: String = (0..len)
                .map(|j| char::from(b'a' + ((i + j) % 26) as u8))
                .collect();
            write_file(&dir, &rel, &body);
            files.push(join(&dir, &rel));
        }
        #[cfg(unix)]
        for i in 0..3 {
            let link = join(&dir, &format!("l{i}"));
            std::os::unix::fs::symlink(format!("d0/f{i}"), &link).unwrap();
            files.push(link);
        }
        (dir, files)
    }

    /// Format v1 written out serially from its description, independently of
    /// `checksum.rs`.
    fn reference(dir: &str, files: &[String]) -> String {
        let mut keyed: Vec<(String, String)> = files
            .iter()
            .map(|f| {
                (
                    f.strip_prefix(&format!("{dir}/")).unwrap().to_string(),
                    f.clone(),
                )
            })
            .collect();
        keyed.sort();
        let mut h = XxHash3_128::new();
        h.write(b"task-rs/fingerprint/v1\0");
        h.write(&(keyed.len() as u64).to_le_bytes());
        for (key, path) in &keyed {
            let meta = std::fs::symlink_metadata(path).unwrap();
            let (kind, d) = if meta.file_type().is_symlink() {
                let t = std::fs::read_link(path).unwrap();
                (b'L', XxHash3_128::oneshot(t.to_str().unwrap().as_bytes()))
            } else {
                (b'F', XxHash3_128::oneshot(&std::fs::read(path).unwrap()))
            };
            h.write(b"R");
            h.write(&(key.len() as u64).to_le_bytes());
            h.write(key.as_bytes());
            h.write(&[kind]);
            h.write(&d.to_be_bytes());
        }
        h.write(&0u64.to_le_bytes());
        format!("{:032x}", h.finish_128())
    }

    #[test]
    fn thread_count_does_not_change_the_checksum() {
        for seed in [1, 7, 42] {
            let (dir, files) = tree(seed, 300);
            let want = reference(&dir, &files);
            for n in [1, 2, 3, 8, 64] {
                assert_eq!(
                    checksum_files_with(&dir, &files, &[], Workers::Exactly(n)).unwrap(),
                    want,
                    "seed {seed}, {n} threads"
                );
            }
            assert_eq!(
                checksum_files_with(&dir, &files, &[], Workers::Auto).unwrap(),
                want
            );
        }
    }

    #[test]
    fn completion_order_does_not_change_the_digests() {
        let (dir, files) = tree(3, 200);
        let entries = entries(&dir, &files);
        let forward = digest_in(&entries, Workers::Exactly(4), &|i, _| i).unwrap();
        let reverse = digest_in(&entries, Workers::Exactly(4), &|i, n| n - 1 - i).unwrap();
        // Odd claims from the front, even ones from the back.
        let mixed = |i: usize, n: usize| {
            if i.is_multiple_of(2) {
                i / 2
            } else {
                n - 1 - i / 2
            }
        };
        let interleaved = digest_in(&entries, Workers::Exactly(3), &mixed).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward, interleaved);
        assert_eq!(forward, digest_all(&entries, Workers::Exactly(1)).unwrap());
    }

    /// Several failures report the first in canonical order, whatever the
    /// threads and the order they claim entries in.
    #[test]
    fn error_is_independent_of_the_schedule() {
        let (dir, mut files) = tree(5, 150);
        for missing in ["d9/z", "d3/m", "d0/zz"] {
            files.push(join(&dir, missing));
        }
        let want = join(&dir, "d0/zz");
        for n in [1, 2, 5, 16] {
            for order in [
                &(|i, _| i) as &(dyn Fn(usize, usize) -> usize + Sync),
                &|i, n| n - 1 - i,
            ] {
                let entries = entries(&dir, &files);
                let err = digest_in(&entries, Workers::Exactly(n), order).unwrap_err();
                assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
                assert!(err.to_string().starts_with(&want), "{n}: {err}");
            }
        }
    }

    #[test]
    fn data_is_folded_in_after_the_files() {
        let (dir, files) = tree(9, 70);
        let data = [Datum::Value("v".to_string())];
        let one = checksum_files_with(&dir, &files, &data, Workers::Exactly(1)).unwrap();
        assert_eq!(
            one,
            checksum_files_with(&dir, &files, &data, Workers::Exactly(6)).unwrap()
        );
        assert_ne!(one, reference(&dir, &files));
    }
}
