//! #2284 — the cross-channel "has the owner replied?" signal. Temporary
//! stores and synthetic ids only.

use augmentagent_store::owner_reply::conversation_ids_for;
use augmentagent_store::rusqlite::params;
use augmentagent_store::Store;

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    (dir, store)
}

/// One `message_index` row, as the extractor would write it.
fn index_row(
    store: &Store,
    message_id: &str,
    platform: &str,
    conv_kind: &str,
    conversation_id: &str,
    from_me: bool,
    ts_ms: i64,
    ts_fallback: bool,
) {
    store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO message_index(message_id, platform, conv_kind, conversation_id, \
                 conversation_title, sender_handle, from_me, ts_ms, ts_fallback, has_attachment, \
                 extractor_version) VALUES (?1, ?2, ?3, ?4, 'Title', ?5, ?6, ?7, ?8, 0, 2)",
                params![
                    message_id,
                    platform,
                    conv_kind,
                    conversation_id,
                    if from_me { "me" } else { "someone" },
                    from_me,
                    ts_ms,
                    ts_fallback
                ],
            )
        })
        .unwrap();
}

/// Each channel's card `threadId` maps to the conversation ids the index and
/// the owner ledger use. Most are the same string; WhatsApp and iMessage
/// carry a prefix on the history side that the live side may lack.
#[test]
fn every_channel_maps_its_card_thread_id_to_index_conversation_ids() {
    let cases: &[(&str, &str, &[&str])] = &[
        ("imessage", "imessage:+15550000001", &["imessage:+15550000001"]),
        ("imessage", "imessage:chat000000000001", &["imessage:chat000000000001"]),
        (
            "whatsapp",
            "whatsapp-history:15550000002@s.whatsapp.net",
            &["whatsapp-history:15550000002@s.whatsapp.net", "15550000002@s.whatsapp.net"],
        ),
        (
            "whatsapp",
            "15550000003@s.whatsapp.net",
            &["15550000003@s.whatsapp.net", "whatsapp-history:15550000003@s.whatsapp.net"],
        ),
        ("whatsapp", "whatsapp-history:000000000000000001@g.us", &[
            "whatsapp-history:000000000000000001@g.us",
            "000000000000000001@g.us",
        ]),
        (
            "socialapi",
            "00000000-0000-4000-8000-000000000001",
            &["00000000-0000-4000-8000-000000000001"],
        ),
        ("instagram", "ig-thread-0001", &["ig-thread-0001"]),
        ("gmail", "1a0000000000000a", &["1a0000000000000a"]),
        (
            "linkedin",
            "urn:li:msg_conversation:synthetic-0001",
            &["urn:li:msg_conversation:synthetic-0001"],
        ),
        ("slack", "D00000001", &["D00000001"]),
        ("discord", "100000000000000001", &["100000000000000001"]),
        ("telegram", "1000000001", &["1000000001"]),
        ("twitter", "1000000000000000001", &["1000000000000000001"]),
    ];
    for (platform, thread, want) in cases {
        assert_eq!(
            conversation_ids_for(platform, thread),
            want.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "{platform} {thread}"
        );
    }
    // Blank ids never map to anything.
    assert!(conversation_ids_for("gmail", "  ").is_empty());
}

/// Index evidence: a non-fallback owner row strictly after the bound, in the
/// same conversation and platform, is a reply. Each channel's id format.
#[test]
fn index_owner_rows_answer_every_channel_format() {
    let (_d, store) = temp_store();
    let convs = [
        ("imessage", "dm", "imessage:+15550000001"),
        ("whatsapp", "group", "whatsapp-history:000000000000000001@g.us"),
        ("linkedin", "dm", "urn:li:msg_conversation:synthetic-0001"),
        ("slack", "dm", "D00000001"),
        ("discord", "dm", "100000000000000001"),
        ("telegram", "dm", "1000000001"),
        ("twitter", "dm", "1000000000000000001"),
        ("gmail", "email", "1a0000000000000a"),
    ];
    for (i, (platform, kind, conv)) in convs.iter().enumerate() {
        index_row(&store, &format!("in-{i}"), platform, kind, conv, false, 1_000, false);
        assert!(!store.owner_replied_since(platform, conv, 1_000).unwrap(), "{platform}");
        index_row(&store, &format!("me-{i}"), platform, kind, conv, true, 2_000, false);
        assert!(store.owner_replied_since(platform, conv, 1_000).unwrap(), "{platform}");
        // Strictly after: a reply at the bound itself does not count.
        assert!(!store.owner_replied_since(platform, conv, 2_000).unwrap(), "{platform}");
        // Another platform's conversation with the same id is not this one.
        assert!(!store.owner_replied_since("other", conv, 1_000).unwrap(), "{platform}");
    }
}

