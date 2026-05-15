//! Synthetic load test for the RateGovernor (#83 §10).
//!
//! Fires 1000 mock actions concurrently across 8 worker threads, verifies
//! sliding-window caps hold, then closes the connection, reopens, and
//! verifies post-restart state matches.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use augmentagent_channel_core::governor::{
    ActionKind, ActionRequest, Clock, Denial, Outcome, Platform, RateGovernor, Risk,
    SqliteGovernor, TargetAttrs,
};
use augmentagent_store::Store;
use rusqlite::Connection;
use tempfile::TempDir;
use uuid::Uuid;

/// Seed the Node-tree-owned tables `Store::migrate` probes for. Mirrors
/// the helper in the lib's `tests` module — duplicated here because Rust
/// integration tests can't `use` private items from the library crate.
fn seed_node_owned_tables(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS actions (
            id TEXT PRIMARY KEY,
            messageId TEXT NOT NULL,
            threadId TEXT,
            fromEmail TEXT NOT NULL,
            subject TEXT NOT NULL,
            originalBody TEXT,
            draftBody TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            errorMessage TEXT,
            createdAt INTEGER NOT NULL,
            updatedAt INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS emails (
            messageId TEXT PRIMARY KEY,
            threadId TEXT,
            fromEmail TEXT NOT NULL,
            subject TEXT NOT NULL,
            body TEXT,
            receivedAt TEXT,
            accountEntityId TEXT,
            firstSeenAt INTEGER NOT NULL,
            triageResult TEXT,
            agentProcessedAt INTEGER,
            platform TEXT NOT NULL DEFAULT 'gmail',
            kind TEXT NOT NULL DEFAULT 'dm'
        );
        CREATE TABLE IF NOT EXISTS gmail_accounts (
            id TEXT PRIMARY KEY,
            connectionId TEXT NOT NULL,
            email TEXT,
            label TEXT,
            entityId TEXT NOT NULL,
            active INTEGER DEFAULT 1,
            createdAt INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS channel_subscriptions (
            id TEXT PRIMARY KEY,
            platform TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            display_name TEXT NOT NULL,
            mode TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1,
            last_seen_message_id TEXT,
            last_digest_at_ms INTEGER,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS slack_workspaces (
            id TEXT PRIMARY KEY,
            team_id TEXT NOT NULL UNIQUE,
            team_name TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            connection_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1,
            created_at_ms INTEGER NOT NULL
        );
        "#,
    )
    .unwrap();
}

/// Pin-in-place clock — mirrors the one in the unit-test module so we can
/// reuse the same time-control pattern from an integration test.
struct FakeClock {
    now: AtomicI64,
    hour: AtomicI64,
}

impl FakeClock {
    fn new(now: i64, hour: u32) -> Self {
        Self {
            now: AtomicI64::new(now),
            hour: AtomicI64::new(hour as i64),
        }
    }
    fn advance(&self, d: Duration) {
        self.now.fetch_add(d.as_millis() as i64, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn local_hour(&self) -> u32 {
        self.hour.load(Ordering::SeqCst) as u32
    }
}

fn req(account: &str) -> ActionRequest {
    ActionRequest {
        platform: Platform::Twitter,
        action: ActionKind::Like,
        // Use a unique account ID per spawned task so the warmup row is
        // shared but the per-task target_id stays distinct in the audit log.
        account_id: account.into(),
        risk: Risk::Low,
        cause: "load_test".into(),
        target_id: Some(Uuid::new_v4().to_string()),
        target_attrs: Some(TargetAttrs {
            known_contact: true,
            mass_action: false,
            stranger: false,
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn caps_hold_under_concurrent_load_and_survive_restart() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("rg.db");
    seed_node_owned_tables(&db_path);

    // 12:00 local — outside quiet hours.
    let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));

    // ----- Phase 1: pile up to the cap, all on one account so one warmup
    // row applies to the whole barrage. We use `Twitter::Like` because it
    // has the most generous caps in the table (200/day, 50/hr, 10 burst,
    // 15s gap), so we can actually exercise *all* of the windowed gates
    // with a single test by walking the clock forward in fixed steps.

    // 200 day-cap * 0.25 warmup = 50 effective day cap on a fresh account.
    // Step the clock 16s per attempt so min_gap (15s) clears each time;
    // 1000 attempts * 16s = 16000s > 1h, so the *hourly* cap (50 * 0.25 =
    // 12 effective) also bites. After day cap hits, we should stop at 50.

    {
        let store = Arc::new(Store::open(&db_path).unwrap());
        let gov = Arc::new(SqliteGovernor::new(store.clone(), clock.clone()));

        // Concurrent fan-out — 8 tasks, each grabbing permits in a loop.
        // The clock is shared and advanced atomically; SQLite's mutex
        // serializes the COUNT() so the cap math sees a consistent view.
        // Total attempts: 1000. The cap math (not the test) decides how
        // many succeed.
        let mut handles = Vec::new();
        for worker in 0..8u32 {
            let gov = Arc::clone(&gov);
            let clock = Arc::clone(&clock);
            handles.push(tokio::spawn(async move {
                let mut local_ok = 0u32;
                for i in 0..125u32 {
                    // Stagger so workers don't all hit the same now_ms.
                    clock.advance(Duration::from_secs(2));
                    let r = req(&format!("acct-{worker}-{i}"));
                    // Force everything onto one platform-account so the
                    // sliding window actually accumulates across workers.
                    let mut r = r;
                    r.account_id = "shared".into();
                    match gov.permit(r).await {
                        Ok(p) => {
                            gov.record(p, Outcome::Ok).await.unwrap();
                            local_ok += 1;
                        }
                        Err(
                            Denial::DailyCap { .. }
                            | Denial::HourlyCap { .. }
                            | Denial::BurstCap { .. }
                            | Denial::MinGap { .. },
                        ) => {
                            // Expected — we're deliberately overrunning.
                        }
                        Err(other) => panic!("unexpected denial: {other:?}"),
                    }
                }
                local_ok
            }));
        }
        let total_ok: u32 = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .sum();

        // Day cap = 200 * 0.25 = 50. Hourly cap is the actual limiting
        // factor in this test once 1h of clock time has passed (12 effective
        // per hour). Total clock advance = 8 workers * 125 iters * 2s =
        // 2000s ≈ 33 min, so we never roll out of the hour window — the
        // hourly cap (12) is what actually bites. Burst (10 in 5min)
        // bites earlier still. Safe upper bound: ≤ 50.
        assert!(
            total_ok > 0 && total_ok <= 50,
            "expected to land within day-1 caps (≤50), got {total_ok}"
        );
    }

    // ----- Phase 2: drop the governor + reopen the store, simulating a
    // daemon restart. The same effective caps should hold because state is
    // in SQLite, not in memory.
    {
        let store = Arc::new(Store::open(&db_path).unwrap());
        let gov = SqliteGovernor::new(store.clone(), clock.clone());

        // Same wall-clock day → cap still exhausted. Even if the warmup
        // multiplier or burst window has space, the day cap should bite.
        // Step gap forward so min_gap doesn't pre-empt the cap denial.
        clock.advance(Duration::from_secs(20));
        let r = {
            let mut r = req("post-restart");
            r.account_id = "shared".into();
            r
        };
        match gov.permit(r).await {
            Err(Denial::DailyCap { .. })
            | Err(Denial::HourlyCap { .. })
            | Err(Denial::BurstCap { .. }) => {
                // Expected — caps survived restart.
            }
            Ok(_) => {
                // Acceptable iff sliding window has space — the day cap is
                // 50 but we should have already burned 50. Fail loudly so a
                // regression here is loud.
                panic!("permit succeeded post-restart; caps did not survive!");
            }
            Err(other) => panic!("unexpected post-restart denial: {other:?}"),
        }
    }

    // ----- Phase 3: advance clock 25h ⇒ window slides ⇒ permits flow again.
    {
        let store = Arc::new(Store::open(&db_path).unwrap());
        let gov = SqliteGovernor::new(store, clock.clone());
        clock.advance(Duration::from_secs(25 * 3600));
        let mut r = req("after-window");
        r.account_id = "shared".into();
        let p = gov
            .permit(r)
            .await
            .expect("permit should succeed once 24h slides");
        gov.record(p, Outcome::Ok).await.unwrap();
    }
}

#[tokio::test]
async fn audit_query_returns_inserted_rows() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rg.db");
    seed_node_owned_tables(&path);
    let store = Arc::new(Store::open(&path).unwrap());
    let clock = Arc::new(FakeClock::new(1_700_000_000_000, 12));
    let gov = SqliteGovernor::new(store.clone(), clock.clone());

    // Fire 5 successful permits for the same account.
    for i in 0..5 {
        clock.advance(Duration::from_secs(20));
        let mut r = req(&format!("ignored-{i}"));
        r.account_id = "audit-acct".into();
        let p = gov.permit(r).await.unwrap();
        gov.record(p, Outcome::Ok).await.unwrap();
    }

    let rows = store
        .rate_audit_query("audit-acct", Some("twitter"), 0, i64::MAX)
        .unwrap();
    assert_eq!(rows.len(), 5);
    // Newest-first ordering.
    for w in rows.windows(2) {
        assert!(w[0].occurred_at_ms >= w[1].occurred_at_ms);
    }
    // Cross-platform query (no filter) should also return them.
    let all = store
        .rate_audit_query("audit-acct", None, 0, i64::MAX)
        .unwrap();
    assert_eq!(all.len(), 5);
}
