use augmentagent_channel_core::draft_context::load_seed;
use augmentagent_store::{Email, Store};

fn message(i: usize) -> Email {
    Email {
        message_id: format!("m{i:02}"),
        thread_id: Some("thread".into()),
        from: "peer@example.invalid".into(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        subject: "subject".into(),
        body: format!("body {i}"),
        date: format!("2026-01-01T00:{i:02}:00Z"),
        account_entity_id: Some("account".into()),
        platform: "imessage".into(),
        kind: "dm".into(),
    }
}

#[test]
fn seed_uses_stored_turns_and_explicit_wiki_identity_not_only_new_batch() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    std::fs::create_dir_all(wiki.join("about")).unwrap();
    std::fs::write(wiki.join("about/me.md"), "# Owner\nKeep texts short.").unwrap();
    std::fs::write(wiki.join("people/peer.md"),"---\nidentities:\n  imessage: [peer@example.invalid]\n---\n# Peer\nWe promised the document on Friday.").unwrap();
    for i in 0..9 {
        store.upsert_email_backfill(&message(i), 1).unwrap();
    }
    let seed = load_seed(&store, &message(9), Some(&wiki)).unwrap();
    assert_eq!(seed.messages.len(), 10);
    assert_eq!(seed.messages[0].message_id, "m00");
    assert_eq!(seed.messages[9].message_id, "m09");
    assert_eq!(seed.person_slug.as_deref(), Some("peer"));
    assert_eq!(seed.wiki.len(), 2);
    assert!(seed.wiki.iter().any(|p| p.text.contains("Friday")));
    assert!(seed.gaps.is_empty());
    store.upsert_email_backfill(&message(9), 1).unwrap();
    let again = load_seed(&store, &message(9), Some(&wiki)).unwrap();
    assert_eq!(
        again
            .messages
            .iter()
            .filter(|m| m.message_id == "m09")
            .count(),
        1
    );
}

#[test]
fn seed_reports_ambiguity_and_truncation_without_mixing_people() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    for name in ["first", "second"] {
        std::fs::write(
            wiki.join(format!("people/{name}.md")),
            "---\nidentities:\n  imessage: [peer@example.invalid]\n---\nPrivate unrelated details",
        )
        .unwrap();
    }
    let mut current = message(1);
    current.body = "é".repeat(40_000);
    current.attachments.push("diagram.png".into());
    let seed = load_seed(&store, &current, Some(&wiki)).unwrap();
    assert_eq!(seed.messages[0].body.chars().count(), 32_000);
    assert!(seed.person_slug.is_none());
    assert!(seed.wiki.is_empty());
    assert!(seed.gaps.iter().any(|g| g.contains("Ambiguous")));
    assert!(seed.gaps.iter().any(|g| g.contains("truncated")));
    assert!(seed.gaps.iter().any(|g| g.contains("Attachment")));
    assert!(!seed.prompt().contains("Private unrelated details"));
}

struct Scripted(std::sync::Mutex<std::collections::VecDeque<String>>);
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for Scripted {
    async fn call(
        &self,
        opts: &augmentagent_channel_core::ReasonerOpts,
        _prompt: &str,
    ) -> anyhow::Result<String> {
        assert!(
            opts.allowed_tools.is_empty(),
            "retrieval execution belongs to the coordinator"
        );
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call"))
    }
}
fn script(values: Vec<serde_json::Value>) -> Scripted {
    Scripted(std::sync::Mutex::new(
        values.into_iter().map(|v| v.to_string()).collect(),
    ))
}

#[tokio::test]
async fn retrieval_reads_cross_platform_source_before_citing_it() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    let mut email = message(0);
    email.message_id = "email-plan".into();
    email.platform = "gmail".into();
    email.body = "Project plan: moved the meeting to Friday.".into();
    store.upsert_email_backfill(&email, 1).unwrap();
    augmentagent_messages::index::drain_batch(&store, 100).unwrap();
    let seed = load_seed(&store, &message(1), None).unwrap();
    let agent = script(vec![
        json!({"action":"search","query":"in:gmail meeting","reason":"find the email plan"}),
        json!({"action":"read_message","message_id":"email-plan","reason":"verify new meeting day"}),
        json!({"action":"ready","facts":[{"text":"Meeting moved to Friday","source_ids":["email-plan"]}],"gaps":[]}),
    ]);
    let evidence = augmentagent_channel_core::draft_evidence::gather(
        &store,
        &agent,
        &seed,
        "Does the email plan work?",
        None,
    )
    .await;
    assert!(evidence.complete, "{:?}", evidence.gaps);
    assert_eq!(evidence.reads.len(), 2);
    assert!(evidence.reads[1].result["text"]
        .as_str()
        .unwrap()
        .contains("Friday"));
    assert_eq!(evidence.facts[0].source_ids, vec!["email-plan"]);
}