/// A live WhatsApp card (bare JID) is answered by an owner row the history
/// export recorded under the prefixed id, and the other way round.
#[test]
fn whatsapp_live_and_history_ids_answer_each_other() {
    let (_d, store) = temp_store();
    index_row(
        &store,
        "me-h",
        "whatsapp",
        "dm",
        "whatsapp-history:15550000003@s.whatsapp.net",
        true,
        5_000,
        false,
    );
    assert!(store.owner_replied_since("whatsapp", "15550000003@s.whatsapp.net", 4_000).unwrap());
    index_row(&store, "me-l", "whatsapp", "dm", "15550000004@s.whatsapp.net", true, 5_000, false);
    assert!(store
        .owner_replied_since("whatsapp", "whatsapp-history:15550000004@s.whatsapp.net", 4_000)
        .unwrap());
}

/// An owner row whose time is only the ingest time can't be ordered against
/// the inbound, so it never counts.
#[test]
fn fallback_timestamps_are_not_evidence() {
    let (_d, store) = temp_store();
    index_row(&store, "me-1", "gmail", "email", "1a0000000000000b", true, 9_000, true);
    assert!(!store.owner_replied_since("gmail", "1a0000000000000b", 1_000).unwrap());
}

/// The owner ledger (Instagram/SocialAPI DMs seen in polled history) is
/// evidence, idempotent per message id.
#[test]
fn owner_ledger_records_socialapi_dms_once() {
    let (_d, store) = temp_store();
    let conv = "00000000-0000-4000-8000-000000000001";
    assert!(!store.owner_replied_since("socialapi", conv, 1_000).unwrap());
    assert!(store
        .record_owner_message("socialapi", conv, "dm-out-1", 3_000, "socialapi-poll")
        .unwrap());
    assert!(!store
        .record_owner_message("socialapi", conv, "dm-out-1", 3_000, "socialapi-webhook")
        .unwrap());
    assert!(store.owner_replied_since("socialapi", conv, 1_000).unwrap());
    assert!(!store.owner_replied_since("socialapi", conv, 3_000).unwrap());
    assert!(!store.owner_replied_since("gmail", conv, 1_000).unwrap());
}

/// Gmail: a manual reply the SENT-folder observer logged, or a send Jarvis
/// made, answers the thread.
#[test]
fn gmail_observer_and_daemon_sends_answer_the_thread() {
    let (_d, store) = temp_store();
    store
        .record_outbound_thread_event("ent-1", "sent-1", Some("1a0000000000000c"), 7_000)
        .unwrap();
    assert!(store.owner_replied_since("gmail", "1a0000000000000c", 6_999).unwrap());
    assert!(!store.owner_replied_since("gmail", "1a0000000000000c", 7_000).unwrap());
    store
        .record_self_sent_message("sent-2", Some("1a0000000000000d"), Some("ent-1"), Some("a-1"))
        .unwrap();
    assert!(store.owner_replied_since("gmail", "1a0000000000000d", 0).unwrap());
}

/// Conversations whose newest inbound has no owner message after it, newest
/// first, bounded by `limit` and `since_ms`; channels and notes are not
/// conversations anyone waits on.
#[test]
fn unanswered_conversations_lists_only_open_threads() {
    let (_d, store) = temp_store();
    // Answered in the index.
    index_row(&store, "a-in", "imessage", "dm", "imessage:+15550000001", false, 1_000, false);
    index_row(&store, "a-me", "imessage", "dm", "imessage:+15550000001", true, 1_500, false);
    // Open: owner spoke first, then the other side.
    index_row(&store, "b-me", "whatsapp", "dm", "whatsapp-history:1@s.whatsapp.net", true, 1_000, false);
    index_row(&store, "b-in", "whatsapp", "dm", "whatsapp-history:1@s.whatsapp.net", false, 2_000, false);
    // Answered through the owner ledger only.
    index_row(&store, "c-in", "socialapi", "dm", "00000000-0000-4000-8000-000000000002", false, 3_000, false);
    store
        .record_owner_message("socialapi", "00000000-0000-4000-8000-000000000002", "c-me", 3_100, "t")
        .unwrap();
    // Open, newest.
    index_row(&store, "d-in", "linkedin", "dm", "urn:li:msg_conversation:synthetic-0002", false, 4_000, false);
    // Older than the window.
    index_row(&store, "e-in", "gmail", "email", "1a0000000000000e", false, 10, false);
    // Not a conversation the owner owes an answer in.
    index_row(&store, "f-in", "discord", "channel", "100000000000000009", false, 5_000, false);
    index_row(&store, "g-in", "apple_notes", "note", "note-1", false, 5_000, false);

    let got = store.unanswered_conversations(100, 10).unwrap();
    let ids: Vec<_> = got.iter().map(|c| c.conversation_id.as_str()).collect();
    assert_eq!(
        ids,
        ["urn:li:msg_conversation:synthetic-0002", "whatsapp-history:1@s.whatsapp.net"]
    );
    assert_eq!(got[0].platform, "linkedin");
    assert_eq!(got[0].last_inbound_message_id, "d-in");
    assert_eq!(got[0].last_inbound_ms, 4_000);
    assert_eq!(got[1].conv_kind, "dm");

    assert_eq!(store.unanswered_conversations(100, 1).unwrap().len(), 1);
}
