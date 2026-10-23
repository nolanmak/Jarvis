//! #1412 — `actions.originalBody` duplicated `emails.body` for the rows that
//! never produce a reply (most of them), and was ~40% of the database file.
//! Compaction drops only an exact duplicate, and readers fall back to the
//! email's own body, so nothing observable changes.

use augmentagent_store::Store;

const DAY_MS: i64 = 86_400_000;
const NOW: i64 = 1_800_000_000_000;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    (dir, store)
}

/// One email (when `email_body` is given) and one action referencing it.
fn seed(store: &Store, id: &str, status: &str, age_days: i64, body: Option<&str>, email_body: Option<&str>) {
    let created = NOW - age_days * DAY_MS;
    store
        .with_conn(|c| {
            if let Some(eb) = email_body {
                c.execute(
                    "INSERT INTO emails (messageId, fromEmail, subject, body, receivedAt, firstSeenAt) \
                     VALUES (?1, 'a@example.test', 's', ?2, ?3, ?3)",
                    rusqlite::params![format!("m-{id}"), eb, created],
                )?;
            }
            c.execute(
                "INSERT INTO actions (id, messageId, fromEmail, subject, originalBody, status, createdAt, updatedAt) \
                 VALUES (?1, ?2, 'a@example.test', 's', ?3, ?4, ?5, ?5)",
                rusqlite::params![id, format!("m-{id}"), body, status, created],
            )?;
            Ok(())
        })
        .unwrap();
}

fn raw_body(store: &Store, id: &str) -> Option<String> {
    store
        .with_conn(|c| c.query_row("SELECT originalBody FROM actions WHERE id = ?1", [id], |r| r.get(0)))
        .unwrap()
}

#[test]
fn only_an_old_terminal_row_with_an_identical_email_body_is_compacted() {
    let (_d, store) = store();
    let body = "x".repeat(1000);
    seed(&store, "old-skipped", "skipped", 40, Some(&body), Some(&body));
    seed(&store, "old-permanent", "permanent_error", 40, Some(&body), Some(&body));
    seed(&store, "recent-skipped", "skipped", 5, Some(&body), Some(&body));
    seed(&store, "old-pending", "pending", 40, Some(&body), Some(&body));
    seed(&store, "old-sent", "sent", 40, Some(&body), Some(&body));
    seed(&store, "old-flagged", "flagged", 40, Some(&body), Some(&body));
    seed(&store, "differs", "skipped", 40, Some("{\"merge\":1}"), Some(&body));
    seed(&store, "no-email", "skipped", 40, Some(&body), None);
    seed(&store, "empty-email", "skipped", 40, Some(&body), Some(""));

    let cutoff = NOW - 30 * DAY_MS;
    let pending = store.action_body_compaction_candidates(cutoff).unwrap();
    assert_eq!((pending.rows, pending.bytes), (2, 2000));

    let done = store.compact_action_bodies(cutoff, 100).unwrap();
    assert_eq!((done.rows, done.bytes), (2, 2000));
    assert_eq!(raw_body(&store, "old-skipped"), None);
    assert_eq!(raw_body(&store, "old-permanent"), None);
    for kept in ["recent-skipped", "old-pending", "old-sent", "old-flagged", "no-email", "empty-email"] {
        assert_eq!(raw_body(&store, kept).as_deref(), Some(body.as_str()), "{kept} must keep its body");
    }
    assert_eq!(raw_body(&store, "differs").as_deref(), Some("{\"merge\":1}"));

    // Idempotent: nothing left to do.
    let again = store.compact_action_bodies(cutoff, 100).unwrap();
    assert_eq!((again.rows, again.bytes), (0, 0));
    assert_eq!(store.action_body_compaction_candidates(cutoff).unwrap().rows, 0);
}

#[test]
fn a_batch_limit_bounds_one_pass() {
    let (_d, store) = store();
    for i in 0..5 {
        seed(&store, &format!("a{i}"), "skipped", 40, Some("body"), Some("body"));
    }
    let cutoff = NOW - 30 * DAY_MS;
    assert_eq!(store.compact_action_bodies(cutoff, 2).unwrap().rows, 2);
    assert_eq!(store.compact_action_bodies(cutoff, 2).unwrap().rows, 2);
    assert_eq!(store.compact_action_bodies(cutoff, 2).unwrap().rows, 1);
    assert_eq!(store.compact_action_bodies(cutoff, 2).unwrap().rows, 0);
}

#[test]
fn a_compacted_row_still_reads_its_body_and_no_row_or_decision_is_lost() {
    let (_d, store) = store();
    seed(&store, "a", "skipped", 40, Some("the original message"), Some("the original message"));
    let before = store.get_action_with_email("a").unwrap().expect("row");
    assert_eq!(before.action.original_body.as_deref(), Some("the original message"));

    store.compact_action_bodies(NOW - 30 * DAY_MS, 100).unwrap();

    let after = store.get_action_with_email("a").unwrap().expect("the row is still there");
    assert_eq!(
        after.action.original_body.as_deref(),
        Some("the original message"),
        "readers fall back to the email's body"
    );
    assert_eq!(after.email.body, "the original message");
    let (status, n): (String, i64) = store
        .with_conn(|c| c.query_row("SELECT status, (SELECT count(*) FROM actions) FROM actions WHERE id='a'", [], |r| Ok((r.get(0)?, r.get(1)?))))
        .unwrap();
    assert_eq!((status.as_str(), n), ("skipped", 1), "the decision is evaluation data (#448)");
}

#[test]
fn a_row_that_never_had_a_body_and_can_still_act_does_not_gain_one() {
    // The fallback applies to compactable statuses only: a pending card with
    // no body must keep reading as "no body".
    let (_d, store) = store();
    seed(&store, "p", "pending", 1, None, Some("email text"));
    let row = store.get_action_with_email("p").unwrap().unwrap();
    assert_eq!(row.action.original_body, None);
}

#[test]
fn a_batch_waits_out_another_writer_instead_of_failing_locked() {
    // Live failure: the daemon committed between a batch's read and its
    // write, and the deferred transaction died with "database is locked".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    seed(&store, "a", "skipped", 40, Some("body"), Some("body"));

    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE; UPDATE actions SET subject = 'touched' WHERE id = 'a';").unwrap();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        other.execute_batch("COMMIT;").unwrap();
    });

    let done = store.compact_action_bodies(NOW - 30 * DAY_MS, 100).expect("waits, then compacts");
    writer.join().unwrap();
    assert_eq!(done.rows, 1);
    assert_eq!(raw_body(&store, "a"), None);
}
