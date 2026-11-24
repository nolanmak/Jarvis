//! Shared DM-alert policy (#1643): owner-priority resolution and the
//! rolling-hour deduplication that survives a restart. Every test in this
//! binary runs with the gate on; the per-channel suites cover placement.
use augmentagent_channel_core::dm_alerts::{self, InboundDm};
use augmentagent_store::{owner_alerts::Urgency, Store};

const T0: i64 = 1_800_000_000_000;
const CONVERSATION: &str = "15550000000@s.whatsapp.net";
const SOURCE: &str = "https://wa.me/15550000000";

/// Open (or reopen) the one store of a test, with the gate on. The default-off
/// behaviour is covered by the crate's own unit test, in a binary that never
/// sets the variable.
fn open(dir: &tempfile::TempDir) -> Store {
    std::env::set_var(dm_alerts::ENABLED, "1");
    Store::open(dir.path().join("db")).unwrap()
}

fn inbound(handles: &[String]) -> InboundDm<'_> {
    InboundDm {
        platform: "whatsapp",
        conversation: CONVERSATION,
        handles,
        person_slugs: &[],
        sender_label: "Priority Contact",
        source_url: SOURCE,
    }
}

#[test]
fn one_alert_per_rolling_hour_per_conversation_survives_a_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let handles = vec![CONVERSATION.to_string()];
    let dm = inbound(&handles);
    let first = {
        let store = open(&dir);
        assert!(
            dm_alerts::evaluate(&store, &dm, T0).is_none(),
            "no owner priority covers this sender"
        );
        store
            .set_owner_alert_priority(CONVERSATION, Urgency::Routine)
            .unwrap();
        assert!(
            dm_alerts::evaluate(&store, &dm, T0).is_none(),
            "a routine selector mutes the sender"
        );
        store
            .set_owner_alert_priority(CONVERSATION, Urgency::High)
            .unwrap();
        let first = dm_alerts::evaluate(&store, &dm, T0).expect("first qualifying DM alerts");
        assert!(
            dm_alerts::evaluate(&store, &dm, T0 + 60_000).is_none(),
            "a second message in the same hour must not alert again"
        );
        first
    };
    let reopened = open(&dir);
    assert!(
        dm_alerts::evaluate(&reopened, &dm, T0 + dm_alerts::WINDOW_MS - 1).is_none(),
        "a restart must not reopen the rolling hour"
    );
    let later = dm_alerts::evaluate(&reopened, &dm, T0 + dm_alerts::WINDOW_MS)
        .expect("the next hour alerts again");
    assert_ne!(first, later);
    let alerts: i64 = reopened
        .with_conn(|c| c.query_row("SELECT COUNT(*) FROM owner_alerts", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(alerts, 2);
}