#[tokio::test]
async fn retrieval_does_not_accept_a_snippet_as_verified_source() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    let mut email = message(0);
    email.message_id = "email-plan".into();
    email.platform = "gmail".into();
    email.body = "Friday meeting".into();
    store.upsert_email_backfill(&email, 1).unwrap();
    augmentagent_messages::index::drain_batch(&store, 100).unwrap();
    let seed = load_seed(&store, &message(1), None).unwrap();
    let agent = script(vec![
        json!({"action":"search","query":"in:gmail meeting","reason":"find email"}),
        json!({"action":"ready","facts":[{"text":"Friday","source_ids":["email-plan"]}],"gaps":[]}),
    ]);
    let evidence =
        augmentagent_channel_core::draft_evidence::gather(&store, &agent, &seed, "reply", None)
            .await;
    assert!(!evidence.complete);
    assert!(evidence.gaps[0].contains("not read"));
}

#[tokio::test]
async fn retrieval_rejects_wiki_path_escape_and_keeps_missing_evidence_visible() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    let seed = load_seed(&store, &message(1), None).unwrap();
    let agent = script(vec![
        json!({"action":"read_wiki","path":"../secret","reason":"untrusted requested path"}),
        json!({"action":"ready","facts":[],"gaps":["Requested source was unavailable"]}),
    ]);
    let evidence = augmentagent_channel_core::draft_evidence::gather(
        &store,
        &agent,
        &seed,
        "reply",
        Some(dir.path()),
    )
    .await;
    assert!(evidence.reads[0].result.get("error").is_some());
    assert!(!evidence.gaps.is_empty());
}

#[tokio::test]
async fn workflow_receipt_omits_source_bodies_and_guards_material_gaps() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db")).unwrap();
    let mut incoming = message(1);
    incoming.body = "SYNTHETIC_PRIVATE_MESSAGE_BODY".into();
    let agent = script(vec![
        json!({"action":"ready","facts":[],"gaps":["The referenced email archive is unavailable"]}),
    ]);
    let context = augmentagent_channel_core::draft_workflow::prepare(
        &store,
        &agent,
        &incoming,
        "draft a reply",
        None,
    )
    .await
    .unwrap();
    let body = context.guard_draft("Can you resend it?");
    let (plain, needs) = augmentagent_approval_discord::split_needs_input(&body);
    assert_eq!(plain, "Can you resend it?");
    assert!(!needs.is_empty());
    let action = store
        .log_action(
            &incoming.message_id,
            incoming.thread_id.as_deref(),
            &incoming.from,
            &incoming.subject,
            Some(&incoming.body),
            Some(&body),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap();
    assert!(store
        .attach_draft_context_receipt(&context.receipt_id, &action)
        .unwrap());
    let receipt = store.draft_context_receipts(&action).unwrap().join("");
    assert!(!receipt.contains("SYNTHETIC_PRIVATE_MESSAGE_BODY"));
    assert!(receipt.contains("email archive is unavailable"));
}

#[test]
fn context_command_does_not_leak_private_evidence_through_tool_notices() {
    use augmentagent_channel_core::tool_audit::{build_audit_record, format_notice};
    use augmentagent_channel_core::ProviderKind;
    for command in [
        "augmentagent messages context --message-id PRIVATE_ID --request PRIVATE_REQUEST",
        "augmentagent 'messages' 'context' --request PRIVATE_REQUEST --message-id PRIVATE_ID",
        "augmentagent messages context-receipts --action-id PRIVATE_ID",
    ] {
        for failed in [false, true] {
            let record = build_audit_record(
                ProviderKind::Claude,
                "now".into(),
                "session".into(),
                "Bash".into(),
                serde_json::json!({"command":command,"description":"PRIVATE_REQUEST"}),
                "PRIVATE_EVIDENCE_BODY",
                failed,
            );
            let rendered = format!(
                "{} {}",
                serde_json::to_string(&record).unwrap(),
                format_notice(&record)
            );
            for secret in ["PRIVATE_ID", "PRIVATE_REQUEST", "PRIVATE_EVIDENCE_BODY"] {
                assert!(!rendered.contains(secret));
            }
        }
    }
}

#[test]
fn owner_requested_context_refreshes_turns_after_the_original_inbound() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db")).unwrap();
    for i in 0..13 {
        let mut row = message(i);
        if i == 12 {
            row.from = "me".into();
            row.body = "Already answered by hand".into();
        }
        store.upsert_email_backfill(&row, 1).unwrap();
    }
    let original = message(0);
    let trigger_seed = load_seed(&store, &original, None).unwrap();
    assert_eq!(trigger_seed.messages.len(), 1);
    let refreshed =
        augmentagent_channel_core::draft_context::load_latest_seed(&store, &original, None)
            .unwrap();
    assert_eq!(refreshed.messages.len(), 10);
    assert_eq!(refreshed.messages[0].message_id, "m03");
    assert_eq!(
        refreshed.messages.last().unwrap().body,
        "Already answered by hand"
    );
    assert_eq!(refreshed.audience["trigger_message_id"], "m00");
}
