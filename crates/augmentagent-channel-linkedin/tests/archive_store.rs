use augmentagent_channel_linkedin::{
    archive::{Conversation, Message, Page, Participant},
    archive_store::*,
};
use augmentagent_store::Store;
use serde_json::Value;
const OWNER: &str = "urn:li:fsd_profile:OWNER";
fn conversation(i: usize) -> Conversation {
    Conversation {
        urn: format!("thread:{i}"),
        title: format!("Thread {i}"),
        group: i >= 42,
        last_activity_ms: 100,
        participants: vec![
            Participant {
                urn: OWNER.into(),
                name: "Owner".into(),
            },
            Participant {
                urn: format!("person:{i}"),
                name: format!("Person {i}"),
            },
        ]
        .into_iter()
        .chain((i >= 42).then(|| Participant {
            urn: "introducer-1".into(),
            name: "Introducer".into(),
        }))
        .collect(),
    }
}
fn message(i: usize, j: usize) -> Message {
    Message {
        urn: format!("message:{i}:{j}"),
        conversation: format!("thread:{i}"),
        sender: if j % 3 == 0 {
            OWNER.into()
        } else {
            format!("person:{i}")
        },
        from_me: j % 3 == 0,
        delivered_at_ms: 1000 + j as i64,
        body: format!("Message {j}"),
        kind: "text".into(),
        content: Value::Null,
    }
}
#[test]
fn stores_exact_corpus_and_replay_is_idempotent() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    for (page_idx, range) in [(0, 0..20), (1, 20..40), (2, 40..45)] {
        let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
        let p = Page {
            items: range.map(conversation).collect(),
            next: (page_idx < 2).then(|| format!("page:{}", page_idx + 1)),
        };
        assert!(commit_conversations(&s, OWNER, "PRIMARY_INBOX", &cp, &p, 10000).unwrap());
        assert!(!commit_conversations(&s, OWNER, "PRIMARY_INBOX", &cp, &p, 10000).unwrap());
    }
    for i in 0..45 {
        let count = if i >= 42 { 9 } else { 3 };
        for (page_idx, chunk) in (0..count).collect::<Vec<_>>().chunks(4).enumerate() {
            let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
                .unwrap()
                .into_iter()
                .find(|(u, _)| *u == format!("thread:{i}"))
                .unwrap()
                .1;
            let p = Page {
                items: chunk.iter().map(|j| message(i, *j)).collect(),
                next: (chunk.last().unwrap() + 1 < count).then(|| format!("history:{page_idx}")),
            };
            assert!(commit_messages(&s, OWNER, &format!("thread:{i}"), &cp, &p, 10000).unwrap());
            assert!(!commit_messages(&s, OWNER, &format!("thread:{i}"), &cp, &p, 10000).unwrap());
        }
    }
    let c = coverage(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(
        (
            c.conversations,
            c.complete_threads,
            c.messages,
            c.owner_messages,
            c.group_conversations
        ),
        (45, 45, 153, 51, 3)
    );
    assert!(!c.complete, "index backlog must remain visible");
    assert_eq!(c.queued_index, 153);
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    assert!(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete);
    let own = s
        .with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM message_index WHERE platform='linkedin' AND from_me=1",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert_eq!(own, 51);
    let hits = s
        .with_conn(|c| {
            Ok(
                augmentagent_messages::query::search(c, "with:Introducer in:linkedin", Some(50), 0)
                    .unwrap(),
            )
        })
        .unwrap();
    assert_eq!(hits.total_estimate, 27);
    assert_eq!(
        hits.hits
            .iter()
            .map(|h| h.thread_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let owner_hits = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(
                c,
                "with:Introducer from:me in:linkedin",
                Some(50),
                0,
            )
            .unwrap())
        })
        .unwrap();
    assert_eq!(owner_hits.total_estimate, 9);
    let server = augmentagent_mcp_memory::Server::open(f.path()).unwrap();
    let read = server
        .read_conversation_thread("thread:42", 0, 0, 20)
        .unwrap();
    assert_eq!(read["messages"].as_array().unwrap().len(), 9);
    assert_eq!(read["messages"][0]["timestamp_ms"], 1000);
    drop(s);
    let s = Store::open(f.path()).unwrap();
    assert!(pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .is_empty());
    assert_eq!(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().messages, 153);
    let actions = s
        .with_conn(|c| c.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get::<_, i64>(0)))
        .unwrap();
    assert_eq!(actions, 0);
}
#[test]
fn failed_page_rolls_back_records_and_checkpoint() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    let p = Page {
        items: vec![conversation(0)],
        next: Some("page:1".into()),
    };
    commit_conversations(&s, OWNER, "PRIMARY_INBOX", &cp, &p, 10000).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    // A repeated durable continuation fails in the same transaction as its rows.
    let bad = Page {
        items: vec![conversation(1)],
        next: Some("page:1".into()),
    };
    assert!(commit_conversations(&s, OWNER, "PRIMARY_INBOX", &cp, &bad, 10001).is_err());
    assert_eq!(
        checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap().revision,
        cp.revision
    );
    assert_eq!(
        coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().conversations,
        1
    );
}
#[test]
fn independent_accounts_cannot_overwrite_identical_remote_ids() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    for owner in [OWNER, "urn:li:fsd_profile:OTHER"] {
        let cp = checkpoint(&s, owner, "PRIMARY_INBOX").unwrap();
        commit_conversations(
            &s,
            owner,
            "PRIMARY_INBOX",
            &cp,
            &Page {
                items: vec![conversation(0)],
                next: None,
            },
            10000,
        )
        .unwrap();
        let cp = pending_threads(&s, owner, "PRIMARY_INBOX")
            .unwrap()
            .remove(0)
            .1;
        commit_messages(
            &s,
            owner,
            "thread:0",
            &cp,
            &Page {
                items: vec![message(0, 0)],
                next: None,
            },
            10000,
        )
        .unwrap();
    }
    assert_eq!(
        s.with_conn(|c| c.query_row(
            "SELECT COUNT(DISTINCT stored_id) FROM linkedin_archive_messages",
            [],
            |r| r.get::<_, i64>(0)
        ))
        .unwrap(),
        2
    );
    let server = augmentagent_mcp_memory::Server::open(f.path()).unwrap();
    assert!(server
        .read_conversation_thread("thread:0", 0, 0, 20)
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    let account = format!("linkedin:{OWNER}");
    let page = server
        .read_conversation_thread_scoped("thread:0", 0, 0, 20, Some(&account), None)
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["account_id"], account);
    assert!(server
        .read_conversation_thread_scoped("thread:0", 0, 0, 20, Some("linkedin:foreign"), None)
        .is_err());
}

