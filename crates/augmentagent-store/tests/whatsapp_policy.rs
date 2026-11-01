//! #1763 — WhatsApp owner reply policy: defaults cover people and groups,
//! never broadcasts or newsletters; denials win and persist.

use augmentagent_store::{whatsapp_default_coverable, Store};

const DM: &str = "14155550123@s.whatsapp.net"; // pii-ok synthetic
const LID: &str = "111111111111111@lid"; // pii-ok synthetic
const GROUP: &str = "120363001234567890@g.us";
const OLD_GROUP: &str = "14155550123-1600000000@g.us"; // pii-ok synthetic

fn fresh() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Store::open(dir.path().join("data.db")).unwrap(), dir)
}

#[test]
fn whatsapp_contact_defaults_cover_dms_and_groups_but_never_broadcast_or_newsletters() {
    let (store, _d) = fresh();
    for jid in [DM, LID, GROUP] {
        assert!(!store.is_whatsapp_inbound_allowed(jid).unwrap(), "off by default: {jid}");
    }
    store.set_whatsapp_contact_defaults(true, false).unwrap();
    for jid in [DM, LID, GROUP, OLD_GROUP] {
        assert!(store.is_whatsapp_inbound_allowed(jid).unwrap(), "{jid}");
        assert!(!store.is_whatsapp_outbound_allowed(jid).unwrap(), "outbound stays off: {jid}");
    }
    for jid in [
        "status@broadcast",
        "123@broadcast",
        "120363000000000000@newsletter",
        "abc@s.whatsapp.net",
        "",
        "@g.us",
        "1@g.us\n",
    ] {
        assert!(!whatsapp_default_coverable(jid), "{jid:?}");
        assert!(!store.is_whatsapp_inbound_allowed(jid).unwrap(), "{jid:?}");
    }
    assert!(store.whatsapp_contact_default(false).unwrap());
    assert!(!store.whatsapp_contact_default(true).unwrap());
}

#[test]
fn whatsapp_denial_wins_over_defaults_and_persists_across_reopen() {
    let (store, dir) = fresh();
    store.set_whatsapp_contact_defaults(true, true).unwrap();
    store.deny_whatsapp_inbound(GROUP).unwrap();
    store.deny_whatsapp_outbound(DM).unwrap();
    drop(store);
    let store = Store::open(dir.path().join("data.db")).unwrap();
    assert!(!store.is_whatsapp_inbound_allowed(GROUP).unwrap());
    assert!(store.is_whatsapp_outbound_allowed(GROUP).unwrap());
    assert!(!store.is_whatsapp_outbound_allowed(DM).unwrap());
    assert!(store.is_whatsapp_inbound_allowed(DM).unwrap());
    let (entries, denials) = store.list_whatsapp_policy(false).unwrap();
    assert!(entries.is_empty());
    assert_eq!(denials, vec![GROUP.to_string()]);
}

#[test]
fn whatsapp_explicit_allow_lifts_a_denial_and_works_without_defaults() {
    let (store, _d) = fresh();
    store.deny_whatsapp_inbound(GROUP).unwrap();
    store.allow_whatsapp_inbound(GROUP).unwrap();
    assert!(store.is_whatsapp_inbound_allowed(GROUP).unwrap());
    assert_eq!(store.list_whatsapp_policy(false).unwrap(), (vec![GROUP.to_string()], vec![]));
    store.deny_whatsapp_inbound(GROUP).unwrap();
    assert!(!store.is_whatsapp_inbound_allowed(GROUP).unwrap());
    assert_eq!(store.list_whatsapp_policy(false).unwrap(), (vec![], vec![GROUP.to_string()]));
}
