//! #1285 — `augmentagent status` reports durable-delivery backlog, retry,
//! reconcile and dead-letter counts per surface. Runs the built binary
//! against a temporary database seeded through the store API; nothing here
//! reads the real user state or talks to a provider.

use std::process::Command;

use augmentagent_store::delivery::{
    NewInboundEvent, NewOutboundSend, OutboundOperation, RetryPolicy,
};
use augmentagent_store::{Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform};
use tempfile::TempDir;

const T0: i64 = 1_700_000_000_000;

fn conv(platform: &str, account: &str, conversation: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), account).unwrap(),
        conversation,
        None,
    )
    .unwrap()
}

fn send(conversation: &SurfaceConversationRef, key: &str, max_attempts: u32) -> NewOutboundSend {
    NewOutboundSend {
        conversation: conversation.clone(),
        idempotency_key: key.into(),
        operation: OutboundOperation::Post,
        target_message_id: None,
        payload: "{}".into(),
        max_attempts,
        interaction_expires_at_ms: None,
    }
}

/// Slack: one unhandled inbound event, one send retrying, one queued behind
/// it, one in reconcile (in flight at a simulated crash). WhatsApp: one dead
/// letter. Discord has no rows and must still be listed with zeros.
fn seed(path: &std::path::Path) {
    let slack = conv("slack", "team:T00000001", "C00000001");
    let slack_dm = conv("slack", "team:T00000001", "D00000001");
    let whatsapp = conv("whatsapp", "device:1", "chat:1");
    let policy = RetryPolicy {
        base_delay_ms: 60_000,
        max_delay_ms: 600_000,
    };
    {
        let store = Store::open(path).unwrap();
        store
            .record_inbound_event(
                &NewInboundEvent {
                    conversation: slack.clone(),
                    event_id: "C00000001:1700000000.000100".into(),
                    kind: "message".into(),
                    occurred_at_ms: T0,
                    payload: "{}".into(),
                },
                T0,
            )
            .unwrap();
        store
            .enqueue_outbound_send(&send(&slack_dm, "in-flight", 3), T0)
            .unwrap();
        let claimed = store.claim_next_outbound_send(T0).unwrap().unwrap();
        assert_eq!(claimed.idempotency_key, "in-flight");
        // Crash here: the send is ambiguous.
    }
    let store = Store::open(path).unwrap();
    store.recover_surface_delivery(T0 + 1).unwrap();
    store
        .enqueue_outbound_send(&send(&slack, "retrying", 3), T0 + 1)
        .unwrap();
    store
        .enqueue_outbound_send(&send(&slack, "queued", 3), T0 + 1)
        .unwrap();
    store
        .enqueue_outbound_send(&send(&whatsapp, "dead", 1), T0 + 1)
        .unwrap();
    for _ in 0..2 {
        let claimed = store.claim_next_outbound_send(T0 + 1).unwrap().unwrap();
        store
            .mark_outbound_failed(claimed.id, "timeout", true, &policy, T0 + 1)
            .unwrap();
    }
}

fn run_status(db: &std::path::Path, dir: &std::path::Path, json: bool) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .args(["status", "--json", if json { "true" } else { "false" }])
        .current_dir(dir)
        .env("AUGMENTAGENT_DB", db)
        .env("AUGMENTAGENT_GH_DISABLE", "1")
        .env_remove("AUGMENTAGENT_API_KEY")
        .env("DASHBOARD_PORT", "1")
        .output()
        .expect("spawn augmentagent status");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.is_empty(),
        "status printed nothing; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
fn status_json_reports_delivery_counts_per_surface() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("data.db");
    seed(&db);
    let doc: serde_json::Value = serde_json::from_str(&run_status(&db, tmp.path(), true)).unwrap();
    let delivery = &doc["delivery"];
    assert_eq!(
        delivery["slack"],
        serde_json::json!({
            "inbound_backlog": 1,
            "inbound_dead_letter": 0,
            "outbound_backlog": 3,
            "outbound_retrying": 1,
            "outbound_reconcile": 1,
            "outbound_dead_letter": 0,
        })
    );
    assert_eq!(delivery["whatsapp"]["outbound_dead_letter"], 1);
    assert_eq!(delivery["whatsapp"]["outbound_backlog"], 0);
    assert_eq!(delivery["discord"]["outbound_backlog"], 0);
    assert_eq!(delivery["discord"]["inbound_backlog"], 0);
}

#[test]
fn status_table_shows_a_delivery_section() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("data.db");
    seed(&db);
    let table = run_status(&db, tmp.path(), false);
    let line = |surface: &str| {
        table
            .lines()
            .find(|l| l.trim_start().starts_with(surface) && l.contains("inbound"))
            .unwrap_or_else(|| panic!("no delivery line for {surface} in:\n{table}"))
            .to_string()
    };
    assert!(table.contains("delivery:"), "{table}");
    assert_eq!(
        line("slack").split_whitespace().collect::<Vec<_>>(),
        [
            "slack",
            "inbound",
            "1",
            "dead",
            "0",
            "|",
            "outbound",
            "3",
            "retrying",
            "1",
            "reconcile",
            "1",
            "dead",
            "0"
        ]
    );
    assert!(line("whatsapp").ends_with("dead 1"), "{table}");
}