#[test]
fn thread_continuation_reconstructs_unicode_and_excludes_new_arrivals() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(42)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    let mut original = (0..9).map(|j| message(42, j)).collect::<Vec<_>>();
    original[0].body = "🌍你好é\n".repeat(2001);
    original[1].delivered_at_ms = original[0].delivered_at_ms;
    commit_messages(
        &s,
        OWNER,
        "thread:42",
        &cp,
        &Page {
            items: original.iter().rev().cloned().collect(),
            next: None,
        },
        10000,
    )
    .unwrap();
    let server = augmentagent_mcp_memory::Server::open(f.path()).unwrap();
    let hits = server
        .search_conversation_history(None, Some(1000), Some(1002), Some("linkedin"), Some(20))
        .unwrap();
    assert_eq!(hits.len(), 3, "date windows must use the source event time");
    assert_eq!(hits.iter().filter(|h| h.role == "owner").count(), 1);
    let mut offset = 0;
    let mut body_offset = 0;
    let mut snapshot = None;
    let mut ordered_ids = Vec::<String>::new();
    let mut reconstructed = std::collections::BTreeMap::<String, String>::new();
    let account = format!("linkedin:{OWNER}");
    for n in 0..30 {
        let req=serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":n,"method":"tools/call","params":{"name":"read_conversation_thread","arguments":{
   "thread_id":"thread:42","offset":offset,"body_offset":body_offset,"limit":2,"account_id":account,
   "snapshot_max_rowid":snapshot
  }}})).unwrap();
        // Omit an absent optional boundary on the first request.
        let mut req: augmentagent_mcp_memory::McpRequest = req;
        if snapshot.is_none() {
            req.params.as_mut().unwrap()["arguments"]
                .as_object_mut()
                .unwrap()
                .remove("snapshot_max_rowid");
        }
        let response = server.dispatch(&req);
        assert_ne!(response["result"]["isError"], true, "{response}");
        let page: serde_json::Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        snapshot = page["snapshot_max_rowid"].as_i64();
        for m in page["messages"].as_array().unwrap() {
            assert_eq!(m["imported_at_ms"], 10000);
            let id = m["message_id"].as_str().unwrap().to_string();
            if ordered_ids.last() != Some(&id) {
                ordered_ids.push(id);
            }
            reconstructed
                .entry(m["message_id"].as_str().unwrap().into())
                .or_default()
                .push_str(m["body"].as_str().unwrap());
        }
        if n == 0 {
            // Arrives later but sorts before the original source events.
            s.with_conn(|c|{c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,accountEntityId,firstSeenAt,platform,kind) VALUES('late','thread:42','late','','late','',?1,0,'linkedin','group')",[&account])?;Ok(())}).unwrap();
        }
        if page["next_offset"].is_null() {
            break;
        }
        offset = page["next_offset"].as_u64().unwrap();
        body_offset = page["next_body_offset"].as_u64().unwrap();
    }
    assert_eq!(reconstructed.len(), 9);
    assert_eq!(
        ordered_ids,
        original.iter().map(|m| m.urn.clone()).collect::<Vec<_>>(),
        "stored thread/body continuations must preserve source order after reversed ingestion"
    );
    for m in original {
        assert_eq!(reconstructed[&m.urn], m.body);
    }
}

