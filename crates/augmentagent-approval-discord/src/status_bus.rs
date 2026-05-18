//! #47 — cross-surface state sync: in-process status broadcast.
//!
//! When any surface (Discord click, dashboard POST `/api/v1/actions/:id`,
//! Telegram, CLI, the nudge loop) resolves an action, it should flip the row
//! via the store's compare-and-swap [`Store::try_resolve_action`] (so two
//! racing surfaces can't double-fire side effects) and then publish a
//! [`StatusChanged`] on this bus. Other surfaces subscribe and drop their own
//! now-stale UI (e.g. Discord deletes the approval card) — *unless they were
//! the originating surface*, in which case they skip their own echo.
//!
//! The bus is a thin wrapper over `tokio::sync::broadcast`. It is intentionally
//! lossy: a slow subscriber that lags just misses intermediate events and
//! re-syncs from the DB on its next sweep (the existing
//! `sweep_resolved_cards` poll is the durable backstop). Instant card removal
//! is the optimization; correctness never depends on the broadcast arriving.

use tokio::sync::broadcast;

/// One cross-surface status transition.
#[derive(Debug, Clone)]
pub struct StatusChanged {
    pub action_id: String,
    pub new_status: String,
    /// Surface that performed the transition: `discord` | `dashboard` |
    /// `telegram` | `cli` | `nudge` | `api_v1`. A subscriber compares this
    /// against its own name to suppress its own echo.
    pub source: String,
}

/// Cloneable handle to publish + subscribe. Construct once in the daemon and
/// hand clones to every surface.
#[derive(Clone)]
pub struct StatusBus {
    tx: broadcast::Sender<StatusChanged>,
}

impl StatusBus {
    pub fn new() -> Self {
        // 256 is comfortably above the burst size of one user resolving a
        // backlog; lagged receivers degrade to a DB re-sync, never a panic.
        let (tx, _rx) = broadcast::channel(256);
        Self { tx }
    }

    /// Publish a transition. Best-effort: returns the live receiver count, or
    /// 0 if nobody is listening (not an error — the DB is source of truth).
    pub fn publish(&self, ev: StatusChanged) -> usize {
        self.tx.send(ev).unwrap_or(0)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StatusChanged> {
        self.tx.subscribe()
    }
}

impl Default for StatusBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscriber_receives_published_event() {
        let bus = StatusBus::new();
        let mut rx = bus.subscribe();
        let n = bus.publish(StatusChanged {
            action_id: "a1".into(),
            new_status: "sent".into(),
            source: "dashboard".into(),
        });
        assert_eq!(n, 1, "one subscriber should be counted");
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.action_id, "a1");
        assert_eq!(ev.source, "dashboard");
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_not_an_error() {
        let bus = StatusBus::new();
        assert_eq!(
            bus.publish(StatusChanged {
                action_id: "x".into(),
                new_status: "skipped".into(),
                source: "cli".into(),
            }),
            0
        );
    }

    #[tokio::test]
    async fn originating_surface_can_suppress_its_own_echo() {
        let bus = StatusBus::new();
        let mut rx = bus.subscribe();
        bus.publish(StatusChanged {
            action_id: "a2".into(),
            new_status: "rejected".into(),
            source: "discord".into(),
        });
        let ev = rx.recv().await.unwrap();
        // A Discord subscriber would compare `ev.source == "discord"` and skip
        // deleting a card it itself just resolved.
        let is_own_echo = ev.source == "discord";
        assert!(is_own_echo);
    }
}
