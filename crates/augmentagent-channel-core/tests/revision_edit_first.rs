//! #2012 — a revision is one edit call; retrieval only when the model asks.
use augmentagent_channel_core::draft_workflow::{
    parse_edit_response, redraft_with, EditResponse, RevisionConfig,
};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_store::{ActionStatus, Email, Store};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

fn message(i: usize, thread: &str) -> Email {
    Email {
        message_id: format!("m{i:02}"),
        thread_id: Some(thread.into()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Edit,
    Evidence,
    Full,
}

fn kind_of(opts: &ReasonerOpts, prompt: &str) -> Kind {
    if opts.system_prompt.contains("You select evidence") {
        Kind::Evidence
    } else if prompt.contains("NEEDS_CONTEXT:") {
        Kind::Edit
    } else {
        Kind::Full
    }
}

/// Answers per call kind from a queue, optionally after a (virtual) delay.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(Kind, String)>>,
    edit: Mutex<VecDeque<anyhow::Result<String>>>,
    evidence: Mutex<VecDeque<Value>>,
    full: Mutex<VecDeque<String>>,
    evidence_delay: Duration,
}

impl Recorder {
    fn edit(self, reply: &str) -> Self {
        self.edit.lock().unwrap().push_back(Ok(reply.into()));
        self
    }
    fn edit_err(self) -> Self {
        self.edit
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("provider down")));
        self
    }
    fn evidence(self, decision: Value) -> Self {
        self.evidence.lock().unwrap().push_back(decision);
        self
    }
    fn full(self, reply: &str) -> Self {
        self.full.lock().unwrap().push_back(reply.into());
        self
    }
    fn kinds(&self) -> Vec<Kind> {
        self.calls.lock().unwrap().iter().map(|(k, _)| *k).collect()
    }
    fn prompt(&self, kind: Kind) -> String {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, p)| p.clone())
            .expect("call of that kind")
    }
}

#[async_trait::async_trait]
impl Reasoner for Recorder {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        let kind = kind_of(opts, prompt);
        self.calls.lock().unwrap().push((kind, prompt.to_string()));
        match kind {
            Kind::Edit => {
                assert!(
                    opts.allowed_tools.is_empty(),
                    "the edit call needs no tool round-trips"
                );
                self.edit
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected edit call")
            }
            Kind::Evidence => {
                tokio::time::sleep(self.evidence_delay).await;
                Ok(self
                    .evidence
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected evidence call")
                    .to_string())
            }
            Kind::Full => Ok(self
                .full
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected full redraft call")),
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    wiki: std::path::PathBuf,
    email: Email,
    action: String,
}

const PREVIOUS: &str =
    "Hey! Friday works.\nOne heads-up: the old PRs touch marketing files.\nSee you then.";

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("about")).unwrap();
    std::fs::create_dir_all(wiki.join("projects")).unwrap();
    std::fs::write(wiki.join("about/me.md"), "# Owner\nKeep texts short.").unwrap();
    std::fs::write(wiki.join("projects/plan.md"), "Launch is March 3.").unwrap();
    let email = message(1, "thread");
    store.upsert_email_backfill(&email, 1).unwrap();
    let action = store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(PREVIOUS),
            ActionStatus::Pending,
        )
        .unwrap();
    Fixture {
        _dir: dir,
        store,
        wiki,
        email,
        action,
    }
}

fn config() -> RevisionConfig {
    RevisionConfig::default()
}

async fn revise(f: &Fixture, agent: &Recorder, feedback: &str, cfg: RevisionConfig) -> String {
    redraft_with(
        &cfg,
        &f.store,
        agent,
        &f.email,
        &f.action,
        PREVIOUS,
        feedback,
        Some(&f.wiki),
        "Write a reply",
    )
    .await
    .unwrap()
}

fn receipts(f: &Fixture) -> Vec<Value> {
    f.store
        .draft_context_receipts(&f.action)
        .unwrap()
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect()
}