#[test]
fn coverage_requires_derived_index_and_full_text_rows() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![message(0, 0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    assert!(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete);
    s.with_conn(|c| {
        c.execute("DELETE FROM message_fts", [])?;
        Ok(())
    })
    .unwrap();
    assert!(
        !coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete,
        "a missing FTS body cannot count as complete coverage"
    );
    s.with_conn(|c| {
        c.execute("DELETE FROM message_index", [])?;
        Ok(())
    })
    .unwrap();
    assert!(
        !coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete,
        "missing derived rows cannot count as complete coverage"
    );
}

#[test]
fn membership_cannot_leak_across_accounts_with_same_thread_id() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    for (owner, peer) in [
        (OWNER, "person:0"),
        ("urn:li:fsd_profile:OTHER", "person:other"),
    ] {
        let cp = checkpoint(&s, owner, "PRIMARY_INBOX").unwrap();
        let mut conv = conversation(0);
        conv.participants[0].urn = owner.into();
        conv.participants[1].urn = peer.into();
        commit_conversations(
            &s,
            owner,
            "PRIMARY_INBOX",
            &cp,
            &Page {
                items: vec![conv],
                next: None,
            },
            10000,
        )
        .unwrap();
        let cp = pending_threads(&s, owner, "PRIMARY_INBOX")
            .unwrap()
            .remove(0)
            .1;
        let mut m = message(0, 1);
        m.sender = peer.into();
        commit_messages(
            &s,
            owner,
            "thread:0",
            &cp,
            &Page {
                items: vec![m],
                next: None,
            },
            10000,
        )
        .unwrap();
    }
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    let hits = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(
                c,
                "with:linkedin:person:0 in:linkedin",
                Some(50),
                0,
            )
            .unwrap())
        })
        .unwrap();
    assert_eq!(
        hits.total_estimate, 1,
        "same native thread ID in another account is a different conversation"
    );
}

#[test]
fn unowned_legacy_id_is_not_overwritten_by_an_archive_account() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    s.with_conn(|c| { c.execute("INSERT INTO emails(messageId,fromEmail,subject,body,receivedAt,firstSeenAt,platform,kind) VALUES('message:0:0','unknown','','preserve me','',1,'linkedin','dm')",[])?; Ok(()) }).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![message(0, 0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let body = s
        .with_conn(|c| {
            c.query_row(
                "SELECT body FROM emails WHERE messageId='message:0:0'",
                [],
                |r| r.get::<_, String>(0),
            )
        })
        .unwrap();
    assert_eq!(
        body, "preserve me",
        "unknown account ownership is not permission to overwrite"
    );
    assert_eq!(
        s.with_conn(|c| c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get::<_, i64>(0)))
            .unwrap(),
        2
    );
}

