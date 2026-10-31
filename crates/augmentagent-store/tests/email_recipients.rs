//! #1699 — first-class, normalized To/Cc recipients on `emails`, recovered
//! from `retryGuardContext` for rows written before the columns existed.
//! Temporary stores and synthetic addresses only.

use augmentagent_store::{Email, Recipients, Store};

fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn email(id: &str, platform: &str, to: &str, cc: &str) -> Email {
    Email {
        message_id: id.into(),
        thread_id: Some("T1".into()),
        from: "Asker <asker@example.org>".into(),
        to: to.into(),
        cc: cc.into(),
        attachments: Vec::new(),
        subject: "s".into(),
        body: "b".into(),
        date: "2026-01-01T00:00:00Z".into(),
        account_entity_id: Some("acc".into()),
        platform: platform.into(),
        kind: "dm".into(),
    }
}

fn r(to: &[&str], cc: &[&str]) -> Option<Recipients> {
    Some(Recipients {
        to: to.iter().map(|s| s.to_string()).collect(),
        cc: cc.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn upsert_persists_normalized_recipients() {
    let (_d, _p, store) = temp_store();
    store
        .upsert_email(&email(
            "m1",
            "gmail",
            r#""Doe, Jane" <Jane@Example.COM>, bo@example.com , JANE@example.com"#,
            "  Cy <cy@example.net>",
        ))
        .unwrap();
    assert_eq!(
        store.email_recipients("m1").unwrap(),
        r(&["jane@example.com", "bo@example.com"], &["cy@example.net"])
    );
    // A re-seen message refreshes the recipients.
    store.upsert_email(&email("m1", "gmail", "dee@example.com", "")).unwrap();
    assert_eq!(store.email_recipients("m1").unwrap(), r(&["dee@example.com"], &[]));
}

#[test]
fn missing_recipients_are_null_not_empty() {
    let (_d, _p, store) = temp_store();
    // Gmail row with no recipient headers at all: unknown, not "nobody".
    store.upsert_email(&email("m-none", "gmail", "", "")).unwrap();
    assert_eq!(store.email_recipients("m-none").unwrap(), None);
    // Chat platforms have no To/Cc.
    store.upsert_email(&email("m-chat", "whatsapp", "", "")).unwrap();
    assert_eq!(store.email_recipients("m-chat").unwrap(), None);
    // Unknown message id.
    assert_eq!(store.email_recipients("nope").unwrap(), None);
}

/// Simulate a store written before #1699: drop the new columns, keep the
/// `retryGuardContext` tuple, reopen.
fn downgrade(path: &std::path::Path, store: Store) {
    store
        .with_conn(|c| {
            c.execute_batch(
                "ALTER TABLE emails DROP COLUMN toAddrs; ALTER TABLE emails DROP COLUMN ccAddrs;",
            )
        })
        .unwrap();
    drop(store);
    let _ = path;
}

#[test]
fn backfill_recovers_from_retry_guard_context() {
    let (_d, path, store) = temp_store();
    store
        .upsert_email(&email("m-old", "gmail", "Ana <ana@example.com>", "bo@example.com"))
        .unwrap();
    store.upsert_email(&email("m-legacy", "gmail", "x@example.com", "")).unwrap();
    store
        .with_conn(|c| {
            c.execute("UPDATE emails SET retryGuardContext = NULL WHERE messageId = 'm-legacy'", [])
        })
        .unwrap();
    downgrade(&path, store);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.email_recipients("m-old").unwrap(), r(&["ana@example.com"], &["bo@example.com"]));
    assert_eq!(
        store.email_recipients("m-legacy").unwrap(),
        None,
        "no saved context means unknown, never empty"
    );
}

#[test]
fn backfill_is_idempotent() {
    let (_d, path, store) = temp_store();
    store.upsert_email(&email("m1", "gmail", "ana@example.com", "")).unwrap();
    downgrade(&path, store);
    let first = Store::open(&path).unwrap();
    let a = first.email_recipients("m1").unwrap();
    drop(first);
    let second = Store::open(&path).unwrap();
    assert_eq!(second.email_recipients("m1").unwrap(), a);
    assert_eq!(a, r(&["ana@example.com"], &[]));
}