/// The receipt an initial draft would have left: one cross-thread message and
/// one project page actually read.
fn attach_original_receipt(f: &Fixture, wiki_sha: &str) {
    let mut other = message(5, "other-thread");
    other.body = "The venue is Pier 9.".into();
    f.store.upsert_email_backfill(&other, 1).unwrap();
    let receipt = json!({
        "version":1,
        "reads":[
            {"operation":"search","message_id":null,"path":null},
            {"operation":"read_message","message_id":"m05","path":null},
            {"operation":"read_wiki","message_id":null,"path":"projects/plan.md","sha256":wiki_sha},
        ],
        "fact_sources":[["m05"],["projects/plan.md"],["m01"]],
        "gaps":[],
    });
    let id = f
        .store
        .save_draft_context_receipt(&f.email.message_id, &receipt)
        .unwrap();
    assert!(f
        .store
        .attach_draft_context_receipt(&id, &f.action)
        .unwrap());
}

#[tokio::test]
async fn remove_line_revision_is_one_call_and_keeps_rest_verbatim() {
    let f = fixture();
    let agent = Recorder::default().edit("Hey! Friday works.\nSee you then.");
    let draft = revise(&f, &agent, "remove the heads-up line", config()).await;
    assert_eq!(draft, "Hey! Friday works.\nSee you then.");
    assert_eq!(agent.kinds(), vec![Kind::Edit]);
    let prompt = agent.prompt(Kind::Edit);
    assert!(prompt.contains(PREVIOUS));
    assert!(prompt.contains("remove the heads-up line"));
    assert!(prompt.contains("smallest change"));
    let receipts = receipts(&f);
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["mode"], "revision_edit");
    assert_eq!(receipts[0]["evidence_mode"], "skipped");
}

#[tokio::test]
async fn preset_revision_makes_exactly_one_model_call() {
    let f = fixture();
    let agent = Recorder::default().edit("Friday works. See you then.");
    revise(&f, &agent, "Make it shorter", config()).await;
    assert_eq!(agent.kinds(), vec![Kind::Edit]);
}

