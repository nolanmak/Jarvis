//! #1833 C1 exit tests — config loader, handles, trust state. Example data
//! only (`*.example.test`, 555-01xx numbers).

use std::path::{Path, PathBuf};

use augmentagent_autonomy::{
    confirm_contact, load_config, normalize_handle, reconcile, Handle, LoadReport, Platform, Tier,
    TrustState,
};
use augmentagent_store::Store;

const NOW: i64 = 1_800_000_000_000;

fn report(text: &str) -> LoadReport {
    LoadReport::from_text(Path::new("trusted-contacts.toml"), text)
}

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    (dir, store)
}

const JANE: &str = r#"
[[contact]]
wiki = "jane-doe"
tier = "actions"
allowed_actions = ["calendar.add_guest", "calendar.send_details"]
handles = ["imessage:+12025550100", "email:jane@work.example.test"]
calendar_email = "jane@work.example.test"
"#;

const SAM: &str = r#"
[[contact]]
wiki = "sam-roe"
tier = "full"
handles = ["email:sam@example.test"]
"#;

fn slugs(r: &LoadReport) -> Vec<&str> {
    r.contacts.iter().map(|c| c.slug.as_str()).collect()
}

fn problem_for<'a>(r: &'a LoadReport, slug: &str) -> &'a str {
    r.problems
        .iter()
        .find(|p| p.slug.as_deref() == Some(slug))
        .map(|p| p.reason.as_str())
        .unwrap_or_else(|| panic!("no problem recorded for {slug}: {:?}", r.problems))
}

// ----- loader ---------------------------------------------------------------

#[test]
fn parse_error_loads_empty_and_alerts() {
    let r = report(&format!("{JANE}\n[[contact]\nwiki = broken"));
    assert!(
        r.contacts.is_empty(),
        "a parse error must load nothing: {:?}",
        r.contacts
    );
    let fatal = r.fatal.as_deref().expect("parse error must be fatal");
    assert!(!fatal.is_empty());
    let alerts = r.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains("trusted-contacts.toml"), "{alerts:?}");
}

#[test]
fn per_contact_error_disables_only_that_contact() {
    let bad = r#"
[[contact]]
wiki = "bad-actor"
tier = "actions"
allowed_actions = ["wire.money"]
handles = ["email:bad@example.test"]
"#;
    let r = report(&format!("{JANE}{bad}{SAM}"));
    assert!(r.fatal.is_none());
    assert_eq!(slugs(&r), ["jane-doe", "sam-roe"]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(problem_for(&r, "bad-actor").contains("wire.money"));
    let alerts = r.alerts();
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].contains("bad-actor"), "{alerts:?}");
}

#[test]
fn missing_tier_rejected() {
    let r = report(
        r#"
[[contact]]
wiki = "no-tier"
handles = ["email:nt@example.test"]

[[contact]]
wiki = "odd-tier"
tier = "everything"
handles = ["email:ot@example.test"]
"#,
    );
    assert!(r.contacts.is_empty(), "{:?}", r.contacts);
    assert!(problem_for(&r, "no-tier").contains("tier"));
    assert!(problem_for(&r, "odd-tier").contains("tier"));
}

#[test]
fn noncanonical_handle_rejected() {
    for handle in [
        "email:Jane@Work.Example.Test",
        "imessage:(202) 555-0100",
        "imessage:+1 202 555 0100",
        "phone:+12025550100",
        "+12025550100",
        "email: jane@work.example.test",
    ] {
        let r = report(&format!(
            "[[contact]]\nwiki = \"jane-doe\"\ntier = \"full\"\nhandles = [\"{handle}\"]\n"
        ));
        assert!(r.contacts.is_empty(), "{handle} must be rejected");
        assert!(
            problem_for(&r, "jane-doe").contains("handle"),
            "{handle}: {:?}",
            r.problems
        );
    }
}

#[test]
fn calendar_email_must_be_a_handle() {
    let r = report(
        r#"
[[contact]]
wiki = "jane-doe"
tier = "actions"
allowed_actions = ["calendar.add_guest"]
handles = ["imessage:+12025550100"]
calendar_email = "jane@work.example.test"
"#,
    );
    assert!(r.contacts.is_empty());
    assert!(problem_for(&r, "jane-doe").contains("calendar_email"));
    // Present among handles → loads.
    assert_eq!(slugs(&report(JANE)), ["jane-doe"]);
}

#[test]
fn duplicate_handle_disables_both() {
    let twin = r#"
[[contact]]
wiki = "jane-twin"
tier = "full"
handles = ["imessage:+12025550100"]
"#;
    let r = report(&format!("{JANE}{twin}{SAM}"));
    assert_eq!(
        slugs(&r),
        ["sam-roe"],
        "both sharers disabled, the rest loads"
    );
    assert!(problem_for(&r, "jane-doe").contains("imessage:+12025550100"));
    assert!(problem_for(&r, "jane-twin").contains("imessage:+12025550100"));
}

