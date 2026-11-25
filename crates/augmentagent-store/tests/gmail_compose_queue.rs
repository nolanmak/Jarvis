//! #2387 — durable Gmail compose queue: schema, idempotent enqueue, due
//! selection, compare-and-swap coordination and the once-only approval-card
//! post receipt. Storage only; nothing here talks to Gmail.

use augmentagent_store::ComposeQueueStatus::{self, GaveUp, Landed, Queued, Retrying, Unknown};
use augmentagent_store::{NewComposeQueueEntry, Store, StoreError};

const T0: i64 = 1_700_000_000_000;
const LEASE: i64 = 120_000;
const ALL: [ComposeQueueStatus; 5] = [Queued, Retrying, Landed, GaveUp, Unknown];

fn open(dir: &tempfile::TempDir) -> Store {
    Store::open(dir.path().join("data.db")).unwrap()
}

/// `payload` stands in for the serialized draft the compose path would park.
fn entry(idem_key: &str, marker: &str, retry_after_ms: i64) -> NewComposeQueueEntry {
    NewComposeQueueEntry {
        entity: "owner@example.com".into(),
        payload: format!(r#"{{"to":["peer@example.net"],"body":"{marker}"}}"#),
        retry_after_ms,
        idem_key: idem_key.into(),
    }
}

fn park(store: &Store, idem_key: &str, retry_after_ms: i64) -> bool {
    store.enqueue_gmail_compose(&entry(idem_key, "original", retry_after_ms), T0).unwrap()
}

fn due_keys(store: &Store, now_ms: i64) -> Vec<String> {
    store.due_gmail_composes(now_ms).unwrap().into_iter().map(|r| r.idem_key).collect()
}

/// Writes around the typed API so the schema's own constraints are what is
/// under test; `true` when the row reached the file.
fn raw_insert(store: &Store, idem_key: &str, status: &str) -> bool {
    let sql = format!(
        "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, \
         status, created_at, updated_at) \
         VALUES ('owner@example.com', 'x', 0, 0, '{idem_key}', '{status}', 0, 0)"
    );
    store.with_conn(|conn| Ok(conn.execute(&sql, []).is_ok())).unwrap()
}

#[test]
fn schema_pins_the_columns_the_unique_key_and_the_allowed_statuses() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let columns: Vec<String> = store
        .with_conn(|conn| {
            let mut stmt = conn.prepare("PRAGMA table_info(gmail_compose_queue)")?;
            let names = stmt.query_map([], |r| r.get::<_, String>(1))?.collect();
            names
        })
        .unwrap();
    let want = "entity,payload,retry_after,attempt,idem_key,status,created_at,updated_at";
    assert_eq!(columns.join(","), want);

    for status in ALL {
        assert!(raw_insert(&store, status.as_str(), status.as_str()));
        assert_eq!(ComposeQueueStatus::parse(status.as_str()).unwrap(), status);
        assert_eq!(status.is_retryable(), matches!(status, Queued | Retrying));
    }
    assert!(!raw_insert(&store, "queued", "queued"), "idem_key must be unique");
    assert!(!raw_insert(&store, "fresh", "posted"), "unknown statuses must be refused");
    for bad in ["", "posted", "QUEUED", "gaveup"] {
        assert!(matches!(ComposeQueueStatus::parse(bad), Err(StoreError::InvalidInput(_))));
    }
}

/// The reported case: a compose parked once the in-loop 429 retries ran out
/// stays exactly one row, with its original payload and retry boundary, over
/// a repeat 429 and a reopen — and its approval card is authorized once.
#[test]
fn a_reopen_keeps_one_row_per_key_and_one_post_receipt() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(&dir);
        assert!(park(&store, "rate-limited", T0 + 60_000));
        let again = store.enqueue_gmail_compose(&entry("rate-limited", "replacement", T0), T0 + 5);
        assert!(!again.unwrap(), "a second enqueue under the same key must not insert");
        assert!(store.claim_approval_card_post("rate-limited", T0).unwrap());
        let twice = store.claim_approval_card_post("rate-limited", T0 + 1);
        assert!(!twice.unwrap(), "the second claim in the same process must lose");
    }

    let store = open(&dir);
    let rows: i64 = store
        .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM gmail_compose_queue", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(rows, 1, "a repeat 429 under one key must leave exactly one row");
    assert_eq!(due_keys(&store, T0 + 60_000), ["rate-limited"]);
    let row = store.gmail_compose("rate-limited").unwrap().unwrap();
    assert_eq!(row.payload, entry("", "original", 0).payload);
    assert_eq!((row.entity.as_str(), row.status, row.attempt), ("owner@example.com", Queued, 0));
    assert_eq!((row.retry_after_ms, row.created_at_ms, row.updated_at_ms), (T0 + 60_000, T0, T0));
    assert!(store.gmail_compose("never-queued").unwrap().is_none());
    let reopened = store.claim_approval_card_post("rate-limited", T0 + 2);
    assert!(!reopened.unwrap(), "the receipt must outlive the process that wrote it");
    assert!(store.claim_approval_card_post("other-key", T0 + 2).unwrap());
}