#[tokio::test]
async fn style_only_revision_reuses_original_receipt_sources() {
    let f = fixture();
    use sha2::{Digest, Sha256};
    let sha = format!("{:x}", Sha256::digest(b"Launch is March 3."));
    attach_original_receipt(&f, &sha);
    let agent = Recorder::default().edit("Friday works!");
    revise(&f, &agent, "make it warmer", config()).await;
    assert_eq!(agent.kinds(), vec![Kind::Edit]);
    let prompt = agent.prompt(Kind::Edit);
    assert!(prompt.contains("The venue is Pier 9."), "{prompt}");
    assert!(prompt.contains("Launch is March 3."));
    let latest = receipts(&f).pop().unwrap();
    assert_eq!(latest["mode"], "revision_edit");
    let reused: Vec<_> = latest["reused_sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(reused, vec!["m05", "projects/plan.md"]);
    assert!(latest["changed_sources"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn revision_rehydration_marks_changed_wiki_page() {
    let f = fixture();
    attach_original_receipt(&f, "stale-sha");
    let agent = Recorder::default().edit("Friday works!");
    revise(&f, &agent, "make it warmer", config()).await;
    assert!(agent.prompt(Kind::Edit).contains("Launch is March 3."));
    let latest = receipts(&f).pop().unwrap();
    assert_eq!(latest["changed_sources"], json!(["projects/plan.md"]));
}

#[tokio::test]
async fn reused_wiki_path_outside_the_wiki_is_never_read() {
    let f = fixture();
    let receipt = json!({"reads":[{"operation":"read_wiki","path":"../secret.md","sha256":"x"}],"fact_sources":[["../secret.md"]]});
    std::fs::write(f.wiki.parent().unwrap().join("secret.md"), "TOP SECRET").unwrap();
    let id = f
        .store
        .save_draft_context_receipt(&f.email.message_id, &receipt)
        .unwrap();
    assert!(f
        .store
        .attach_draft_context_receipt(&id, &f.action)
        .unwrap());
    let agent = Recorder::default().edit("ok");
    revise(&f, &agent, "make it warmer", config()).await;
    assert!(!agent.prompt(Kind::Edit).contains("TOP SECRET"));
}

#[tokio::test]
async fn needs_context_escalates_to_retrieval_once() {
    let f = fixture();
    let agent = Recorder::default()
        .edit("NEEDS_CONTEXT: the meeting link they sent")
        .evidence(json!({"action":"ready","facts":[],"gaps":[]}))
        .full("Here is the link.");
    let draft = revise(&f, &agent, "include the meeting link", config()).await;
    assert_eq!(draft, "Here is the link.");
    assert_eq!(agent.kinds(), vec![Kind::Edit, Kind::Evidence, Kind::Full]);
    assert!(agent
        .prompt(Kind::Evidence)
        .contains("the meeting link they sent"));
    let latest = receipts(&f).pop().unwrap();
    assert_eq!(latest["evidence_mode"], "gathered");
    assert_eq!(latest["need"], "the meeting link they sent");
}

#[tokio::test]
async fn malformed_edit_response_retries_then_falls_back_to_full_path() {
    let f = fixture();
    let agent = Recorder::default()
        .edit("   ")
        .edit("NEEDS_CONTEXT:")
        .evidence(json!({"action":"ready","facts":[],"gaps":[]}))
        .full("Fallback draft.");
    let draft = revise(&f, &agent, "make it warmer", config()).await;
    assert_eq!(draft, "Fallback draft.");
    assert_eq!(
        agent.kinds(),
        vec![Kind::Edit, Kind::Edit, Kind::Evidence, Kind::Full]
    );
    assert_eq!(receipts(&f).pop().unwrap()["evidence_mode"], "fallback");
}

#[tokio::test]
async fn edit_call_error_falls_back_to_full_path_instead_of_failing_the_card() {
    let f = fixture();
    let agent = Recorder::default()
        .edit_err()
        .evidence(json!({"action":"ready","facts":[],"gaps":[]}))
        .full("Fallback draft.");
    let draft = revise(&f, &agent, "make it warmer", config()).await;
    assert_eq!(draft, "Fallback draft.");
    assert_eq!(agent.kinds(), vec![Kind::Edit, Kind::Evidence, Kind::Full]);
}

#[tokio::test(start_paused = true)]
async fn needs_context_lookup_respects_revision_budget_and_names_the_need() {
    let f = fixture();
    let mut agent = Recorder::default()
        .edit("NEEDS_CONTEXT: the meeting link")
        .evidence(json!({"action":"ready","facts":[],"gaps":[]}))
        .full("I'll send the link shortly.");
    agent.evidence_delay = Duration::from_secs(60);
    let cfg = RevisionConfig {
        evidence_budget: Duration::from_secs(25),
        ..config()
    };
    let started = tokio::time::Instant::now();
    let draft = revise(&f, &agent, "include the meeting link", cfg).await;
    assert!(
        started.elapsed() <= Duration::from_secs(26),
        "{:?}",
        started.elapsed()
    );
    let (_, needs) = augmentagent_approval_discord::split_needs_input(&draft);
    assert!(
        needs.iter().any(|g| g.text.contains("the meeting link")),
        "{needs:?}"
    );
    assert!(
        !needs.iter().any(|g| g.text.contains("time limit")),
        "{needs:?}"
    );
    assert_eq!(receipts(&f).pop().unwrap()["evidence_mode"], "timeout");
}

#[tokio::test]
async fn direct_edit_never_emits_a_context_gap() {
    let f = fixture();
    let agent = Recorder::default().edit("Friday works.");
    let draft = revise(&f, &agent, "don't say perfect", config()).await;
    let (_, needs) = augmentagent_approval_discord::split_needs_input(&draft);
    assert!(needs.is_empty(), "{needs:?}");
}

#[tokio::test]
async fn new_inbound_since_the_trigger_reaches_the_edit_call() {
    let f = fixture();
    let mut newer = message(2, "thread");
    newer.body = "actually make it Saturday".into();
    f.store.upsert_email_backfill(&newer, 2).unwrap();
    let agent = Recorder::default().edit("Saturday works.");
    revise(&f, &agent, "make it warmer", config()).await;
    assert_eq!(agent.kinds(), vec![Kind::Edit]);
    assert!(agent
        .prompt(Kind::Edit)
        .contains("actually make it Saturday"));
}

#[tokio::test]
async fn prior_owner_instructions_reach_the_edit_call() {
    let f = fixture();
    f.store
        .record_revision_triple(&f.action, PREVIOUS, "Keep it brief", "Friday.")
        .unwrap();
    let agent = Recorder::default().edit("Friday!");
    revise(&f, &agent, "add an exclamation mark", config()).await;
    let prompt = agent.prompt(Kind::Edit);
    assert!(prompt.contains("Keep it brief"));
    assert!(prompt.contains("add an exclamation mark"));
}

#[tokio::test]
async fn edit_first_off_restores_the_evidence_then_redraft_sequence() {
    let f = fixture();
    let agent = Recorder::default()
        .evidence(json!({"action":"ready","facts":[],"gaps":[]}))
        .full("Legacy draft.");
    let cfg = RevisionConfig {
        edit_first: false,
        ..config()
    };
    assert_eq!(
        revise(&f, &agent, "make it warmer", cfg).await,
        "Legacy draft."
    );
    assert_eq!(agent.kinds(), vec![Kind::Evidence, Kind::Full]);
}

#[test]
fn edit_response_fixture_table() {
    use EditResponse::*;
    let ok = |raw: &str| parse_edit_response(raw).ok();
    let rows: Vec<(&str, Option<EditResponse>)> = vec![
        ("Friday works.", Some(Revised("Friday works.".into()))),
        ("  Friday works.\n\n", Some(Revised("Friday works.".into()))),
        (
            "Line one\nLine two",
            Some(Revised("Line one\nLine two".into())),
        ),
        (
            "NEEDS_CONTEXT: the meeting link",
            Some(NeedsContext("the meeting link".into())),
        ),
        (
            "\n  NEEDS_CONTEXT:   the invoice total  \n",
            Some(NeedsContext("the invoice total".into())),
        ),
        (
            "NEEDS_CONTEXT: the link\nextra prose",
            Some(NeedsContext("the link".into())),
        ),
        (
            "needs_context: the link",
            Some(NeedsContext("the link".into())),
        ),
        (
            "`NEEDS_CONTEXT: the link`",
            Some(NeedsContext("the link".into())),
        ),
        ("NEEDS_CONTEXT:", None),
        ("NEEDS_CONTEXT:    ", None),
        ("", None),
        ("   \n  ", None),
        ("```\n```", None),
        // A draft that merely mentions the word mid-text is still a draft.
        (
            "Thanks! (no NEEDS_CONTEXT: here)",
            Some(Revised("Thanks! (no NEEDS_CONTEXT: here)".into())),
        ),
        (
            "<!--aa:assumes x-->\nHi",
            Some(Revised("<!--aa:assumes x-->\nHi".into())),
        ),
    ];
    for (raw, expected) in rows {
        assert_eq!(ok(raw), expected, "row {raw:?}");
    }
}

#[test]
fn revision_config_reads_env_switches_with_safe_defaults() {
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    };
    let default = RevisionConfig::from_lookup(lookup(&[]));
    assert!(default.edit_first);
    assert_eq!(default.evidence_budget, Duration::from_secs(25));
    let off = RevisionConfig::from_lookup(lookup(&[
        ("AUGMENTAGENT_REVISION_EDIT_FIRST", "off"),
        ("AUGMENTAGENT_REVISION_EVIDENCE_BUDGET_SECS", "40"),
    ]));
    assert!(!off.edit_first);
    assert_eq!(off.evidence_budget, Duration::from_secs(40));
    let junk = RevisionConfig::from_lookup(lookup(&[(
        "AUGMENTAGENT_REVISION_EVIDENCE_BUDGET_SECS",
        "0",
    )]));
    assert_eq!(junk.evidence_budget, Duration::from_secs(25));
}

/// Evidence decisions that queue behind a held CLI permit before answering.
struct Queued {
    gate: std::sync::Arc<augmentagent_channel_core::cli_gate::CliGate>,
}

#[async_trait::async_trait]
impl Reasoner for Queued {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        match kind_of(opts, prompt) {
            Kind::Edit => Ok("NEEDS_CONTEXT: the meeting link".into()),
            Kind::Evidence => {
                let _permit = self
                    .gate
                    .acquire_timed("t", "draft", Duration::from_secs(90))
                    .await?;
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(json!({"action":"ready","facts":[],"gaps":[]}).to_string())
            }
            Kind::Full => Ok("Here it is.".into()),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn gate_wait_is_recorded_separately_and_not_billed_to_the_decision() {
    let f = fixture();
    let gate = std::sync::Arc::new(augmentagent_channel_core::cli_gate::CliGate::new(1));
    let held = gate
        .acquire_timed("t", "triage", Duration::from_secs(90))
        .await
        .unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(held);
    });
    let agent = Queued { gate };
    redraft_with(
        &config(),
        &f.store,
        &agent,
        &f.email,
        &f.action,
        PREVIOUS,
        "include the meeting link",
        Some(&f.wiki),
        "Write a reply",
    )
    .await
    .unwrap();
    let latest = receipts(&f).pop().unwrap();
    let steps = latest["step_timings"].as_array().unwrap();
    let of = |kind: &str| {
        steps
            .iter()
            .filter(|s| s["kind"] == kind)
            .map(|s| s["elapsed_ms"].as_u64().unwrap())
            .sum::<u64>()
    };
    assert!((10_000..=10_100).contains(&of("gate_wait")), "{steps:?}");
    assert!((2_000..=2_100).contains(&of("model_decision")), "{steps:?}");
}

/// Every daemon caller spawns the revise future; it must stay `Send`.
#[allow(dead_code)]
fn redraft_future_is_send(f: &Fixture, agent: &Recorder) {
    fn assert_send<T: Send>(_: T) {}
    assert_send(augmentagent_channel_core::draft_workflow::redraft(
        &f.store, agent, &f.email, &f.action, PREVIOUS, "x", None, "s",
    ));
}

/// Delegates to a real provider, recording only the call kinds.
struct Counting<R: Reasoner> {
    inner: R,
    kinds: Mutex<Vec<Kind>>,
}

#[async_trait::async_trait]
impl<R: Reasoner> Reasoner for Counting<R> {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        self.kinds.lock().unwrap().push(kind_of(opts, prompt));
        self.inner.call(opts, prompt).await
    }
}

/// Real-model e2e for the #2012 receipt: spends a few Claude calls on the
/// owner's subscription, so it only runs on request:
/// `cargo test -p augmentagent-channel-core --test revision_edit_first -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live: spawns the real claude CLI"]
async fn live_remove_line_is_one_fast_call_and_missing_link_escalates() {
    let f = fixture();
    let system = "You write short, friendly replies on the owner's behalf.";
    let agent = Counting {
        inner: augmentagent_channel_core::ClaudeCliReasoner::new(),
        kinds: Mutex::new(Vec::new()),
    };
    let started = std::time::Instant::now();
    let draft = redraft_with(
        &config(),
        &f.store,
        &agent,
        &f.email,
        &f.action,
        PREVIOUS,
        "remove the heads-up line, we don't need that in there",
        Some(&f.wiki),
        system,
    )
    .await
    .unwrap();
    let edit_secs = started.elapsed().as_secs_f32();
    println!(
        "remove-line: {edit_secs:.1}s kinds={:?}\n{draft}",
        agent.kinds.lock().unwrap()
    );
    assert_eq!(*agent.kinds.lock().unwrap(), vec![Kind::Edit]);
    assert!(!draft.contains("heads-up"), "{draft}");
    assert!(
        draft.contains("Friday works") && draft.contains("See you then"),
        "{draft}"
    );

    agent.kinds.lock().unwrap().clear();
    let started = std::time::Instant::now();
    let draft = redraft_with(
        &config(),
        &f.store,
        &agent,
        &f.email,
        &f.action,
        PREVIOUS,
        "include the zoom link they sent us earlier",
        Some(&f.wiki),
        system,
    )
    .await
    .unwrap();
    println!(
        "missing-link: {:.1}s kinds={:?}\n{draft}",
        started.elapsed().as_secs_f32(),
        agent.kinds.lock().unwrap()
    );
    let kinds = agent.kinds.lock().unwrap().clone();
    assert_eq!(kinds.first(), Some(&Kind::Edit));
    assert!(
        kinds.contains(&Kind::Evidence),
        "a missing source must escalate: {kinds:?}"
    );
    let latest = receipts(&f).pop().unwrap();
    println!(
        "receipt evidence_mode={} need={}",
        latest["evidence_mode"], latest["need"]
    );
}
