//! #1833 — the single trusted-contacts writer round-trips through the loader.

use augmentagent_autonomy::writer::{read_entries, write_contacts};
use augmentagent_autonomy::{load_config, ContactEntry};

#[test]
fn written_file_loads_back_and_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/trusted-contacts.toml");
    assert!(
        read_entries(&path).unwrap().is_empty(),
        "missing file is empty"
    );
    let jane = ContactEntry {
        wiki: "jane-doe".into(),
        tier: Some("actions".into()),
        allowed_actions: vec!["calendar.send_details".into()],
        handles: vec![
            "imessage:+12025550100".into(),
            "email:jane@work.example.test".into(),
        ],
        calendar_email: Some("jane@work.example.test".into()),
        preferred_channel: None,
        daily_cap: Some(3),
    };
    let sam = ContactEntry {
        wiki: "sam-roe".into(),
        tier: Some("full".into()),
        handles: vec!["email:sam@example.test".into()],
        ..ContactEntry::default()
    };
    write_contacts(&path, &[jane.clone(), sam.clone()]).unwrap();
    assert_eq!(read_entries(&path).unwrap(), vec![jane, sam]);
    let r = load_config(&path);
    assert!(
        r.fatal.is_none() && r.problems.is_empty(),
        "{:?} {:?}",
        r.fatal,
        r.problems
    );
    assert_eq!(r.contact("jane-doe").unwrap().daily_cap, 3);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    write_contacts(&path, &[]).unwrap();
    assert!(load_config(&path).contacts.is_empty());
    assert!(load_config(&path).fatal.is_none());

    std::fs::write(&path, "[[contact]\n").unwrap();
    let err = read_entries(&path).unwrap_err().to_string();
    assert!(err.contains("by hand"), "{err}");
}