#[test]
fn full_tier_with_allowed_actions_rejected() {
    let r = report(
        r#"
[[contact]]
wiki = "sam-roe"
tier = "full"
allowed_actions = ["calendar.send_details"]
handles = ["email:sam@example.test"]
"#,
    );
    assert!(r.contacts.is_empty());
    assert!(problem_for(&r, "sam-roe").contains("allowed_actions"));
    // `reply` is never listable, even for the actions tier.
    let r = report(
        r#"
[[contact]]
wiki = "sam-roe"
tier = "actions"
allowed_actions = ["reply"]
handles = ["email:sam@example.test"]
"#,
    );
    assert!(r.contacts.is_empty());
    assert!(problem_for(&r, "sam-roe").contains("reply"));
}

#[test]
fn defaults_preferred_channel_and_daily_cap() {
    let r = report(&format!("{JANE}{SAM}"));
    let jane = r.contact("jane-doe").unwrap();
    assert_eq!(jane.tier, Tier::Actions);
    assert_eq!(jane.daily_cap, 10);
    assert_eq!(jane.preferred_handle.canonical(), "imessage:+12025550100");
    assert_eq!(
        jane.calendar_email.as_deref(),
        Some("jane@work.example.test")
    );
    assert!(jane.allows("calendar.add_guest"));
    assert!(!jane.allows("reply"));
    let sam = r.contact("sam-roe").unwrap();
    assert_eq!(sam.preferred_handle.canonical(), "email:sam@example.test");
    assert!(sam.allows("reply") && sam.allows("calendar.send_details"));
    assert!(!sam.allows("wire.money"));
}

#[test]
fn missing_file_is_empty_without_alert() {
    let dir = tempfile::tempdir().unwrap();
    let r = load_config(&dir.path().join("absent.toml"));
    assert!(r.contacts.is_empty() && r.problems.is_empty() && r.fatal.is_none());
    assert!(r.file_hash.is_none());
    assert!(r.alerts().is_empty());
}

#[test]
fn missing_wiki_slug_keeps_contact_and_notices() {
    let (_d, store) = temp_store();
    let wiki = tempfile::tempdir().unwrap();
    let people = wiki.path().join("people");
    std::fs::create_dir_all(&people).unwrap();
    std::fs::write(people.join("sam-roe.md"), "---\nkind: person\n---\n").unwrap();

    let snap = reconcile(&store, report(&format!("{JANE}{SAM}")), Some(&people), NOW).unwrap();
    confirm_contact(&store, &snap.report, "jane-doe", NOW).unwrap();
    assert_eq!(snap.slug_notices.len(), 1, "{:?}", snap.slug_notices);
    assert!(snap.slug_notices[0].contains("jane-doe"));
    assert!(snap.slug_notices[0].contains("trust sync"));

    // The contact stays loaded and, once confirmed, trusted.
    let snap = reconcile(
        &store,
        report(&format!("{JANE}{SAM}")),
        Some(&people),
        NOW + 1,
    )
    .unwrap();
    assert!(snap.is_active("jane-doe"), "missing page must not untrust");
    // One-time: the next load does not repeat the notice.
    assert!(snap.slug_notices.is_empty(), "{:?}", snap.slug_notices);
}

// ----- handles --------------------------------------------------------------

#[test]
fn normalize_email_lowercases() {
    let h = normalize_handle("email", "  Jane.Doe+Work@Work.Example.Test ").unwrap();
    assert_eq!(h.canonical(), "email:jane.doe+work@work.example.test");
    assert_eq!(h.platform(), Platform::Email);
    // Inbound Gmail senders arrive as `Name <addr>` on platform `gmail`.
    let h = normalize_handle("gmail", "Jane Doe <Jane@Work.Example.Test>").unwrap();
    assert_eq!(h.canonical(), "email:jane@work.example.test");
    assert!(normalize_handle("email", "not-an-address").is_err());
}

#[test]
fn normalize_imessage_phone_to_e164() {
    for raw in [
        "(202) 555-0100",
        "202-555-0100",
        "+1 202 555 0100",
        "+12025550100",
    ] {
        let h = normalize_handle("imessage", raw).unwrap();
        assert_eq!(h.canonical(), "imessage:+12025550100", "{raw}");
    }
    assert!(normalize_handle("imessage", "12").is_err());
}

#[test]
fn normalize_imessage_appleid_lowercases() {
    let h = normalize_handle("imessage", " Sam@iCloud.Example.Test ").unwrap();
    assert_eq!(h.canonical(), "imessage:sam@icloud.example.test");
    assert_eq!(h.platform(), Platform::Imessage);
    // Same address on email is a different handle.
    assert_ne!(
        h,
        normalize_handle("email", "sam@icloud.example.test").unwrap()
    );
    assert!(normalize_handle("whatsapp", "x").is_err());
}

#[test]
fn canonical_round_trip() {
    let h = Handle::parse_canonical("imessage:+12025550100").unwrap();
    assert_eq!(h.to_string(), "imessage:+12025550100");
    assert!(Handle::parse_canonical("imessage:2025550100").is_err());
}