#[test]
fn metadata_correction_enqueues_the_existing_message() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![message(0, 0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    assert!(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete);
    refresh_mailbox(&s, OWNER, "PRIMARY_INBOX").unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    let mut conv = conversation(0);
    conv.last_activity_ms += 1;
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conv],
            next: None,
        },
        10001,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    let mut corrected = message(0, 0);
    corrected.from_me = false;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![corrected],
            next: None,
        },
        10001,
    )
    .unwrap();
    assert_eq!(
        coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().queued_index,
        1
    );
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    assert!(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete);
    assert_eq!(
        coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().owner_messages,
        0
    );
}

#[test]
fn overlapping_page_edit_updates_an_already_queued_message_atomically() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![message(0, 0)],
            next: Some("more".into()),
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    let mut edited = message(0, 0);
    edited.body = "editedmarker".into();
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![edited],
            next: None,
        },
        10001,
    )
    .unwrap();
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    let c = coverage(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert!(c.complete);
    assert_eq!(c.messages, 1);
    let hits = s
        .with_conn(|c| {
            Ok(
                augmentagent_messages::query::search(c, "editedmarker in:linkedin", Some(50), 0)
                    .unwrap(),
            )
        })
        .unwrap();
    assert_eq!(hits.total_estimate, 1);
}

#[test]
fn equal_participant_names_are_ambiguous_but_stable_identity_is_exact() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let mut second = conversation(43);
    second.participants[2].urn = "introducer-2".into();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(42), second],
            next: None,
        },
        10000,
    )
    .unwrap();
    for i in [42, 43] {
        let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == &format!("thread:{i}"))
            .unwrap()
            .1;
        commit_messages(
            &s,
            OWNER,
            &format!("thread:{i}"),
            &cp,
            &Page {
                items: vec![message(i, 1)],
                next: None,
            },
            10000,
        )
        .unwrap();
    }
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    let ambiguous = s
        .with_conn(|c| {
            Ok(
                augmentagent_messages::query::search(c, "with:Introducer in:linkedin", Some(50), 0)
                    .unwrap(),
            )
        })
        .unwrap();
    assert_eq!(ambiguous.total_estimate, 0);
    assert_eq!(ambiguous.ambiguous.len(), 1);
    let exact = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(
                c,
                "with:linkedin:introducer-1 in:linkedin",
                Some(50),
                0,
            )
            .unwrap())
        })
        .unwrap();
    assert_eq!(exact.total_estimate, 1);
    assert_eq!(exact.hits[0].thread_id, "thread:42");
    let authored = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(
                c,
                "from:linkedin:introducer-1 in:linkedin",
                Some(50),
                0,
            )
            .unwrap())
        })
        .unwrap();
    assert_eq!(
        authored.total_estimate, 0,
        "membership must not imply authorship"
    );
}

#[test]
fn search_results_expose_account_and_partial_archive_coverage() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let empty = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(c, "in:linkedin", Some(50), 0).unwrap())
        })
        .unwrap();
    let empty = serde_json::to_value(empty).unwrap();
    assert_eq!(empty["linkedin_coverage"]["scope"], "known_categories_only");
    assert_eq!(empty["linkedin_coverage"]["state"], "not_synced");
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0)],
            next: Some("more".into()),
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![message(0, 1)],
            next: None,
        },
        10001,
    )
    .unwrap();
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    let result = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(c, "in:linkedin", Some(50), 0).unwrap())
        })
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["hits"][0]["account_id"], format!("linkedin:{OWNER}"));
    assert_eq!(result["linkedin_coverage"]["scopes"][0]["complete"], false);
    assert_eq!(
        result["linkedin_coverage"]["scopes"][0]["last_success_ms"],
        10000
    );
    let server = augmentagent_mcp_memory::Server::open(f.path()).unwrap();
    let result = server
        .search_conversation_history(Some("Message"), None, None, Some("linkedin"), Some(10))
        .unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap()[0]["account_id"],
        format!("linkedin:{OWNER}")
    );
    let request=serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_conversation_history","arguments":{"keyword":"Message","channel":"linkedin"}}})).unwrap();
    let response = server.dispatch(&request);
    let page: serde_json::Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["linkedin_coverage"]["scopes"][0]["complete"], false);
}

