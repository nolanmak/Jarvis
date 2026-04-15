//! `GmailChannel` — poll loop + per-email reasoning dispatch.
//!
//! Per-email flow (three specialized Claude calls):
//!
//! 1. **Triage** (Haiku, no tools)   → `{decision: reply|skip|flag, reason}`
//! 2. **Draft**  (Opus, wiki read)   → plain text draft, only on `reply`
//! 3. **Ingest** (Haiku, wiki write) → async, best-effort; see `ingest.rs`
//!
//! Cost win: skips and flags never pay the Opus premium.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use augmentagent_approval_discord::{ApprovalBroker, ApprovalError, ApprovalOutcome, NoopBroker};
use augmentagent_store::{ActionStatus, Store, TriageResult};

use crate::decision::{parse as parse_decision, DecisionKind};
use crate::gmail::GmailApi;
use crate::ingest::{spawn_ingest, IngestTrigger};
use crate::prompt::{
    draft_user_message, redraft_message, triage_user_message, SkillPrompt, TRIAGE_SYSTEM,
};

#[derive(Clone, Debug)]
pub struct GmailChannelConfig {
    pub poll_interval: Duration,
    pub per_account_limit: u32,
    pub skill_dir: PathBuf,
    pub dry_run: bool,
    pub model: Option<String>,
    /// Max number of revise rounds before we give up on a reply.
    pub max_revise_rounds: u8,
    /// Wiki root. `None` = no wiki integration (drafting gets no extra context).
    pub wiki_root: Option<PathBuf>,
    /// Path to schema/wiki-skill.md. Required when wiki_root is set; loaded once per poll cycle.
    pub wiki_schema_path: Option<PathBuf>,
}

impl Default for GmailChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(120),
            per_account_limit: 100,
            skill_dir: PathBuf::from("skills/email-triage"),
            dry_run: true,
            model: None,
            max_revise_rounds: 3,
            wiki_root: None,
            wiki_schema_path: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub accounts_polled: usize,
    pub emails_checked: usize,
    pub new_emails: usize,
    pub skipped: usize,
    pub flagged: usize,
    pub replied_dry_run: usize,
    pub sent: usize,
    pub rejected: usize,
    pub errors: usize,
}

/// Per-call options for a `Reasoner`. Each call type (triage, draft, ingest)
/// gets a different preset — see `crate::reasoner::ReasonerOpts::triage`,
/// `::draft`, `::ingest`.
#[derive(Debug, Clone)]
pub struct ReasonerOpts {
    pub system_prompt: String,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub permission_mode: String,
}

/// Trait the channel uses to reach Claude. Test doubles stub this.
#[async_trait]
pub trait Reasoner: Send + Sync {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String>;
}

pub struct GmailChannel<G: GmailApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub gmail: Arc<G>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: GmailChannelConfig,
    /// Schema contents (`wiki-skill.md`), lazily cached. `None` means wiki disabled.
    wiki_schema: Option<String>,
}