// ----- trust state ----------------------------------------------------------

#[test]
fn changed_entry_becomes_pending_until_confirm() {
    let (_d, store) = temp_store();
    let snap = reconcile(&store, report(JANE), None, NOW).unwrap();
    assert_eq!(snap.newly_pending, ["jane-doe"]);
    assert!(snap.active.is_empty(), "a new contact is pending");
    assert_eq!(snap.state_of("jane-doe"), Some(TrustState::Pending));
    let h = normalize_handle("imessage", "+12025550100").unwrap();
    assert!(snap.active_contact_for(&h).is_none());

    assert!(confirm_contact(&store, &snap.report, "jane-doe", NOW).unwrap());
    let snap = reconcile(&store, report(JANE), None, NOW + 1).unwrap();
    assert!(snap.newly_pending.is_empty());
    assert_eq!(
        snap.active_contact_for(&h).map(|c| c.slug.as_str()),
        Some("jane-doe")
    );

    // Tier change → pending again, until confirmed.
    let widened = JANE.replace(
        "tier = \"actions\"\nallowed_actions = [\"calendar.add_guest\", \"calendar.send_details\"]",
        "tier = \"full\"",
    );
    let snap = reconcile(&store, report(&widened), None, NOW + 2).unwrap();
    assert_eq!(snap.newly_pending, ["jane-doe"]);
    assert!(
        snap.active_contact_for(&h).is_none(),
        "changed entry must not stay trusted"
    );
    assert!(confirm_contact(&store, &snap.report, "jane-doe", NOW + 3).unwrap());
    let snap = reconcile(&store, report(&widened), None, NOW + 4).unwrap();
    assert_eq!(snap.active_contact_for(&h).unwrap().tier, Tier::Full);

    // Handle change → pending again.
    let more = widened.replace(
        "handles = [\"imessage:+12025550100\", \"email:jane@work.example.test\"]",
        "handles = [\"imessage:+12025550100\", \"email:jane@work.example.test\", \"email:jane@home.example.test\"]",
    );
    let snap = reconcile(&store, report(&more), None, NOW + 5).unwrap();
    assert!(!snap.is_active("jane-doe"));

    // Unknown slug cannot be confirmed.
    assert!(confirm_contact(&store, &snap.report, "nobody", NOW).is_err());
}

#[test]
fn hash_change_while_down_detected_at_startup() {
    let (dir, store) = temp_store();
    let path: PathBuf = dir.path().join("trusted-contacts.toml");
    std::fs::write(&path, JANE).unwrap();
    let snap = reconcile(&store, load_config(&path), None, NOW).unwrap();
    assert!(snap.file_changed, "first load records the hash");
    confirm_contact(&store, &snap.report, "jane-doe", NOW).unwrap();
    let snap = reconcile(&store, load_config(&path), None, NOW + 1).unwrap();
    assert!(!snap.file_changed);
    assert!(snap.is_active("jane-doe"));
    drop(store);

    // "Daemon down": the file is edited with no process watching.
    let planted = format!(
        "{}\n{SAM}",
        JANE.replace("tier = \"actions\"\nallowed_actions = [\"calendar.add_guest\", \"calendar.send_details\"]", "tier = \"full\"")
    );
    std::fs::write(&path, planted).unwrap();

    // "Startup": a fresh store handle on the same db sees the change.
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let snap = reconcile(&store, load_config(&path), None, NOW + 2).unwrap();
    assert!(snap.file_changed, "edit made while down must be detected");
    assert!(!snap.is_active("jane-doe"), "edited entry is pending");
    assert!(!snap.is_active("sam-roe"), "planted entry is pending");
    let mut np = snap.newly_pending.clone();
    np.sort();
    assert_eq!(np, ["jane-doe", "sam-roe"]);
    assert_eq!(store.autonomy_config_hash().unwrap(), snap.report.file_hash);
}

#[test]
fn example_config_parses_and_contains_only_example_domains() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/trusted-contacts.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let r = load_config(&path);
    assert!(r.fatal.is_none(), "{:?}", r.fatal);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert!(r.contacts.len() >= 2, "example shows both tiers");
    assert!(r.contacts.iter().any(|c| c.tier == Tier::Actions));
    assert!(r.contacts.iter().any(|c| c.tier == Tier::Full));
    for c in &r.contacts {
        for h in &c.handles {
            match h.value().split_once('@') {
                Some((_, domain)) => assert!(
                    domain == "example.test" || domain.ends_with(".example.test"),
                    "non-example domain in {h}"
                ),
                None => assert!(
                    h.value().len() == 12 && h.value()[5..].starts_with("55501"),
                    "non-fictional number in {h}"
                ),
            }
        }
    }
    // Nothing that looks like an address outside the reserved test domains,
    // anywhere in the file (comments included).
    let re = regex::Regex::new(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9.-]+)").unwrap();
    for cap in re.captures_iter(&text) {
        let d = &cap[1];
        assert!(
            d.ends_with("example.test"),
            "non-example domain `{d}` in example config"
        );
    }
}
