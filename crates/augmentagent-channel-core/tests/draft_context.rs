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

struct Delayed {
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    delay: std::time::Duration,
}
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for Delayed {
    async fn call(
        &self,
        _opts: &augmentagent_channel_core::ReasonerOpts,
        _prompt: &str,
    ) -> anyhow::Result<String> {
        tokio::time::sleep(self.delay).await;
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call"))
    }
}
fn delayed(values: Vec<serde_json::Value>, delay: std::time::Duration) -> Delayed {
    Delayed {
        replies: std::sync::Mutex::new(values.into_iter().map(|v| v.to_string()).collect()),
        delay,
    }
}

fn pending_action(store: &Store, incoming: &Email) -> String {
    store
        .log_action(
            &incoming.message_id,
            incoming.thread_id.as_deref(),
            &incoming.from,
            &incoming.subject,
            Some(&incoming.body),
            Some("draft"),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap()
}

fn persisted_receipt(
    store: &Store,
    context: &augmentagent_channel_core::draft_workflow::PreparedDraftContext,
    incoming: &Email,
) -> String {
    let action = pending_action(store, incoming);
    assert!(context.attach(store, &action).unwrap());
    store
        .draft_context_receipts(&action)
        .unwrap()
        .pop()
        .expect("receipt attached to the action")
}

#[tokio::test]
async fn receipt_times_every_step_and_names_the_budget_consumer_on_each_platform() {
    use augmentagent_channel_core::draft_evidence::{BudgetConsumer, StepKind};
    use serde_json::{json, Value};
    let mut schemas = Vec::new();
    for platform in ["imessage", "gmail"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(wiki.join("projects")).unwrap();
        std::fs::write(
            wiki.join("projects/cobalt.md"),
            "SYNTHETIC_WIKI_BODY: launch moved to Friday.",
        )
        .unwrap();
        let mut incoming = message(1);
        incoming.platform = platform.into();
        incoming.body = "SYNTHETIC_PRIVATE_MESSAGE_BODY".into();
        let agent = delayed(
            vec![
                json!({"action":"search_wiki","query":"cobalt","reason":"locate launch plan"}),
                json!({"action":"read_wiki","path":"projects/cobalt.md","reason":"verify date"}),
                json!({"action":"ready","facts":[{"text":"Launch is Friday","source_ids":["projects/cobalt.md"]}],"gaps":[]}),
            ],
            std::time::Duration::from_millis(20),
        );
        let context = augmentagent_channel_core::draft_workflow::prepare(
            &store,
            &agent,
            &incoming,
            "draft a reply",
            Some(&wiki),
        )
        .await
        .unwrap();
        assert!(context.evidence.complete, "{:?}", context.evidence.gaps);
        let timings = &context.evidence.step_timings;
        assert_eq!(
            timings
                .iter()
                .map(|s| (s.kind, s.operation.as_str()))
                .collect::<Vec<_>>(),
            [
                (StepKind::ModelDecision, "decision"),
                (StepKind::Retrieval, "search_wiki"),
                (StepKind::ModelDecision, "decision"),
                (StepKind::Retrieval, "read_wiki"),
                (StepKind::ModelDecision, "decision"),
            ]
        );
        assert!(timings.iter().all(|s| s.elapsed_ms > 0), "{timings:?}");
        assert_eq!(
            context.evidence.budget_consumer(),
            BudgetConsumer::ModelDecision
        );
        let raw = persisted_receipt(&store, &context, &incoming);
        assert!(!raw.contains("SYNTHETIC_PRIVATE_MESSAGE_BODY"));
        assert!(!raw.contains("SYNTHETIC_WIKI_BODY"));
        let receipt: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(receipt["budget_consumer"], "model_decision");
        let steps = receipt["step_timings"].as_array().unwrap().clone();
        assert_eq!(steps.len(), 5);
        assert!(steps.iter().all(|step| {
            step["elapsed_ms"].as_u64().is_some_and(|ms| ms > 0)
                && step["kind"].is_string()
                && step["outcome"] == "ok"
        }));
        schemas.push(
            steps
                .iter()
                .map(|step| {
                    step.as_object()
                        .unwrap()
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(
        schemas[0], schemas[1],
        "iMessage and email receipts share one telemetry schema"
    );
}

#[tokio::test(start_paused = true)]
async fn budget_timeout_blames_the_model_decision_and_keeps_the_card_warning() {
    use augmentagent_channel_core::draft_evidence::{BudgetConsumer, StepKind};
    use serde_json::{json, Value};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let incoming = message(1);
    let agent = delayed(
        vec![json!({"action":"ready","facts":[],"gaps":[]})],
        std::time::Duration::from_secs(91),
    );
    let context = augmentagent_channel_core::draft_workflow::prepare(
        &store,
        &agent,
        &incoming,
        "draft a reply",
        None,
    )
    .await
    .unwrap();
    assert!(!context.evidence.complete);
    assert_eq!(context.evidence.step_timings.len(), 1);
    assert_eq!(
        context.evidence.step_timings[0].kind,
        StepKind::ModelDecision
    );
    assert_eq!(context.evidence.step_timings[0].outcome, "timeout");
    assert_eq!(
        context.evidence.budget_consumer(),
        BudgetConsumer::ModelDecision
    );
    let (_, needs) =
        augmentagent_approval_discord::split_needs_input(&context.guard_draft("Let me check."));
    assert!(needs.iter().any(|gap| gap.text.contains("time limit")));
    let receipt: Value =
        serde_json::from_str(&persisted_receipt(&store, &context, &incoming)).unwrap();
    assert_eq!(receipt["budget_consumer"], "model_decision");
    assert_eq!(receipt["step_timings"][0]["kind"], "model_decision");
    assert!(receipt["gaps"][0].as_str().unwrap().contains("time limit"));
}

#[tokio::test]
async fn a_receipt_without_timed_work_reports_no_budget_consumer() {
    use augmentagent_channel_core::draft_evidence::BudgetConsumer;
    use serde_json::Value;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let incoming = message(1);
    let context = augmentagent_channel_core::draft_workflow::prepare(
        &store,
        &script(vec![]),
        &incoming,
        &"x".repeat(64_001),
        None,
    )
    .await
    .unwrap();
    assert!(context.evidence.step_timings.is_empty());
    assert_eq!(context.evidence.budget_consumer(), BudgetConsumer::None);
    let receipt: Value =
        serde_json::from_str(&persisted_receipt(&store, &context, &incoming)).unwrap();
    assert_eq!(receipt["budget_consumer"], "none");
    assert_eq!(receipt["step_timings"], serde_json::json!([]));
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
    let repair = store
        .log_action(
            &incoming.message_id,
            incoming.thread_id.as_deref(),
            &incoming.from,
            &incoming.subject,
            Some(&incoming.body),
            Some("Repaired draft"),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap();
    assert!(context.attach(&store, &repair).unwrap());
    assert_eq!(store.draft_context_receipts(&action).unwrap().len(), 1);
    assert_eq!(store.draft_context_receipts(&repair).unwrap().len(), 1);
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

#[tokio::test]
async fn retrieval_discovers_and_reads_project_wiki_before_citing_it() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("projects")).unwrap();
    std::fs::write(
        wiki.join("projects/cobalt.md"),
        "Cobalt launch was moved to Friday.",
    )
    .unwrap();
    let seed = load_seed(&store, &message(1), Some(&wiki)).unwrap();
    let agent = script(vec![
        json!({"action":"search_wiki","query":"cobalt","reason":"locate launch plan"}),
        json!({"action":"read_wiki","path":"projects/cobalt.md","reason":"verify date"}),
        json!({"action":"ready","facts":[{"text":"Launch is Friday","source_ids":["projects/cobalt.md"]}],"gaps":[]}),
    ]);
    let result = augmentagent_channel_core::draft_evidence::gather(
        &store,
        &agent,
        &seed,
        "draft launch reply",
        Some(&wiki),
    )
    .await;
    assert!(result.complete);
    assert_eq!(result.reads[0].result["paths"][0], "projects/cobalt.md");
    assert!(result.reads[1].result["text"]
        .as_str()
        .unwrap()
        .contains("Friday"));
}

#[tokio::test]
async fn revision_refreshes_history_and_preserves_all_owner_feedback() {
    struct RevisionAgent;
    #[async_trait::async_trait]
    impl augmentagent_channel_core::Reasoner for RevisionAgent {
        async fn call(
            &self,
            opts: &augmentagent_channel_core::ReasonerOpts,
            prompt: &str,
        ) -> anyhow::Result<String> {
            assert!(prompt.contains("Keep it brief"));
            assert!(prompt.contains("Use the updated plan"));
            assert!(prompt.contains("newer confirmed plan"));
            if opts.system_prompt.contains("You select evidence") {
                Ok(r#"{"action":"ready","facts":[],"gaps":[]}"#.into())
            } else {
                Ok("Friday works.".into())
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let original = message(1);
    store.upsert_email_backfill(&original, 1).unwrap();
    let mut newer = message(2);
    newer.body = "newer confirmed plan: Friday".into();
    store.upsert_email_backfill(&newer, 2).unwrap();
    let action = store
        .log_action(
            &original.message_id,
            original.thread_id.as_deref(),
            &original.from,
            &original.subject,
            Some(&original.body),
            Some("Thursday works"),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap();
    store
        .record_revision_triple(&action, "Thursday works", "Keep it brief", "Thursday")
        .unwrap();
    let draft = augmentagent_channel_core::draft_workflow::redraft(
        &store,
        &RevisionAgent,
        &original,
        &action,
        "Thursday",
        "Use the updated plan",
        None,
        "Write a reply",
    )
    .await
    .unwrap();
    assert_eq!(draft, "Friday works.");
    assert_eq!(store.draft_context_receipts(&action).unwrap().len(), 1);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}

#[tokio::test]
async fn older_and_cross_platform_sources_can_be_paged_without_search_snippet_citations() {
    use serde_json::json;
    for platform in [
        "imessage",
        "gmail",
        "whatsapp",
        "slack",
        "discord",
        "linkedin",
        "instagram",
        "twitter",
        "telegram",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let mut source = message(0);
        source.platform = platform.into();
        source.body = format!(
            "document {} https://example.invalid/final-plan",
            "x".repeat(8_100)
        );
        store.upsert_email_backfill(&source, 1).unwrap();
        for i in 1..15 {
            store.upsert_email_backfill(&message(i), 1).unwrap();
        }
        augmentagent_messages::index::drain_batch(&store, 100).unwrap();
        let seed = load_seed(&store, &message(15), None).unwrap();
        assert!(!seed.messages.iter().any(|m| m.message_id == "m00"));
        let agent = script(vec![
            json!({"action":"search","query":format!("in:{platform} document"),"reason":"find older document"}),
            json!({"action":"read_message","message_id":"m00","reason":"inspect source"}),
            json!({"action":"read_message","message_id":"m00","offset":8000,"reason":"read omitted link"}),
            json!({"action":"ready","facts":[{"text":"Document https://example.invalid/final-plan","source_ids":["m00"]}],"gaps":[]}),
        ]);
        let evidence = augmentagent_channel_core::draft_evidence::gather(
            &store,
            &agent,
            &seed,
            "send me the document we discussed",
            None,
        )
        .await;
        assert!(evidence.complete, "{platform}: {:?}", evidence.gaps);
        assert_eq!(evidence.reads[1].result["next_offset"], 8000);
        assert!(evidence.reads[2].result["text"]
            .as_str()
            .unwrap()
            .contains("https://example.invalid/final-plan"));
    }
}

#[test]
fn slack_trigger_uses_source_time_for_the_history_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let mut current = message(1);
    current.platform = "slack".into();
    current.date = "1767225660.000001".into();
    for (i, date) in [
        (0, "1767225600.000001"),
        (1, "1767225660.000001"),
        (2, "1767225720.000001"),
    ] {
        let mut source = message(i);
        source.platform = "slack".into();
        source.date = date.into();
        store.upsert_email_backfill(&source, 1).unwrap();
    }
    let seed = load_seed(&store, &current, None).unwrap();
    assert_eq!(
        seed.messages
            .iter()
            .map(|m| m.message_id.as_str())
            .collect::<Vec<_>>(),
        ["m00", "m01"]
    );
    assert!(!seed.gaps.iter().any(|g| g.contains("timestamp")));
}

/// A slow model (#1837: live decisions take ~40 s) that keeps searching
/// until the coordinator tells it the decision is final.
struct SearchesUntilFinal {
    delay: std::time::Duration,
    obeys_final: bool,
    prompts: std::sync::Mutex<Vec<serde_json::Value>>,
}
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for SearchesUntilFinal {
    async fn call(
        &self,
        _opts: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> anyhow::Result<String> {
        let prompt: serde_json::Value = serde_json::from_str(prompt).unwrap();
        let last = prompt["budget"]["final_decision"] == true;
        self.prompts.lock().unwrap().push(prompt);
        tokio::time::sleep(self.delay).await;
        Ok(if last && self.obeys_final {
            serde_json::json!({"action":"ready","facts":[],"gaps":["The invite was not found in the sources read"]})
        } else {
            serde_json::json!({"action":"search","query":"invite","reason":"look for the invite"})
        }
        .to_string())
    }
}
fn searches_until_final(obeys_final: bool) -> SearchesUntilFinal {
    SearchesUntilFinal {
        delay: std::time::Duration::from_secs(40),
        obeys_final,
        prompts: std::sync::Mutex::new(Vec::new()),
    }
}

#[tokio::test(start_paused = true)]
async fn slow_decisions_conclude_before_the_budget_instead_of_timing_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let seed = load_seed(&store, &message(1), None).unwrap();
    let agent = searches_until_final(true);
    let evidence = augmentagent_channel_core::draft_evidence::gather(
        &store,
        &agent,
        &seed,
        "not seeing an invite for today",
        None,
    )
    .await;
    // 40 s + 40 s fits; a third 40 s decision would not, so the second one is
    // the last and the check finishes with its gap instead of at the wall.
    assert!(evidence.complete, "{:?}", evidence.gaps);
    assert_eq!(evidence.model_decisions, 2);
    assert_eq!(evidence.reads.len(), 1, "the first search still ran");
    assert!(evidence.gaps.iter().any(|g| g.contains("invite")));
    assert!(!evidence.gaps.iter().any(|g| g.contains("time limit")));
    let prompts = agent.prompts.lock().unwrap();
    assert_eq!(prompts[0]["budget"]["final_decision"], false);
    assert_eq!(prompts[1]["budget"]["final_decision"], true);
    assert!(prompts[1]["stop_guidance"]
        .as_str()
        .unwrap()
        .contains("final decision"));
}

#[tokio::test(start_paused = true)]
async fn a_final_decision_that_asks_for_more_retrieval_is_not_executed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let seed = load_seed(&store, &message(1), None).unwrap();
    let agent = searches_until_final(false);
    let evidence = augmentagent_channel_core::draft_evidence::gather(
        &store,
        &agent,
        &seed,
        "not seeing an invite for today",
        None,
    )
    .await;
    assert!(!evidence.complete);
    assert_eq!(
        evidence.model_decisions, 2,
        "no third call started at the wall"
    );
    assert_eq!(evidence.reads.len(), 1, "the discarded search never ran");
    assert!(evidence.elapsed_ms < 90_000, "{}", evidence.elapsed_ms);
    assert!(evidence.gaps.iter().any(|g| g.contains("time limit")));
}

#[tokio::test(start_paused = true)]
async fn fast_decisions_keep_searching_until_the_last_step() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let seed = load_seed(&store, &message(1), None).unwrap();
    let mut agent = searches_until_final(true);
    agent.delay = std::time::Duration::from_secs(1);
    let evidence = augmentagent_channel_core::draft_evidence::gather(
        &store,
        &agent,
        &seed,
        "not seeing an invite for today",
        None,
    )
    .await;
    assert!(evidence.complete, "{:?}", evidence.gaps);
    assert_eq!(evidence.model_decisions, 8);
    assert_eq!(evidence.reads.len(), 7);
    let prompts = agent.prompts.lock().unwrap();
    assert!(prompts[..7]
        .iter()
        .all(|p| p["budget"]["final_decision"] == false));
    assert_eq!(prompts[7]["budget"]["final_decision"], true);
}