impl<G: GmailApi, R: Reasoner + 'static> GmailChannel<G, R> {
    pub fn new(
        store: Arc<Store>,
        gmail: Arc<G>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        config: GmailChannelConfig,
    ) -> Self {
        // Bootstrap wiki + load schema if enabled. Wiki layer is best-effort —
        // failures downgrade to "wiki disabled" rather than aborting startup.
        let wiki_schema = match (&config.wiki_root, &config.wiki_schema_path) {
            (Some(root), Some(schema_path)) => {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                if let Err(e) = layout.bootstrap() {
                    warn!("wiki bootstrap failed, disabling wiki: {e}");
                    None
                } else {
                    match std::fs::read_to_string(schema_path) {
                        Ok(s) if !s.trim().is_empty() => {
                            info!(path = %schema_path.display(), "wiki schema loaded");
                            Some(s)
                        }
                        Ok(_) => {
                            warn!("wiki schema file is empty; disabling wiki");
                            None
                        }
                        Err(e) => {
                            warn!("wiki schema read failed, disabling wiki: {e}");
                            None
                        }
                    }
                }
            }
            _ => None,
        };
        Self {
            store,
            gmail,
            reasoner,
            approvals,
            config,
            wiki_schema,
        }
    }

    /// Build a channel with the no-op approval broker. Used when `dry_run = true`.
    pub fn dry_run(
        store: Arc<Store>,
        gmail: Arc<G>,
        reasoner: Arc<R>,
        config: GmailChannelConfig,
    ) -> Self {
        Self::new(store, gmail, reasoner, Arc::new(NoopBroker), config)
    }

    /// Fire a best-effort wiki ingest if wiki is enabled. No-op otherwise.
    fn maybe_ingest(
        &self,
        email: &augmentagent_store::Email,
        decision: DecisionKind,
        reason: Option<&str>,
        draft: Option<&str>,
        trigger: IngestTrigger,
    ) {
        let (Some(root), Some(schema)) = (&self.config.wiki_root, &self.wiki_schema) else {
            return;
        };
        spawn_ingest(
            Arc::clone(&self.reasoner),
            root.clone(),
            schema.clone(),
            email.clone(),
            decision,
            reason.map(str::to_string),
            draft.map(str::to_string),
            trigger,
        );
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("gmail channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(outcome) => info!(?outcome, "gmail poll complete"),
                        Err(e) => error!("gmail poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let accounts = self.store.get_active_gmail_accounts()?;
        outcome.accounts_polled = accounts.len();
        if accounts.is_empty() {
            warn!("no active gmail accounts; nothing to poll");
            return Ok(outcome);
        }

        let skill = SkillPrompt::load(&self.config.skill_dir);
        let learned = skill.load_learned();

        for account in accounts {
            match self
                .gmail
                .fetch_unread(&account.entity_id, self.config.per_account_limit)
                .await
            {
                Ok(emails) => {
                    outcome.emails_checked += emails.len();
                    for email in emails {
                        match self
                            .handle_email(&skill.system, &learned, &account.entity_id, email)
                            .await
                        {
                            Ok(kind) => match kind {
                                Some(DispatchOutcome::Skipped) => outcome.skipped += 1,
                                Some(DispatchOutcome::Flagged) => outcome.flagged += 1,
                                Some(DispatchOutcome::DryRun) => outcome.replied_dry_run += 1,
                                Some(DispatchOutcome::Sent) => outcome.sent += 1,
                                Some(DispatchOutcome::Rejected) => outcome.rejected += 1,
                                None => {}
                            },
                            Err(e) => {
                                outcome.errors += 1;
                                error!("handle_email failed: {e:#}");
                            }
                        }
                    }
                }
                Err(e) => {
                    outcome.errors += 1;
                    error!(account = %account.entity_id, "fetch_unread failed: {e}");
                }
            }
        }

        outcome.new_emails = outcome.skipped
            + outcome.flagged
            + outcome.replied_dry_run
            + outcome.sent
            + outcome.rejected;
        Ok(outcome)
    }

    async fn handle_email(
        &self,
        draft_skill: &str,
        learned: &str,
        entity_id: &str,
        email: augmentagent_store::Email,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        let is_new = self.store.upsert_email(&email)?;
        if !is_new || self.store.is_message_processed(&email.message_id)? {
            return Ok(None);
        }

        // --- 1. TRIAGE call (Haiku, no tools, returns {decision, reason})
        let triage_opts = crate::reasoner::triage_opts();
        let triage_prompt = triage_user_message(&email, learned);
        let raw = self.reasoner.call(&triage_opts, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "triage parse failed: {e}; raw={raw}");
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Error,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                return Err(e.into());
            }
        };

        match decision.decision {
            DecisionKind::Skip => {
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Skipped,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                println!(
                    "[skip] {} from={} reason={}",
                    email.message_id,
                    email.from,
                    decision.reason.as_deref().unwrap_or("")
                );
                self.maybe_ingest(
                    &email,
                    DecisionKind::Skip,
                    decision.reason.as_deref(),
                    None,
                    IngestTrigger::Triaged,
                );
                Ok(Some(DispatchOutcome::Skipped))
            }
            DecisionKind::Flag => {
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Flagged,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                println!(
                    "[flag] {} from={} reason={}",
                    email.message_id,
                    email.from,
                    decision.reason.as_deref().unwrap_or("")
                );
                self.maybe_ingest(
                    &email,
                    DecisionKind::Flag,
                    decision.reason.as_deref(),
                    None,
                    IngestTrigger::Triaged,
                );
                Ok(Some(DispatchOutcome::Flagged))
            }
            DecisionKind::Reply => {
                // --- 2. DRAFT call (Opus, with wiki read access when enabled)
                let draft_opts = crate::reasoner::draft_opts(
                    draft_skill.to_string(),
                    self.config.wiki_root.clone(),
                );
                let wiki_hint = self
                    .config
                    .wiki_root
                    .as_ref()
                    .map(|root| {
                        let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                        augmentagent_wiki::WikiReader::new(&layout).draft_hint(&email)
                    })
                    .unwrap_or_default();
                let draft_prompt = draft_user_message(&email, &wiki_hint);
                let draft = match self.reasoner.call(&draft_opts, &draft_prompt).await {
                    Ok(s) => s.trim().to_string(),
                    Err(e) => {
                        error!(message_id = %email.message_id, "draft call failed: {e}");
                        self.store.log_action(
                            &email.message_id,
                            email.thread_id.as_deref(),
                            &email.from,
                            &email.subject,
                            Some(&email.body),
                            None,
                            ActionStatus::Error,
                        )?;
                        self.store
                            .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                        return Err(e);
                    }
                };

                if self.config.dry_run {
                    self.store.log_action(
                        &email.message_id,
                        email.thread_id.as_deref(),
                        &email.from,
                        &email.subject,
                        Some(&email.body),
                        Some(&draft),
                        ActionStatus::DryRun,
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    println!(
                        "[reply dry-run] {} from={} subject={}\n--- draft ---\n{}\n--- /draft ---",
                        email.message_id, email.from, email.subject, draft,
                    );
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        decision.reason.as_deref(),
                        Some(&draft),
                        IngestTrigger::DryRunDrafted,
                    );
                    return Ok(Some(DispatchOutcome::DryRun));
                }
                self.dispatch_reply(draft_skill, entity_id, email, draft)
                    .await
            }
        }
    }

    async fn dispatch_reply(
        &self,
        draft_skill: &str,
        entity_id: &str,
        email: augmentagent_store::Email,
        initial_draft: String,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        let action_id = self.store.log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(&initial_draft),
            ActionStatus::Pending,
        )?;

        // Create Gmail draft up-front so Approve just needs to send.
        let draft_id = match self
            .gmail
            .create_draft(
                entity_id,
                &email.from,
                &reply_subject(&email.subject),
                &initial_draft,
                email.thread_id.as_deref(),
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.store.update_action_status(
                    &action_id,
                    ActionStatus::Error,
                    None,
                    Some(&format!("create_draft: {e}")),
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                return Err(e.into());
            }
        };

        let mut current_draft = initial_draft;
        let mut rounds: u8 = 0;
        loop {
            let outcome = self
                .approvals
                .request(&action_id, &email, &current_draft)
                .await;

            match outcome {
                Ok(ApprovalOutcome::Approved { final_draft }) => {
                    let _ = final_draft;
                    match self.gmail.send_draft(entity_id, &draft_id).await {
                        Ok(()) => {
                            self.store.update_action_status(
                                &action_id,
                                ActionStatus::Sent,
                                Some(&current_draft),
                                None,
                            )?;
                            self.store
                                .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                            info!(action_id, "reply sent");
                            self.maybe_ingest(
                                &email,
                                DecisionKind::Reply,
                                None,
                                Some(&current_draft),
                                IngestTrigger::Sent,
                            );
                            return Ok(Some(DispatchOutcome::Sent));
                        }
                        Err(e) => {
                            self.store.update_action_status(
                                &action_id,
                                ActionStatus::Error,
                                None,
                                Some(&format!("send_draft: {e}")),
                            )?;
                            self.store
                                .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                            return Err(e.into());
                        }
                    }
                }
                Ok(ApprovalOutcome::Revise { feedback }) => {
                    rounds += 1;
                    if rounds > self.config.max_revise_rounds {
                        self.store.update_action_status(
                            &action_id,
                            ActionStatus::Rejected,
                            None,
                            Some("exceeded max revise rounds"),
                        )?;
                        self.store
                            .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                        warn!(action_id, "revise exceeded max rounds");
                        return Ok(Some(DispatchOutcome::Rejected));
                    }

                    let revise_opts = crate::reasoner::draft_opts(
                        draft_skill.to_string(),
                        self.config.wiki_root.clone(),
                    );
                    let redraft = self
                        .reasoner
                        .call(
                            &revise_opts,
                            &redraft_message(&email, &current_draft, &feedback),
                        )
                        .await?;
                    current_draft = redraft.trim().to_string();
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Pending,
                        Some(&current_draft),
                        None,
                    )?;
                    warn!(
                        "revise: keeping original Gmail draft {} (UPDATE_DRAFT not yet wired)",
                        draft_id
                    );
                    continue;
                }
                Ok(ApprovalOutcome::Skipped) => {
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Rejected,
                        None,
                        Some("skipped by approver"),
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        None,
                        Some(&current_draft),
                        IngestTrigger::Rejected,
                    );
                    return Ok(Some(DispatchOutcome::Rejected));
                }
                Err(ApprovalError::TimedOut) => {
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::TimedOut,
                        None,
                        Some("approval timeout"),
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    warn!(action_id, "approval timed out");
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        None,
                        Some(&current_draft),
                        IngestTrigger::Rejected,
                    );
                    return Ok(Some(DispatchOutcome::Rejected));
                }
                Err(e) => {
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Error,
                        None,
                        Some(&format!("approval: {e}")),
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    return Err(anyhow::anyhow!("approval error: {e}"));
                }
            }
        }
    }
}

