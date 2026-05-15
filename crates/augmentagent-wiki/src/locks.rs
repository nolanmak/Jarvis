//! Per-page advisory locking for wiki Edits.
//!
//! Every wiki Edit must be wrapped to prevent concurrent-write corruption
//! between ingest (email/discord/linkedin/slack triage tails), the proactive
//! scanner, gcal Meeting log writes, and voice-memo captures. All of those
//! run inside the same augmentagent daemon process, so an intra-process
//! `tokio::sync::Mutex` keyed by canonicalised path is enough — we do not
//! need OS-level flock for the v1 wiki concurrency story.
//!
//! Usage:
//!
//! ```ignore
//! use augmentagent_wiki::with_page_lock;
//!
//! let updated = with_page_lock(&path, || async {
//!     let body = tokio::fs::read_to_string(&path).await?;
//!     let next = mutate(body);
//!     tokio::fs::write(&path, next).await?;
//!     anyhow::Ok(())
//! }).await?;
//! ```
//!
//! The `path` does not need to exist — locking is by canonicalised parent +
//! filename, so the first writer creating a page still gets exclusive access.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use dashmap::DashMap;
use tokio::sync::Mutex;

/// In-process registry of per-page mutexes. Keyed by the absolute,
/// lexically-normalised path string. We deliberately keep entries forever
/// (no eviction); a 562-page wiki produces ~562 mutex handles, ~50KB of
/// memory, which is negligible.
static LOCKS: OnceLock<DashMap<PathBuf, Arc<Mutex<()>>>> = OnceLock::new();

fn locks() -> &'static DashMap<PathBuf, Arc<Mutex<()>>> {
    LOCKS.get_or_init(DashMap::new)
}

/// Hard ceiling on how long a writer will wait for the lock. Five seconds
/// matches SQLite's busy_timeout in `augmentagent-store::Store::open` so the
/// two layers fail in roughly the same window when something is wedged.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Acquire an exclusive lock on `path` for the duration of `f`. Concurrent
/// callers serialize; a callsite waiting more than `LOCK_TIMEOUT` returns an
/// error (treats the prior holder as deadlocked rather than blocking the
/// poll loop forever).
pub async fn with_page_lock<F, Fut, R>(path: &Path, f: F) -> Result<R>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R>>,
{
    let key = normalize(path);
    let mutex = locks()
        .entry(key.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();

    let guard = match tokio::time::timeout(LOCK_TIMEOUT, mutex.lock()).await {
        Ok(g) => g,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "wiki page lock for {} not acquired within {:?}",
                key.display(),
                LOCK_TIMEOUT
            ));
        }
    };

    let result = f().await;
    drop(guard);
    result
}

/// Normalize a path to a canonical key for the lock map. We do NOT call
/// `canonicalize` (would fail on not-yet-created pages); instead, lexically
/// strip `.` and `..` segments. Caller is expected to feed an absolute path
/// (which `WikiLayout` produces) — if not, we fall back to the input as-is.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Two tasks racing on the same path must serialize: the inner critical
    /// section must observe `in_flight == 1` for every entry, never 2.
    #[tokio::test]
    async fn two_tasks_serialize_on_the_same_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("people").join("aadit-sheth.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "stub").unwrap();

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let p = path.clone();
            let in_flight = Arc::clone(&in_flight);
            let max_observed = Arc::clone(&max_observed);
            handles.push(tokio::spawn(async move {
                with_page_lock(&p, || async {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_observed.fetch_max(now, Ordering::SeqCst);
                    // Hold the lock long enough that the second task would
                    // race if locking didn't actually serialize.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            max_observed.load(Ordering::SeqCst),
            1,
            "concurrent holders observed inside the lock — serialization broken"
        );
    }

    /// Two distinct paths must NOT serialize: holding the lock on page A
    /// must not block a writer on page B.
    #[tokio::test]
    async fn distinct_paths_run_in_parallel() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.md");
        let b = tmp.path().join("b.md");

        let started = Arc::new(AtomicUsize::new(0));
        let max_inflight = Arc::new(AtomicUsize::new(0));

        let s1 = Arc::clone(&started);
        let m1 = Arc::clone(&max_inflight);
        let t1 = tokio::spawn(async move {
            with_page_lock(&a, || async {
                let now = s1.fetch_add(1, Ordering::SeqCst) + 1;
                m1.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                s1.fetch_sub(1, Ordering::SeqCst);
                Ok::<(), anyhow::Error>(())
            })
            .await
            .unwrap();
        });

        let s2 = Arc::clone(&started);
        let m2 = Arc::clone(&max_inflight);
        let t2 = tokio::spawn(async move {
            with_page_lock(&b, || async {
                let now = s2.fetch_add(1, Ordering::SeqCst) + 1;
                m2.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                s2.fetch_sub(1, Ordering::SeqCst);
                Ok::<(), anyhow::Error>(())
            })
            .await
            .unwrap();
        });

        t1.await.unwrap();
        t2.await.unwrap();

        assert_eq!(
            max_inflight.load(Ordering::SeqCst),
            2,
            "distinct paths serialized — locks should be per-page"
        );
    }
}
