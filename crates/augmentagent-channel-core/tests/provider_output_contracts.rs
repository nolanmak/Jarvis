//! Shared live output contracts using production presets and synthetic inputs.
//! These tests do not send messages or read account data.
use augmentagent_channel_core::{archetype, codex::CodexCliReasoner, decision,
    reasoner::{self, ClaudeCliReasoner, Reasoner}};
use serde_json::Value;

// Consumers tolerate fenced JSON; tone profiles store the descriptor as text.
// Validate the structured payload rather than requiring stricter formatting.
fn structured_object(raw: &str) -> Value {
    let start = raw.find('{').unwrap_or_else(|| panic!("missing structured payload: {raw}"));
    let end = raw.rfind('}').unwrap();
    serde_json::from_str(&raw[start..=end]).unwrap()
}

async fn structured_outputs(provider: &dyn Reasoner) {
    let interval = provider.call(&reasoner::loop_parse_opts(),
        "Every 7 minutes for 35 minutes say fixture-ready").await.unwrap();
    let interval = structured_object(&interval);
    assert_eq!(interval["interval_secs"], 420);
    assert_eq!(interval["duration_secs"], 2100);
    assert!(interval["prompt"].as_str().unwrap().contains("fixture-ready"));

    let ambiguous = provider.call(&reasoner::loop_parse_opts(),
        "Every Monday at 9am say fixture-ready").await.unwrap();
    let ambiguous = structured_object(&ambiguous);
    assert!(ambiguous["error"].as_str().is_some_and(|error| !error.is_empty()));
    assert!(ambiguous.get("cron_expr").is_none(), "must not invent a timezone");

    let choice = provider.call(&reasoner::archetype_pick_opts(),
        "Synthetic inbound message: Can we meet Tuesday at 14:00 UTC instead of Wednesday? Please confirm the new meeting time.")
        .await.unwrap();
    let choice = archetype::parse_choice(&choice);
    assert_eq!(choice.id.as_deref(), Some("scheduling"));
    assert!(choice.confidence >= archetype::CONFIDENCE_FLOOR);

    let triage = provider.call(&reasoner::triage_opts(None),
        "Synthetic bulk newsletter, not a personal message. Subject: Weekly product roundup. Body: This is an automated weekly newsletter sent to all subscribers. No response or action is requested. Unsubscribe using your preferences.")
        .await.unwrap();
    assert_eq!(decision::parse(&triage).unwrap().decision, decision::DecisionKind::Skip);

    let tone = provider.call(&reasoner::tone_summarize_opts(),
        "Synthetic sent-message corpus containing exactly one message:\nHi team,\nThe fixture is ready for review.\nThanks,\nFixture Author")
        .await.unwrap();
    let tone = structured_object(&tone);
    assert_eq!(tone["register"], "insufficient_sample");
    assert!(tone["openers"].is_array());
    assert!(tone["closers"].is_array());
    assert!(tone["punctuation"].is_object());
}

async fn executable_draft(provider: &dyn Reasoner) {
    use augmentagent_channel_core::{code_mode, prompt};
    let manifest = code_mode::manifest_v1();
    let mut options = reasoner::triage_opts(None);
    // Communication handlers use this system prompt with an empty host tool
    // list and default model; actions run through the existing dispatcher.
    options.system_prompt = prompt::code_mode_system(&manifest);
    options.model = None;
    let response = provider.call(&options,
        "Synthetic drafting fixture. All context is supplied here: the sender asks if the fixture is ready, and it is ready. Draft the exact body 'The fixture is ready.' for gmail. No context lookups are necessary. Do not send anything.")
        .await.unwrap();
    let source = code_mode::extract_ts_block(&response).unwrap();
    // This dispatcher has no account or sending capability. The real Deno
    // runner still validates and executes the provider's generated program.
    let dispatcher = code_mode::StubDispatcher::always_null(&["draft"]);
    let outcome = code_mode::run_program(&source, &manifest, &dispatcher).await.unwrap();
    assert_eq!(outcome.trace.len(), 1, "unexpected extra operations");
    assert_eq!(outcome.trace[0].call, "draft");
    assert_eq!(outcome.trace[0].args_summary[0], "gmail");
    assert_eq!(outcome.trace[0].args_summary[1], "The fixture is ready.");
    assert!(outcome.trace[0].error.is_none());
}

