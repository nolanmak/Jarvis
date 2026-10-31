use augmentagent_store::conversation_context::ConversationScope;
use augmentagent_store::{rusqlite::params, Store};

#[test]
fn window_crosses_batches_preserves_direction_and_excludes_drafts_and_other_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    store.with_conn(|c| {
        for i in 0..12 {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,firstSeenAt,platform,accountEntityId,kind)
                VALUES (?1,'thread',?2,'subject',?3,?4,999999,'imessage','account','dm')",
                params![format!("m{i:02}"), if i % 2 == 0 {"me"} else {"peer"}, format!("body{i}"), format!("2026-01-01T00:{i:02}:00Z")])?;
        }
        for (id,platform,account,kind) in [("draft","imessage","account","compose"),("other-account","imessage","other","dm"),("other-platform","whatsapp","account","dm")] {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,firstSeenAt,platform,accountEntityId,kind)
                VALUES (?1,'thread','me','subject','not context','2026-01-01T00:11:30Z',999999,?2,?3,?4)",params![id,platform,account,kind])?;
        }
        Ok(())
    }).unwrap();
    let scope = ConversationScope {
        platform: "imessage",
        account_id: Some("account"),
        thread_id: "thread",
    };
    let rows = store.conversation_window(&scope, i64::MAX, 10).unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows.first().unwrap().message_id, "m02");
    assert_eq!(rows.last().unwrap().message_id, "m11");
    assert_eq!(rows[0].sender, "me");
    assert_eq!(rows[1].sender, "peer");
    assert_eq!(rows[0].body, "body2");
    let cutoff = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:05:00Z")
        .unwrap()
        .timestamp_millis();
    let historical = store.conversation_window(&scope, cutoff, 10).unwrap();
    assert_eq!(historical.len(), 6);
    assert_eq!(historical.last().unwrap().message_id, "m05");
    assert!(store
        .conversation_window(
            &ConversationScope {
                account_id: None,
                ..scope
            },
            i64::MAX,
            10
        )
        .unwrap()
        .is_empty());
}

#[test]
fn window_orders_instants_not_offset_strings_and_reports_missing_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    store.with_conn(|c| {
        for (id,ts,seen) in [("early",Some("2026-01-01T01:00:00+02:00"),9),("late",Some("2026-01-01T00:00:00Z"),1),("unknown",None,0)] {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,firstSeenAt,platform,kind)
                VALUES (?1,'thread','peer','subject','body',?2,?3,'imessage','dm')",params![id,ts,seen])?;
        }
        Ok(())
    }).unwrap();
    let scope = ConversationScope {
        platform: "imessage",
        account_id: None,
        thread_id: "thread",
    };
    let rows = store.conversation_window(&scope, i64::MAX, 10).unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["unknown", "early", "late"]
    );
    assert!(rows[0].timestamp.is_none());
    assert!(store
        .conversation_window(&scope, i64::MAX, 0)
        .unwrap()
        .is_empty());
}