#[test]
fn due_selection_takes_retryable_rows_and_skips_terminal_and_future_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    assert!(park(&store, "due-queued", T0 - 1));
    assert!(park(&store, "due-retrying", T0 - 1));
    assert!(store.claim_gmail_compose("due-retrying", Queued, 0, T0 - 1, LEASE).unwrap());
    // that attempt failed and re-parked itself, releasing its lease.
    let reparked = store.transition_gmail_compose("due-retrying", Retrying, 1, Retrying, Some(T0), T0);
    assert!(reparked.unwrap());
    assert!(park(&store, "future", T0 + 1));
    for terminal in [Landed, GaveUp, Unknown] {
        let key = format!("terminal-{}", terminal.as_str());
        assert!(park(&store, &key, T0 - 5_000));
        let done = store.transition_gmail_compose(&key, Queued, 0, terminal, None, T0);
        assert!(done.unwrap());
    }

    assert!(due_keys(&store, T0 - 2).is_empty());
    assert_eq!(due_keys(&store, T0), ["due-queued", "due-retrying"]);
    assert_eq!(due_keys(&store, T0 + 1), ["due-queued", "due-retrying", "future"]);
}

#[test]
fn competing_and_stale_compare_and_swaps_leave_the_row_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    assert!(park(&store, "cas", T0));

    assert!(store.claim_gmail_compose("cas", Queued, 0, T0 + 1, LEASE).unwrap());
    let claimed = store.gmail_compose("cas").unwrap().unwrap();
    assert_eq!((claimed.status, claimed.attempt, claimed.updated_at_ms), (Retrying, 1, T0 + 1));
    let competing = store.claim_gmail_compose("cas", Queued, 0, T0 + 2, LEASE);
    assert!(!competing.unwrap(), "the second sweeper must lose the claim");
    assert!(!store.claim_gmail_compose("missing", Queued, 0, T0, LEASE).unwrap());
    let stale = store.transition_gmail_compose("cas", Retrying, 0, Landed, None, T0 + 3);
    assert!(!stale.unwrap(), "a stale attempt count must not transition the row");
    assert_eq!(store.gmail_compose("cas").unwrap().unwrap(), claimed);

    let next = Some(T0 + 90_000);
    assert!(store.transition_gmail_compose("cas", Retrying, 1, Queued, next, T0 + 4).unwrap());
    let reparked = store.gmail_compose("cas").unwrap().unwrap();
    assert_eq!((reparked.status, reparked.retry_after_ms), (Queued, T0 + 90_000));

    assert!(store.transition_gmail_compose("cas", Queued, 1, Landed, None, T0 + 5).unwrap());
    let replay = store.transition_gmail_compose("cas", Queued, 1, Unknown, None, T0 + 6);
    assert!(!replay.unwrap(), "the applied transition must not be replayed");
    let landed = store.gmail_compose("cas").unwrap().unwrap();
    assert_eq!((landed.status, landed.updated_at_ms), (Landed, T0 + 5));
    assert_eq!(landed.retry_after_ms, T0 + 90_000, "retry_after is kept when none is supplied");
}

/// A claim is a lease. The row the winner leaves behind carries the state a
/// second sweep would read, so that state must not be claimable: a compose in
/// flight may not be attempted twice. The lease lapsing is what puts an
/// attempt whose process died back in the rotation.
#[test]
fn a_second_sweep_cannot_reclaim_a_compose_that_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    assert!(park(&store, "rate-limited", T0));
    assert!(store.claim_gmail_compose("rate-limited", Queued, 0, T0, LEASE).unwrap());
    let leased = store.gmail_compose("rate-limited").unwrap().unwrap();
    assert_eq!((leased.status, leased.attempt, leased.retry_after_ms), (Retrying, 1, T0 + LEASE));

    let mid_lease = T0 + LEASE - 1;
    assert!(due_keys(&store, mid_lease).is_empty(), "a leased row must not be swept");
    let second = store.claim_gmail_compose("rate-limited", Retrying, 1, mid_lease, LEASE);
    assert!(!second.unwrap(), "the state the winner wrote must not be reclaimable");
    assert_eq!(store.gmail_compose("rate-limited").unwrap().unwrap(), leased);

    assert_eq!(due_keys(&store, T0 + LEASE), ["rate-limited"]);
    assert!(store.claim_gmail_compose("rate-limited", Retrying, 1, T0 + LEASE, LEASE).unwrap());
    let retaken = store.gmail_compose("rate-limited").unwrap().unwrap();
    assert_eq!((retaken.attempt, retaken.retry_after_ms), (2, T0 + 2 * LEASE));

    let unleased = store.claim_gmail_compose("rate-limited", Retrying, 2, T0 + 2 * LEASE, 0);
    assert!(matches!(unleased, Err(StoreError::InvalidInput(_))), "a lease must be positive");
}

/// A settled compose is the last word: whatever a caller believes it read,
/// neither a claim nor a transition may put it back in the retry rotation.
#[test]
fn a_terminal_row_can_neither_be_claimed_nor_revived() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    for terminal in [Landed, GaveUp, Unknown] {
        let key = format!("settled-{}", terminal.as_str());
        assert!(park(&store, &key, T0));
        assert!(store.transition_gmail_compose(&key, Queued, 0, terminal, None, T0 + 1).unwrap());
        let settled = store.gmail_compose(&key).unwrap().unwrap();

        let claimed = store.claim_gmail_compose(&key, terminal, 0, T0 + 2, LEASE);
        assert!(matches!(claimed, Err(StoreError::InvalidInput(_))), "claimed a {terminal:?} row");
        let revived = store.transition_gmail_compose(&key, terminal, 0, Retrying, Some(T0), T0 + 3);
        assert!(matches!(revived, Err(StoreError::InvalidInput(_))), "revived a {terminal:?} row");
        assert_eq!(store.gmail_compose(&key).unwrap().unwrap(), settled);
        assert!(!due_keys(&store, T0 + 3).contains(&key));
    }
}
