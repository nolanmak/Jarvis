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

use augmentagent_approval_discord::{ApprovalBroker, NoopBroker};
use augmentagent_store::{ActionStatus, RetryableReply, Store, TriageResult};

use crate::decision::{parse as parse_decision, DecisionKind};
use crate::gmail::GmailApi;
use crate::ingest::{spawn_ingest, IngestTrigger};
use crate::prompt::{draft_user_message, triage_user_message, SkillPrompt, TRIAGE_SYSTEM};

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

    // --- retry queue ---
    /// How often to scan `actions` for errored replies to retry. 0 = disabled.
    pub retry_interval: Duration,
    /// Max retries per action before flipping to `permanent_error`.
    pub retry_max_attempts: i64,
    /// Age cap: errors older than this are abandoned. Default: 24h.
    pub retry_max_age: Duration,
    /// Minimum gap between attempts on the same action. Default: 5m.
    pub retry_min_gap: Duration,
    /// Max actions processed per retry tick (bounds a burst after an outage).
    pub retry_batch: i64,
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
            retry_interval: Duration::from_secs(300), // 5 min
            retry_max_attempts: 5,
            retry_max_age: Duration::from_secs(24 * 60 * 60),
            retry_min_gap: Duration::from_secs(300),
            retry_batch: 10,
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
    pub awaiting_approval: usize,
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
    /// Override the spawned Claude CLI's working directory. Useful to scope
    /// Write/Edit to a specific subtree (e.g. wiki root) so accidental writes
    /// can't escape into the source tree.
    pub cwd: Option<PathBuf>,
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
        let mut poll_ticker = tokio::time::interval(self.config.poll_interval);
        let retry_enabled = !self.config.retry_interval.is_zero();
        let mut retry_ticker = tokio::time::interval(if retry_enabled {
            self.config.retry_interval
        } else {
            // Stub interval; we'll never actually select on this branch.
            Duration::from_secs(3600)
        });
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("gmail channel: shutdown signal received");
                    return Ok(());
                }
                _ = poll_ticker.tick() => {
                    match self.poll_once().await {
                        Ok(outcome) => info!(?outcome, "gmail poll complete"),
                        Err(e) => error!("gmail poll failed: {e:#}"),
                    }
                }
                _ = retry_ticker.tick(), if retry_enabled => {
                    match self.retry_once().await {
                        Ok(n) if n > 0 => info!(retried = n, "retry tick complete"),
                        Ok(_) => {}
                        Err(e) => error!("retry tick failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One pass of the retry queue. Returns the number of actions re-attempted.
    /// Not public since normal callers should rely on `run`, but tests call it
    /// directly to exercise the logic deterministically.
    pub async fn retry_once(&self) -> anyhow::Result<usize> {
        let now_ms = now_millis();
        let candidates: Vec<RetryableReply> = self.store.list_retryable_replies(
            now_ms,
            self.config.retry_max_age.as_millis() as i64,
            self.config.retry_min_gap.as_millis() as i64,
            self.config.retry_max_attempts,
            self.config.retry_batch,
        )?;

        if candidates.is_empty() {
            return Ok(0);
        }
        info!(count = candidates.len(), "retrying errored reply actions");

        let skill = SkillPrompt::load(&self.config.skill_dir);
        let _ = skill.load_learned(); // not needed for retry, but ensures file system is reachable

        let mut attempted = 0usize;
        for item in candidates {
            let entity_id = match &item.email.account_entity_id {
                Some(e) => e.clone(),
                None => {
                    warn!(action_id = %item.action.id, "skipping retry: missing account_entity_id");
                    continue;
                }
            };

            // Bump the retry counter FIRST. If we crash mid-retry, the counter
            // is still incremented — we don't want to risk an infinite loop.
            let new_count = self.store.increment_retry_count(
                &item.action.id,
                self.config.retry_max_attempts,
            )?;
            info!(
                action_id = %item.action.id,
                attempt = new_count,
                max = self.config.retry_max_attempts,
                subject = %item.action.subject,
                "retrying reply"
            );

            let draft = item
                .action
                .draft_body
                .clone()
                .unwrap_or_default();
            // Reuse the existing action_id so the Discord card already showing
            // for this action stays valid (the event handler looks up this id
            // in sqlite on click).
            let result = self
                .dispatch_reply(
                    &entity_id,
                    item.email.clone(),
                    draft,
                    Some(item.action.id.clone()),
                )
                .await;
            if let Err(e) = result {
                warn!(action_id = %item.action.id, "retry attempt failed: {e:#}");
            }
            attempted += 1;
        }
        Ok(attempted)
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
                                Some(DispatchOutcome::AwaitingApproval) => {
                                    outcome.awaiting_approval += 1
                                }
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
            + outcome.awaiting_approval;
        Ok(outcome)
    }

    async fn handle_email(
        &self,
        draft_skill: &str,
        learned: &str,
        entity_id: &str,
        email: augmentagent_store::Email,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        // Always upsert so the body stays fresh. Gate only on *completion* — if
        // the email hasn't been carried to a terminal outcome we process it,
        // even if we've seen the messageId before (retryable error state).
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(None);
        }

        // --- 1. TRIAGE call (Opus, wiki read-only, returns {decision, reason})
        let triage_opts = crate::reasoner::triage_opts(self.config.wiki_root.clone());
        let wiki_hint = self
            .config
            .wiki_root
            .as_ref()
            .map(|root| {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                augmentagent_wiki::WikiReader::new(&layout).triage_hint(&email)
            })
            .unwrap_or_default();
        let triage_prompt = triage_user_message(&email, learned, &wiki_hint);
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
                // NOT mark_email_processed: a triage parse failure is transient
                // (Claude flakiness). Leave agentProcessedAt NULL so the retry
                // tick can pick it up.
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
                // Post the heads-up to Discord. Best-effort — failure to reach
                // the broker shouldn't abort the flag flow (wiki ingest still
                // runs below and the email stays marked complete).
                let reason = decision.reason.as_deref().unwrap_or("flagged");
                if let Err(e) = self.approvals.post_flag_notice(&email, reason).await {
                    warn!(
                        message_id = %email.message_id,
                        "post_flag_notice failed: {e}"
                    );
                } else {
                    info!(
                        message_id = %email.message_id,
                        from = %email.from,
                        "flag notice posted"
                    );
                }
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
                        // NOT mark_email_processed — retry tick will pick up.
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
                self.dispatch_reply(entity_id, email, draft, None).await
            }
        }
    }

    /// Non-blocking reply dispatch: create Gmail draft, log/update the action,
    /// post the approval card, return. The subsequent Approve / Revise / Skip
    /// is handled by the Discord event handler against the sqlite row.
    ///
    /// `existing_action_id` is `None` for first-time dispatch (log a fresh
    /// row), or `Some(id)` from the retry path (reuse the row so old Discord
    /// cards with that action_id remain valid).
    async fn dispatch_reply(
        &self,
        entity_id: &str,
        email: augmentagent_store::Email,
        initial_draft: String,
        existing_action_id: Option<String>,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        let action_id = match existing_action_id {
            Some(id) => {
                self.store.update_action_status(
                    &id,
                    ActionStatus::Pending,
                    Some(&initial_draft),
                    None,
                )?;
                id
            }
            None => self.store.log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some(&initial_draft),
                ActionStatus::Pending,
            )?,
        };

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
                return Err(e.into());
            }
        };
        self.store.set_action_draft_id(&action_id, &draft_id)?;

        if let Err(e) = self
            .approvals
            .post_approval(&action_id, &email, &initial_draft)
            .await
        {
            self.store.update_action_status(
                &action_id,
                ActionStatus::Error,
                None,
                Some(&format!("post_approval: {e}")),
            )?;
            return Err(anyhow::anyhow!("post_approval: {e}"));
        }

        info!(action_id, draft_id = %draft_id, "approval card posted");
        Ok(Some(DispatchOutcome::AwaitingApproval))
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    /// Draft created, approval card posted. Terminal outcome happens
    /// asynchronously when the user clicks a button in Discord.
    AwaitingApproval,
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
        async fn fetch_with_query(
            &self,
            _e: &str,
            _q: &str,
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
        async fn update_draft(
            &self,
            _e: &str,
            _d: &str,
            _t: &str,
            _s: &str,
            _b: &str,
        ) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
        async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
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

    /// Broker that records every post without blocking. Used by tests to
    /// assert the channel's non-blocking dispatch semantics.
    #[derive(Default)]
    struct RecordingBroker {
        posts: std::sync::Mutex<Vec<String>>,
        flag_posts: std::sync::Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(
            &self,
            action_id: &str,
            _email: &Email,
            _draft: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.posts.lock().unwrap().push(action_id.to_string());
            Ok(())
        }

        async fn post_flag_notice(
            &self,
            email: &Email,
            reason: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.flag_posts
                .lock()
                .unwrap()
                .push((email.message_id.clone(), reason.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn flag_decision_posts_notice_no_approval_card() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                message_id: "m-flag".into(),
                thread_id: None,
                from: "friend@edu.com".into(),
                subject: "Catching up".into(),
                body: "saw your post, wanted to reach out...".into(),
                date: "2026-04-19".into(),
                account_entity_id: Some("acc1".into()),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"personal outreach from known contact"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.flagged, 1);
        assert_eq!(out.awaiting_approval, 0);
        // No approval card — flags don't get the draft flow.
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        // But the heads-up notice was posted.
        let flags = broker.flag_posts.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, "m-flag");
        assert!(flags[0].1.contains("personal outreach"));
        // Email IS complete — flag is a terminal triage outcome.
        assert!(store.is_email_complete("m-flag").unwrap());
    }

    #[tokio::test]
    async fn skip_decision_posts_nothing() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                message_id: "m-skip".into(),
                thread_id: None,
                from: "noreply@marketing.com".into(),
                subject: "50% off!".into(),
                body: "deal deal".into(),
                date: "2026-04-19".into(),
                account_entity_id: Some("acc1".into()),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"marketing"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        assert_eq!(broker.flag_posts.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn live_reply_flow_posts_approval_card() {
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
        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.awaiting_approval, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        // Email is NOT complete — awaiting the user's Discord click.
        assert!(!store.is_email_complete("m3").unwrap());
    }

    /// A Gmail stub that fails create_draft the first N times, then succeeds.
    struct FlakyGmail {
        emails: Vec<Email>,
        create_failures_remaining: std::sync::Mutex<u32>,
    }
    #[async_trait]
    impl GmailApi for FlakyGmail {
        async fn fetch_unread(
            &self,
            _e: &str,
            _l: u32,
        ) -> Result<Vec<Email>, crate::gmail::GmailError> {
            Ok(self.emails.clone())
        }
        async fn fetch_with_query(
            &self,
            _e: &str,
            _q: &str,
            _l: u32,
        ) -> Result<Vec<Email>, crate::gmail::GmailError> {
            Ok(self.emails.clone())
        }
        async fn update_draft(
            &self,
            _e: &str,
            _d: &str,
            _t: &str,
            _s: &str,
            _b: &str,
        ) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
        async fn create_draft(
            &self,
            _e: &str,
            _t: &str,
            _s: &str,
            _b: &str,
            _th: Option<&str>,
        ) -> Result<String, crate::gmail::GmailError> {
            let mut lock = self.create_failures_remaining.lock().unwrap();
            if *lock > 0 {
                *lock -= 1;
                return Err(crate::gmail::GmailError::Composio {
                    message: "synthetic transient".into(),
                });
            }
            Ok("draft-abc".into())
        }
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
        async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn errored_reply_recovers_on_retry_tick() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(FlakyGmail {
            emails: vec![Email {
                message_id: "m-retry".into(),
                thread_id: Some("t-retry".into()),
                from: "user@client.com".into(),
                subject: "quick q".into(),
                body: "free Thursday?".into(),
                date: "2026-04-18".into(),
                account_entity_id: Some("acc1".into()),
            }],
            create_failures_remaining: std::sync::Mutex::new(1),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            // first-pass triage + draft
            r#"{"decision":"reply","reason":"actionable"}"#,
            "Thursday 3pm works.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                retry_min_gap: Duration::from_millis(0),
                retry_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );

        // First pass: triage OK, draft OK, create_draft FAILS, action recorded
        // as Error; not marked complete.
        let out1 = ch.poll_once().await.unwrap();
        assert_eq!(out1.awaiting_approval, 0);
        assert_eq!(out1.errors, 1);
        assert!(!store.is_email_complete("m-retry").unwrap());
        assert_eq!(broker.posts.lock().unwrap().len(), 0);

        // Retry tick: create_draft now succeeds, approval card posted.
        let retried = ch.retry_once().await.unwrap();
        assert_eq!(retried, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        // Email still NOT complete (user hasn't clicked Approve yet).
        assert!(!store.is_email_complete("m-retry").unwrap());
    }
}
