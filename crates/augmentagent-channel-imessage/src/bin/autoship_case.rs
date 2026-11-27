//! #2422 — new-DM alerts: off until the owner opts the platform in, one per
//! conversation per hour, delivered only to the owner's own destination.
use augmentagent_channel_core::dm_alert::{self, DmAlertPolicy, DmEvent};
use augmentagent_store::Store;
use augmentagent_wiki::IdentityIndex;
use serde_json::json;

fn main() {
    let root = std::env::temp_dir().join(format!("autoship-2422-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(root.join("alerts.db")).unwrap();
    store.configure_owner_alert_texts(Some("owner-destination"), true).unwrap();

    let index = IdentityIndex::default();
    let alerted = |policy: &str, conversation: &str, group: bool, now| {
        let url = "http://localhost:3000/history";
        let mut event = DmEvent::inbound_dm("imessage", conversation, "+10000000001", url);
        event.direct_message = !group;
        dm_alert::evaluate(&store, &DmAlertPolicy::parse(policy), &event, &index, now)
            .unwrap().is_some()
    };
    let off = alerted("", "c1", false, 1_000);
    let malformed = alerted("imessage=all,imessage=priority", "c2", false, 1_000);
    let group = alerted("imessage=all", "c3", true, 1_000);
    let first = alerted("imessage=first-contact", "c4", false, 1_000);
    let repeated = alerted("imessage=first-contact", "c4", false, 2_000);

    store.enqueue_owner_alert_texts(3_000).unwrap();
    let outbox = store.list_imessage_outbox(20).unwrap();
    let owner_only = outbox.iter().all(|item| item.target == "owner-destination");
    println!("{}", json!([off, malformed, group, first, repeated, outbox.len(), owner_only]));
    let _ = std::fs::remove_dir_all(root);
}
