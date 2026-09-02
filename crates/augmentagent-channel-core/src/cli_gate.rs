//! #898 — process-global cap on concurrently running CLI reasoner
//! subprocesses (`claude -p`, `codex`, `gemini`).
//!
//! Every reasoner call is a fresh CLI process costing 60–100 MB before it
//! does any work. Nothing bounded how many could be alive at once: on
//! 2026-08-31 an ingest burst spawned 297 of them (~16 GB on a 15 GB box)
//! and the kernel OOM killer took the desktop session with it (#897). The
//! only thing that stopped it at 297 was the unit's 1024 soft fd limit.
//!
//! The gate is a semaphore shared by every reasoner instance in the
//! process — each channel builds its own reasoner, so a per-instance limit
//! would not help. Callers beyond the capacity *wait*: ingest is
//! best-effort and already async, so queueing is the correct backpressure;
//! nothing is dropped or errored here. A permit is taken immediately before
//! `Command::spawn` and lives in the same scope as the `Child`, so it is
//! released when the child is reaped, on every early-return error, and when
//! the caller's future is dropped (the #656 watchdog, shutdown).
//!
//! Capacity: `AUGMENTAGENT_REASONER_MAX_INFLIGHT` (default 4, minimum 1),
//! read once when the global gate is first used.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

pub const DEFAULT_MAX_INFLIGHT: usize = 4;
pub const ENV_MAX_INFLIGHT: &str = "AUGMENTAGENT_REASONER_MAX_INFLIGHT";
/// A saturated gate logs at most once per this interval — a 1,700-item
/// burst must not become 1,700 log lines.
const SATURATION_WARN_EVERY: Duration = Duration::from_secs(60);

pub struct CliGate {
    sem: Arc<Semaphore>,
    capacity: usize,
    in_flight: AtomicUsize,
    waiting: AtomicUsize,
    last_warn_ms: AtomicU64,
}

impl CliGate {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            sem: Arc::new(Semaphore::new(capacity)),
            capacity,
            in_flight: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            last_warn_ms: AtomicU64::new(0),
        }
    }

    /// The process-wide gate every production reasoner shares.
    pub fn global() -> Arc<CliGate> {
        static GLOBAL: OnceLock<Arc<CliGate>> = OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| {
            let capacity = parse_capacity(std::env::var(ENV_MAX_INFLIGHT).ok().as_deref());
            info!(capacity, "reasoner CLI gate armed ({ENV_MAX_INFLIGHT})");
            Arc::new(CliGate::new(capacity))
        }))
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Permits currently held — CLI children alive (or about to spawn).
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Callers currently queued for a permit.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// Wait for a slot. Never fails (the semaphore is never closed).
    /// Cancellation-safe: a caller dropped while queued leaves no trace.
    pub async fn acquire(self: &Arc<Self>, provider: &str) -> CliPermit {
        let _queued = WaitGuard::enter(self);
        if self.waiting() > self.capacity {
            self.warn_saturated(provider);
        }
        let permit = Arc::clone(&self.sem)
            .acquire_owned()
            .await
            .expect("CLI gate semaphore is never closed");
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        CliPermit {
            _permit: permit,
            gate: Arc::clone(self),
        }
    }

    fn warn_saturated(&self, provider: &str) {
        let now = now_ms();
        let last = self.last_warn_ms.load(Ordering::Relaxed);
        let due = now.saturating_sub(last) >= SATURATION_WARN_EVERY.as_millis() as u64;
        if due
            && self
                .last_warn_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            warn!(
                provider,
                waiting = self.waiting(),
                in_flight = self.in_flight(),
                capacity = self.capacity,
                "reasoner CLI gate saturated; calls are queueing (see {ENV_MAX_INFLIGHT})"
            );
        }
    }
}

/// Held for the lifetime of one CLI child. Dropping it frees the slot.
pub struct CliPermit {
    _permit: OwnedSemaphorePermit,
    gate: Arc<CliGate>,
}

impl Drop for CliPermit {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Counts a queued caller; undone on drop so a cancelled wait cannot leak.
struct WaitGuard<'a> {
    gate: &'a CliGate,
}

impl<'a> WaitGuard<'a> {
    fn enter(gate: &'a CliGate) -> Self {
        gate.waiting.fetch_add(1, Ordering::SeqCst);
        Self { gate }
    }
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.gate.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

fn parse_capacity(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_INFLIGHT)
        .max(1)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permits_are_bounded_and_released() {
        let gate = Arc::new(CliGate::new(2));
        let a = gate.acquire("t").await;
        let b = gate.acquire("t").await;
        assert_eq!(gate.in_flight(), 2);

        let third = tokio::time::timeout(Duration::from_millis(50), gate.acquire("t")).await;
        assert!(third.is_err(), "third permit must wait for a free slot");
        assert_eq!(gate.in_flight(), 2);

        drop(a);
        let c = tokio::time::timeout(Duration::from_millis(500), gate.acquire("t"))
            .await
            .expect("a released slot is handed to the next caller");
        assert_eq!(gate.in_flight(), 2);
        drop(b);
        drop(c);
        assert_eq!(gate.in_flight(), 0);
        assert_eq!(gate.waiting(), 0);
    }

    #[tokio::test]
    async fn cancelled_wait_does_not_leak_waiting_count() {
        let gate = Arc::new(CliGate::new(1));
        let held = gate.acquire("t").await;
        let cancelled = tokio::time::timeout(Duration::from_millis(30), gate.acquire("t")).await;
        assert!(cancelled.is_err());
        assert_eq!(gate.waiting(), 0, "a dropped waiter must not stay counted");
        drop(held);
        assert_eq!(gate.in_flight(), 0);
    }

    #[test]
    fn capacity_parsing_is_defensive() {
        assert_eq!(parse_capacity(None), DEFAULT_MAX_INFLIGHT);
        assert_eq!(parse_capacity(Some(" 8 ")), 8);
        assert_eq!(parse_capacity(Some("0")), 1, "never a zero-capacity gate");
        assert_eq!(parse_capacity(Some("lots")), DEFAULT_MAX_INFLIGHT);
        assert_eq!(CliGate::new(0).capacity(), 1);
    }
}