#[test]
fn legacy_linkedin_read_and_date_filter_use_received_time_when_available() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    s.with_conn(|c|{c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,accountEntityId,firstSeenAt,platform,kind) VALUES('legacy','legacy-thread','Peer','','legacySource','2020-01-02T03:04:05.123Z','linkedin:legacy-owner',1900000000000,'linkedin','dm')",[])?;Ok(())}).unwrap();
    let server = augmentagent_mcp_memory::Server::open(f.path()).unwrap();
    let hits = server
        .search_conversation_history(
            None,
            Some(1577934245123),
            Some(1577934245124),
            Some("linkedin"),
            Some(10),
        )
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "legacy receipt time must not move old messages into the import window"
    );
    let page = server
        .read_conversation_thread("legacy-thread", 0, 0, 20)
        .unwrap();
    assert_eq!(page["messages"][0]["timestamp_ms"], 1577934245123i64);
    assert_eq!(page["messages"][0]["imported_at_ms"], 1900000000000i64);
    assert_eq!(page["coverage"]["state"], "unverified");
}

#[test]
fn reused_message_identity_cannot_move_a_message_or_commit_any_of_the_page() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0), conversation(1)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let pending = pending_threads(&s, OWNER, "PRIMARY_INBOX").unwrap();
    let cp0 = &pending.iter().find(|(id, _)| id == "thread:0").unwrap().1;
    commit_messages(
        &s,
        OWNER,
        "thread:0",
        cp0,
        &Page {
            items: vec![message(0, 1)],
            next: None,
        },
        10001,
    )
    .unwrap();
    let cp1 = &pending.iter().find(|(id, _)| id == "thread:1").unwrap().1;
    let mut reused = message(1, 2);
    reused.urn = "message:0:1".into();
    assert!(
        commit_messages(
            &s,
            OWNER,
            "thread:1",
            cp1,
            &Page {
                items: vec![message(1, 1), reused],
                next: Some("next".into()),
            },
            10002
        )
        .is_err(),
        "a stable identity cannot switch conversations"
    );
    let remaining = pending_threads(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].1.revision, cp1.revision);
    assert!(remaining[0].1.cursor.is_none());
    s.with_conn(|c| {
        let row: (String, String) = c.query_row(
            "SELECT threadId,body FROM emails WHERE messageId='message:0:1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(row, ("thread:0".into(), "Message 1".into()));
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM linkedin_archive_cursors WHERE scope='thread:thread:1'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn scoped_fallback_identity_cannot_overwrite_unowned_legacy_data() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let fallback = format!("linkedin-archive:{OWNER}:message:0:0");
    s.with_conn(|c| {
        for id in ["message:0:0", fallback.as_str()] {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,firstSeenAt,platform,kind) VALUES(?1,'legacy-thread','unknown','','preserve me','',1,'linkedin','dm')", [id])?;
        }
        Ok(())
    }).unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    commit_conversations(
        &s,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conversation(0)],
            next: None,
        },
        10000,
    )
    .unwrap();
    let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    assert!(commit_messages(
        &s,
        OWNER,
        "thread:0",
        &cp,
        &Page {
            items: vec![message(0, 0)],
            next: None,
        },
        10001
    )
    .is_err());
    s.with_conn(|c| {
        assert_eq!(c.query_row("SELECT COUNT(*) FROM emails WHERE body='preserve me' AND accountEntityId IS NULL AND threadId='legacy-thread'", [], |r| r.get::<_, i64>(0))?, 2);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM linkedin_archive_messages", [], |r| r.get::<_, i64>(0))?, 0);
        Ok(())
    }).unwrap();
    assert_eq!(
        pending_threads(&s, OWNER, "PRIMARY_INBOX").unwrap()[0]
            .1
            .revision,
        cp.revision
    );
}

#[test]
fn account_failure_clear_cannot_erase_a_concurrent_throttle() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    record_account_failure(&s, OWNER, "initial", 1000).unwrap();
    record_account_failure(&s, OWNER, "newer throttle", 5000).unwrap();
    // A delayed request/error must neither shorten the pause nor erase it.
    record_account_failure(&s, OWNER, "older failure", 2000).unwrap();
    clear_account_failure(&s, OWNER, 1001).unwrap();
    assert_eq!(account_retry_at(&s, OWNER).unwrap(), 5000);
    let status = s
        .with_conn(|c| augmentagent_store::linkedin_archive::account_status(c, OWNER))
        .unwrap();
    assert_eq!(status.last_error.as_deref(), Some("newer throttle"));
    // A successful request begun after the pause can clear the stale error.
    clear_account_failure(&s, OWNER, 5001).unwrap();
    assert_eq!(account_retry_at(&s, OWNER).unwrap(), 0);
    let status = s
        .with_conn(|c| augmentagent_store::linkedin_archive::account_status(c, OWNER))
        .unwrap();
    assert!(status.last_error.is_none());
}