#[tokio::test]
#[ignore = "requires Codex login and Deno; executes a synthetic draft without sending"]
async fn codex_generated_draft_executes_in_code_mode() {
    executable_draft(&CodexCliReasoner::openai()).await;
}

#[tokio::test]
#[ignore = "requires Claude login and Deno; same synthetic draft as Codex"]
async fn claude_generated_draft_executes_in_code_mode() {
    executable_draft(&ClaudeCliReasoner::new()).await;
}

#[tokio::test]
#[ignore = "requires Codex login; synthetic structured-output fixtures only"]
async fn codex_production_structured_output_contracts() {
    structured_outputs(&CodexCliReasoner::openai()).await;
}

#[tokio::test]
#[ignore = "requires Claude login; same synthetic fixtures as Codex"]
async fn claude_production_structured_output_contracts() {
    structured_outputs(&ClaudeCliReasoner::new()).await;
}

async fn digest_extraction_and_lint(provider: std::sync::Arc<dyn Reasoner>) {
    use augmentagent_channel_core::{resolve, tool_audit::AuditLogger};
    let digest = provider.call(&reasoner::digest_opts(None),
        "Synthetic last 24 hours. Total emails: 3; flagged: 2; pending approvals: 1.\n\
         ## Flagged items (all, last 24h) — EXHAUSTIVE\n\
         - alpha@example.com | FIXTURE_ALPHA | requires a decision\n\
         - beta@example.com | FIXTURE_BETA | requests a missing document\n\
         ## Pending approvals (all, oldest first) — EXHAUSTIVE\n\
         - gamma@example.com | FIXTURE_GAMMA | waiting 2d\n\
         All context is supplied here. No additional senders or activity.")
        .await.unwrap();
    for marker in ["FIXTURE_ALPHA", "FIXTURE_BETA", "FIXTURE_GAMMA"] {
        assert!(digest.contains(marker), "digest omitted {marker}: {digest}");
    }
    assert!(digest.len() <= 1500, "small digest exceeded the delivery limit");

    let asks = resolve::detect_asks_shadow(&provider, resolve::AskResolveMode::Shadow,
        "Please send me your Calendly booking link so I can choose a time.").await;
    assert!(asks.iter().any(|ask| ask.kind() == resolve::ResolverKind::Calendly
        && ask.conf() >= resolve::INJECT_CONFIDENCE_FLOOR), "{asks:?}");

    let wiki = tempfile::tempdir().unwrap();
    let index = "# Synthetic wiki\n\n[Missing project](projects/missing-fixture.md)\n";
    std::fs::write(wiki.path().join("index.md"), index).unwrap();
    let logs = tempfile::tempdir().unwrap();
    let audit = logs.path().join("audit.jsonl");
    let mut options = reasoner::lint_opts(
        include_str!("../../../schema/wiki-skill.md").into(), wiki.path().into());
    options.audit_logger = Some(std::sync::Arc::new(AuditLogger::new(audit.clone())));
    let report = provider.call(&options, &format!(
        "Run the lint workflow against the synthetic wiki at `{}`. Inspect index.md and verify its link target. Produce a markdown report using relative paths. Do not change any files.", wiki.path().display()))
        .await.unwrap();
    assert!(report.contains("missing-fixture"), "lint missed the broken link at {}: {report}; audit: {}",
        wiki.path().display(), std::fs::read_to_string(&audit).unwrap_or_default());
    assert_eq!(std::fs::read_to_string(wiki.path().join("index.md")).unwrap(), index);
    assert!(!wiki.path().join("projects/missing-fixture.md").exists());
    let records: Vec<Value> = std::fs::read_to_string(audit).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(records.iter().any(|record| record["tool"] == "Read" || record["tool"] == "Grep"),
        "lint must inspect the synthetic source");
}

#[tokio::test]
#[ignore = "requires Codex login; synthetic digest, extraction and read-only lint"]
async fn codex_digest_extraction_and_lint_contracts() {
    digest_extraction_and_lint(std::sync::Arc::new(CodexCliReasoner::openai())).await;
}

#[tokio::test]
#[ignore = "requires Claude login; same synthetic digest, extraction and lint as Codex"]
async fn claude_digest_extraction_and_lint_contracts() {
    digest_extraction_and_lint(std::sync::Arc::new(ClaudeCliReasoner::new())).await;
}
