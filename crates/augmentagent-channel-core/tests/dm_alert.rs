//! #2422 — the shared new-DM alert evaluator: default-off per-platform
//! policy, strict wiki identity resolution, and the eligibility classes.
use augmentagent_channel_core::dm_alert::{self, DmAlertMode, DmAlertPolicy, DmEvent};
use augmentagent_store::{owner_alerts::Urgency, Store};
use augmentagent_wiki::{IdentityIndex, WikiLayout};

const PLATFORM: &str = "imessage";
/// `KNOWN` is declared on the wiki person page built below; `STRANGER` is on
/// no wiki page at all.
const KNOWN: &str = "+15555550111";
const STRANGER: &str = "+15555550222";
const SOURCE: &str = "https://dashboard.example.test/history";

/// A store, and an identity index knowing exactly one person.
fn env() -> (tempfile::TempDir, Store, IdentityIndex) {
    let dir = tempfile::tempdir().unwrap();
    let layout = WikiLayout::new(dir.path().join("wiki"));
    std::fs::create_dir_all(layout.people_dir()).unwrap();
    let page = "---\nkind: person\nkey: ada-example\nidentities:\n  imessage:\n    - \"+15555550111\"\n  email:\n    - ada@example.com\n---\n";
    std::fs::write(layout.people_dir().join("ada-example.md"), page).unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    (dir, store, IdentityIndex::build(&layout).unwrap())
}

fn high_alerts(store: &Store) -> i64 {
    let sql = "SELECT COUNT(*) FROM owner_alerts WHERE urgency='high'";
    store.with_conn(|c| c.query_row(sql, [], |r| r.get(0))).unwrap()
}

#[test]
fn policy_parses_exact_entries_and_otherwise_stays_off() {
    let mode = |raw| DmAlertPolicy::parse(raw).mode(PLATFORM);
    assert_eq!(mode("imessage=priority"), DmAlertMode::Priority);
    assert_eq!(mode("imessage=first-contact"), DmAlertMode::FirstContact);
    assert_eq!(mode("imessage=all"), DmAlertMode::All);
    assert_eq!(mode("slack=all,imessage=priority"), DmAlertMode::Priority);
    // Fail closed on an unknown mode, stray spacing, wrong case, a platform
    // declared twice (even identically), a malformed entry, a lookalike name.
    for raw in [
        "", "imessage=off", "imessage=everything", "imessage = all", "imessage=ALL",
        "IMESSAGE=all", "imessage=all,imessage=all", "imessage=all,imessage=off",
        "imessage=bogus,imessage=all", "imessage", "=all", "imessages=all", "slack=all",
    ] {
        assert_eq!(mode(raw), DmAlertMode::Off, "policy {raw:?}");
    }
    // A rejected entry must not take a sibling platform down with it.
    let siblings = DmAlertPolicy::parse("imessage=bogus,slack=all").mode("slack");
    assert_eq!(siblings, DmAlertMode::All);
}

#[test]
fn eligibility_table() {
    // (policy, handle, owner priority selector, shape, alerts), where `shape`
    // mutates the default event: inbound, 1:1, from a human, no history.
    type Shape = fn(&mut DmEvent<'_>);
    let plain: Shape = |_| {};
    let established: Shape = |e| e.prior_inbound = true;
    let high = Some(("person:ada-example", Urgency::High));
    let cases: [(&str, &str, Option<(&str, Urgency)>, Shape, bool); 16] = [
        ("", STRANGER, None, plain, false),
        ("imessage=first-contact", STRANGER, None, plain, true),
        ("imessage=first-contact", STRANGER, None, established, false),
        // `first-contact` never promotes a priority contact on its own.
        ("imessage=first-contact", KNOWN, high, established, false),
        ("imessage=priority", STRANGER, None, plain, false),
        ("imessage=priority", KNOWN, high, plain, true),
        ("imessage=priority", KNOWN, high, established, true),
        ("imessage=priority", KNOWN, None, plain, false),
        // A selector cannot elevate a handle the wiki does not claim.
        ("imessage=priority", STRANGER, high, established, false),
        ("imessage=priority", KNOWN, Some(("ada@example.com", Urgency::High)), established, true),
        ("imessage=all", KNOWN, Some(("person:ada-example", Urgency::Routine)), established, false),
        ("imessage=all", STRANGER, None, plain, true),
        ("imessage=all", STRANGER, None, |e| e.direct_message = false, false),
        ("imessage=all", STRANGER, None, |e| e.from_self = true, false),
        ("imessage=all", STRANGER, None, |e| e.from_bot = true, false),
        ("imessage=all", STRANGER, None, |e| e.inbound = false, false),
    ];

    for (policy, handle, selector, shape, alerts) in cases {
        let (_dir, store, index) = env();
        if let Some((selector, level)) = selector {
            store.set_owner_alert_priority_identities(selector, level, &[]).unwrap();
        }
        let mut event = DmEvent::inbound_dm(PLATFORM, "conversation-1", handle, SOURCE);
        shape(&mut event);
        let policy = DmAlertPolicy::parse(policy);
        let created = dm_alert::evaluate(&store, &policy, &event, &index, 1_000).unwrap();
        assert_eq!(created.is_some(), alerts, "{policy:?} from {handle}");
        assert_eq!(high_alerts(&store), i64::from(alerts), "{policy:?} from {handle}");
    }
}