#[test]
fn coverage_counts_only_committed_pages_and_preserves_totals_across_refresh() {
    let s = Store::open(":memory:").unwrap();
    let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    let page = Page {
        items: vec![conversation(0)],
        next: None,
    };
    assert!(commit_conversations(&s, OWNER, "PRIMARY_INBOX", &cp, &page, 10000).unwrap());
    assert!(!commit_conversations(&s, OWNER, "PRIMARY_INBOX", &cp, &page, 10001).unwrap());
    let json = serde_json::to_value(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap()).unwrap();
    assert_eq!(json["mailbox_pages"]["committed"], 1);
    assert_eq!(json["mailbox_pages"]["current_cycle"], 1);
    refresh_mailbox(&s, OWNER, "PRIMARY_INBOX").unwrap();
    let json = serde_json::to_value(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap()).unwrap();
    assert_eq!(json["mailbox_pages"]["committed"], 1);
    assert_eq!(json["mailbox_pages"]["current_cycle"], 0);
    assert_eq!(json["scope"], "all_accessible_history");
}

#[test]
fn legacy_default_platform_keeps_its_row_and_action_while_archive_uses_scoped_id() {
    let s = Store::open(":memory:").unwrap();
    let account = format!("linkedin:{OWNER}");
    s.with_conn(|c| {
        c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,accountEntityId,firstSeenAt,platform,kind) VALUES('message:0:0','thread:0','legacy sender','','preserve legacy body','',?1,1,'gmail','dm')", [&account])?;
        Ok(())
    }).unwrap();
    let action = s
        .log_action(
            "message:0:0",
            Some("thread:0"),
            "legacy sender",
            "",
            Some("preserve legacy body"),
            Some("preserve draft"),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap();
    for cycle in 0..2 {
        if cycle > 0 {
            refresh_mailbox(&s, OWNER, "PRIMARY_INBOX").unwrap();
        }
        let cp = checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
        let mut conversation = conversation(0);
        conversation.last_activity_ms += cycle;
        commit_conversations(
            &s,
            OWNER,
            "PRIMARY_INBOX",
            &cp,
            &Page {
                items: vec![conversation],
                next: None,
            },
            10000 + cycle,
        )
        .unwrap();
        let cp = pending_threads(&s, OWNER, "PRIMARY_INBOX")
            .unwrap()
            .remove(0)
            .1;
        assert!(commit_messages(
            &s,
            OWNER,
            "thread:0",
            &cp,
            &Page {
                items: vec![message(0, 0)],
                next: None
            },
            10000 + cycle
        )
        .unwrap());
        augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
        assert!(coverage(&s, OWNER, "PRIMARY_INBOX").unwrap().complete);
        s.with_conn(|c| {
            let original: (String, String, String) = c.query_row("SELECT platform,body,threadId FROM emails WHERE messageId='message:0:0'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            assert_eq!(original, ("gmail".into(), "preserve legacy body".into(), "thread:0".into()));
            let archived: String = c.query_row("SELECT stored_id FROM linkedin_archive_messages WHERE account=?1 AND urn='message:0:0'", [OWNER], |r| r.get(0))?;
            assert_eq!(archived, format!("linkedin-archive:{OWNER}:message:0:0"));
            assert_eq!(c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get::<_,i64>(0))?, 2);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM linkedin_archive_messages", [], |r| r.get::<_,i64>(0))?, 1);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM message_index WHERE platform='linkedin'", [], |r| r.get::<_,i64>(0))?, 1);
            Ok(())
        }).unwrap();
        let preserved = s.get_action_with_email(&action).unwrap().unwrap();
        assert_eq!(preserved.action.status, "pending");
        assert_eq!(
            preserved.action.draft_body.as_deref(),
            Some("preserve draft")
        );
        assert_eq!(preserved.email.body, "preserve legacy body");
    }
}
