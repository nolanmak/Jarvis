//! #2422 — the durable half of the new-DM owner alert: prior inbound history,
//! the rolling one-hour claim, and delivery only to the owner's destination.
use augmentagent_store::{owner_alerts::{NewDmAlert, Urgency, DM_ALERT_WINDOW_MS}, Store};

fn dm(conversation: &str, first_contact: bool, priority: bool) -> NewDmAlert<'_> {
    NewDmAlert {
        platform: "imessage", conversation_id: conversation, sender: "+15555550111",
        source_url: "https://dashboard.example.test/history", timezone: "UTC",
        allow_first_contact: first_contact, allow_priority: priority, prior_inbound: false,
    }
}

fn high_alerts(store: &Store) -> i64 {
    let sql = "SELECT COUNT(*) FROM owner_alerts WHERE urgency='high'";
    store.with_conn(|c| c.query_row(sql, [], |r| r.get(0))).unwrap()
}

#[test]
fn only_the_first_stored_inbound_dm_is_a_first_contact() {
    let dir=tempfile::tempdir().unwrap();let store=Store::open(dir.path().join("test.db")).unwrap();
    let id = store.create_dm_owner_alert(&dm("c1", true, false), 1_000).unwrap().expect("qualifies");
    assert_eq!(store.owner_alert(&id).unwrap().unwrap().urgency, Urgency::High);
    // Hours later the same conversation is no longer a first contact, and a
    // conversation the channel already has inbound history for never was one.
    let later = 1_000 + 2 * DM_ALERT_WINDOW_MS;
    assert!(store.create_dm_owner_alert(&dm("c1", true, false), later).unwrap().is_none());
    let mut known = dm("c2", true, false);
    known.prior_inbound = true;
    assert!(store.create_dm_owner_alert(&known, 1_000).unwrap().is_none());
    // A policy permitting nothing still records history for the next message.
    assert!(store.create_dm_owner_alert(&dm("c3", false, false), 1_000).unwrap().is_none());
    assert!(store.create_dm_owner_alert(&dm("c3", true, false), 2_000).unwrap().is_none());
    assert_eq!(high_alerts(&store), 1);
}

#[test]
fn priority_alerts_whatever_the_history_but_once_an_hour_across_restart() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let mut known = dm("c1", false, true);
    known.prior_inbound = true;
    let within = 1_000 + DM_ALERT_WINDOW_MS - 1;
    assert!(store.create_dm_owner_alert(&known, 1_000).unwrap().is_some());
    assert!(store.create_dm_owner_alert(&known, within).unwrap().is_none());

    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.create_dm_owner_alert(&known, within).unwrap().is_none(),
        "the hourly claim must survive reopening the database");
    let after = 1_000 + DM_ALERT_WINDOW_MS;
    assert!(store.create_dm_owner_alert(&known, after).unwrap().is_some());
    assert_eq!(high_alerts(&store), 2);
}

#[test]
fn delivery_uses_only_the_enabled_owner_destination() {
    let dir=tempfile::tempdir().unwrap();let store=Store::open(dir.path().join("test.db")).unwrap();
    store.create_dm_owner_alert(&dm("c1", true, false), 1_000).unwrap().expect("qualifies");
    // Without the owner-text opt-in the alert is observable but undelivered.
    assert_eq!(store.enqueue_owner_alert_texts(1_000).unwrap(), 0);
    store.configure_owner_alert_texts(Some("+15555550100"), true).unwrap();
    assert_eq!(store.enqueue_owner_alert_texts(1_000).unwrap(), 1);
    let outbox = store.list_imessage_outbox(10).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].target, "+15555550100", "only the owner is ever texted");
}

#[test]
fn a_rejected_dm_leaves_no_history_behind() {
    let dir=tempfile::tempdir().unwrap();let store=Store::open(dir.path().join("test.db")).unwrap();
    let good = dm("c1", true, false);
    for broken in [
        NewDmAlert { platform: " ", ..good }, NewDmAlert { conversation_id: "", ..good },
        NewDmAlert { sender: "", ..good }, NewDmAlert { timezone: "Mars/Olympus", ..good },
        NewDmAlert { source_url: "file:///etc/hosts", ..good },
    ] {
        assert!(store.create_dm_owner_alert(&broken, 1_000).is_err());
    }
    // Nothing above consumed the conversation's one first contact.
    assert!(store.create_dm_owner_alert(&good, 1_000).unwrap().is_some());
    assert_eq!(high_alerts(&store), 1);
}

#[test]
fn an_alert_the_store_cannot_insert_consumes_neither_history_nor_the_claim() {
    let dir=tempfile::tempdir().unwrap();let store=Store::open(dir.path().join("test.db")).unwrap();
    let long = format!("https://dashboard.example.test/{}", "x".repeat(1_500));
    let good = dm("c1", true, false);
    assert!(store.create_dm_owner_alert(&NewDmAlert { source_url: &long, ..good }, 1_000).is_err());
    // The rejected attempt must not have recorded inbound history, or the
    // corrected retry would no longer count as a first contact.
    assert!(store.create_dm_owner_alert(&good, 1_000).unwrap().is_some());
    assert_eq!(high_alerts(&store), 1);
}