fn reply_subject(original: &str) -> String {
    if original.to_ascii_lowercase().starts_with("re:") {
        original.to_string()
    } else {
        format!("Re: {original}")
    }
}

#[derive(Debug, Clone, Copy)]
enum DispatchOutcome {
    Skipped,
    Flagged,
    DryRun,
    Sent,
    Rejected,
}

// Silence dead_code warning for TRIAGE_SYSTEM constant which is exported for
// callers but not otherwise referenced at compile time inside this module.
#[allow(dead_code)]
const _TRIAGE_SYSTEM_REF: &str = TRIAGE_SYSTEM;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_store::Email;

    struct StubGmail {
        emails: Vec<Email>,
    }
    #[async_trait]
    impl GmailApi for StubGmail {
        async fn fetch_unread(
            &self,
            _e: &str,
            _l: u32,
        ) -> Result<Vec<Email>, crate::gmail::GmailError> {
            Ok(self.emails.clone())
        }
        async fn create_draft(
            &self,
            _e: &str,
            _t: &str,
            _s: &str,
            _b: &str,
            _th: Option<&str>,
        ) -> Result<String, crate::gmail::GmailError> {
            Ok("draft".into())
        }
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
    }

    /// Scripted reasoner: returns responses in order per call — first call is
    /// triage, second is draft (and each subsequent is another draft/redraft).
    struct ScriptedReasoner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(resps: I) -> Self {
            Self {
                responses: std::sync::Mutex::new(resps.into_iter().map(String::from).collect()),
            }
        }
    }
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
            let mut q = self.responses.lock().unwrap();
            Ok(q.pop_front()
                .unwrap_or_else(|| "{\"decision\":\"skip\",\"reason\":\"stub\"}".into()))
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                INSERT INTO gmail_accounts VALUES ('a1', 'c1', 'me@x.com', NULL, 'acc1', 1, 0);
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    #[tokio::test]
    async fn dry_run_skip_flow() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                message_id: "m1".into(),
                thread_id: None,
                from: "noreply@foo.com".into(),
                subject: "Newsletter".into(),
                body: "buy things".into(),
                date: "2026-04-13".into(),
                account_entity_id: Some("acc1".into()),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"newsletter"}"#,
        ]));
        let ch = GmailChannel::dry_run(
            store,
            gmail,
            reasoner,
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(out.replied_dry_run, 0);
        assert_eq!(out.errors, 0);
    }

    #[tokio::test]
    async fn dry_run_reply_flow_two_calls() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                message_id: "m2".into(),
                thread_id: Some("t2".into()),
                from: "user@client.com".into(),
                subject: "Question".into(),
                body: "how do I...".into(),
                date: "2026-04-13".into(),
                account_entity_id: Some("acc1".into()),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            // triage
            r#"{"decision":"reply","reason":"actionable question"}"#,
            // draft (plain text)
            "Sure — here is the answer.",
        ]));
        let ch = GmailChannel::dry_run(
            store,
            gmail,
            reasoner,
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.replied_dry_run, 1);
    }

    struct ApproveBroker;
    #[async_trait]
    impl ApprovalBroker for ApproveBroker {
        async fn request(
            &self,
            _action_id: &str,
            _email: &Email,
            initial_draft: &str,
        ) -> Result<ApprovalOutcome, ApprovalError> {
            Ok(ApprovalOutcome::Approved {
                final_draft: initial_draft.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn live_reply_flow_sends() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                message_id: "m3".into(),
                thread_id: Some("t3".into()),
                from: "user@client.com".into(),
                subject: "Ping".into(),
                body: "any update?".into(),
                date: "2026-04-13".into(),
                account_entity_id: Some("acc1".into()),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"ping"}"#,
            "Yes — shipping today.",
        ]));
        let ch = GmailChannel::new(
            store,
            gmail,
            reasoner,
            Arc::new(ApproveBroker),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.sent, 1);
    }

    struct SkipBroker;
    #[async_trait]
    impl ApprovalBroker for SkipBroker {
        async fn request(
            &self,
            _: &str,
            _: &Email,
            _: &str,
        ) -> Result<ApprovalOutcome, ApprovalError> {
            Ok(ApprovalOutcome::Skipped)
        }
    }

    #[tokio::test]
    async fn live_reply_flow_rejected_on_skip() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                message_id: "m4".into(),
                thread_id: None,
                from: "user@client.com".into(),
                subject: "Ping".into(),
                body: "any update?".into(),
                date: "2026-04-13".into(),
                account_entity_id: Some("acc1".into()),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"ping"}"#,
            "hi",
        ]));
        let ch = GmailChannel::new(
            store,
            gmail,
            reasoner,
            Arc::new(SkipBroker),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.rejected, 1);
        assert_eq!(out.sent, 0);
    }
}
