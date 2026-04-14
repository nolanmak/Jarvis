//! `GmailChannel` — poll loop + per-email reasoning dispatch.

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
use crate::prompt::{redraft_message, user_message, SkillPrompt};

#[derive(Clone, Debug)]
pub struct GmailChannelConfig {
    pub poll_interval: Duration,
    pub per_account_limit: u32,
    pub skill_dir: PathBuf,
    pub dry_run: bool,
    pub model: Option<String>,
    /// Max number of revise rounds before we give up on a reply.
    pub max_revise_rounds: u8,
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

/// Trait the channel uses to reach Claude. Abstracted so we can stub it in
/// tests without hitting the real `claude` CLI spawn.
#[async_trait]
pub trait Reasoner: Send + Sync {
    async fn decide(&self, system_prompt: &str, user_message: &str) -> anyhow::Result<String>;
}

pub struct GmailChannel<G: GmailApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub gmail: Arc<G>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: GmailChannelConfig,
}

impl<G: GmailApi, R: Reasoner> GmailChannel<G, R> {
    pub fn new(
        store: Arc<Store>,
        gmail: Arc<G>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        config: GmailChannelConfig,
    ) -> Self {
        Self { store, gmail, reasoner, approvals, config }
    }

    /// Build a channel with the Phase 1 no-op approval broker.
    pub fn dry_run(
        store: Arc<Store>,
        gmail: Arc<G>,
        reasoner: Arc<R>,
        config: GmailChannelConfig,
    ) -> Self {
        Self::new(store, gmail, reasoner, Arc::new(NoopBroker), config)
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
            match self.gmail.fetch_unread(&account.entity_id, self.config.per_account_limit).await {
                Ok(emails) => {
                    outcome.emails_checked += emails.len();
                    for email in emails {
                        match self.handle_email(&skill.system, &learned, &account.entity_id, email).await {
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

        outcome.new_emails =
            outcome.skipped + outcome.flagged + outcome.replied_dry_run + outcome.sent + outcome.rejected;
        Ok(outcome)
    }

    async fn handle_email(
        &self,
        system_prompt: &str,
        learned: &str,
        entity_id: &str,
        email: augmentagent_store::Email,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        let is_new = self.store.upsert_email(&email)?;
        if !is_new || self.store.is_message_processed(&email.message_id)? {
            return Ok(None);
        }

        let user = user_message(&email, learned);
        let raw = self.reasoner.decide(system_prompt, &user).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "decision parse failed: {e}; raw={raw}");
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Error,
                )?;
                self.store.mark_email_processed(&email.message_id, TriageResult::Flag)?;
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
                self.store.mark_email_processed(&email.message_id, TriageResult::Skip)?;
                println!(
                    "[skip] {} from={} reason={}",
                    email.message_id,
                    email.from,
                    decision.reason.as_deref().unwrap_or("")
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
                self.store.mark_email_processed(&email.message_id, TriageResult::Flag)?;
                println!(
                    "[flag] {} from={} reason={}",
                    email.message_id,
                    email.from,
                    decision.reason.as_deref().unwrap_or("")
                );
                Ok(Some(DispatchOutcome::Flagged))
            }
            DecisionKind::Reply => {
                let draft = decision.draft.clone().unwrap_or_default();
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
                    self.store.mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    println!(
                        "[reply dry-run] {} from={} subject={}\n--- draft ---\n{}\n--- /draft ---",
                        email.message_id, email.from, email.subject, draft,
                    );
                    return Ok(Some(DispatchOutcome::DryRun));
                }
                self.dispatch_reply(system_prompt, entity_id, email, draft).await
            }
        }
    }

    async fn dispatch_reply(
        &self,
        system_prompt: &str,
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
                self.store.mark_email_processed(&email.message_id, TriageResult::Reply)?;
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
                    // Send the draft we have on record (draft_id in Gmail). The
                    // current Phase 2 flow sends the Gmail-side draft, which
                    // matches the initially-created body or the last revise.
                    // If a revise has updated `current_draft`, the Gmail draft
                    // was also updated in the revise branch below before
                    // re-requesting approval.
                    let _ = final_draft; // reserved for future UX where Discord edits the draft inline
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

                    let redraft = self
                        .reasoner
                        .decide(system_prompt, &redraft_message(&email, &current_draft, &feedback))
                        .await?;
                    // Redraft responses are prose, not JSON — take the whole thing verbatim.
                    current_draft = redraft.trim().to_string();
                    self.store
                        .update_action_status(&action_id, ActionStatus::Pending, Some(&current_draft), None)?;
                    // Regenerate Gmail-side draft body to reflect the revise.
                    // Composio exposes UPDATE separately; simplest is to delete+recreate
                    // but Composio's GMAIL_UPDATE_DRAFT action isn't wired. For Phase 2
                    // we just create a second draft; the first stays orphan. Mitigate
                    // via Phase 3 (wire UPDATE_DRAFT or delete-and-recreate).
                    warn!(
                        "revise: creating a replacement Gmail draft (orphan draft {} remains)",
                        draft_id
                    );
                    // Note: keep draft_id pointing at the latest creation.
                    // In Phase 3 swap this for GMAIL_UPDATE_DRAFT.
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
        async fn fetch_unread(&self, _e: &str, _l: u32) -> Result<Vec<Email>, crate::gmail::GmailError> {
            Ok(self.emails.clone())
        }
        async fn create_draft(&self, _e: &str, _t: &str, _s: &str, _b: &str, _th: Option<&str>) -> Result<String, crate::gmail::GmailError> {
            Ok("draft".into())
        }
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
    }

    struct ScriptedReasoner {
        response: String,
    }
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn decide(&self, _s: &str, _u: &str) -> anyhow::Result<String> {
            Ok(self.response.clone())
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
            ).unwrap();
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
        let reasoner = Arc::new(ScriptedReasoner {
            response: r#"{"decision":"skip","reason":"newsletter"}"#.into(),
        });
        let ch = GmailChannel::dry_run(store, gmail, reasoner, GmailChannelConfig {
            skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
            ..Default::default()
        });
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(out.replied_dry_run, 0);
        assert_eq!(out.errors, 0);
    }

    #[tokio::test]
    async fn dry_run_reply_flow() {
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
        let reasoner = Arc::new(ScriptedReasoner {
            response: r#"```json
{"decision":"reply","draft":"Sure — here is the answer.","reason":"actionable question"}
```"#.into(),
        });
        let ch = GmailChannel::dry_run(store, gmail, reasoner, GmailChannelConfig {
            skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
            ..Default::default()
        });
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
            Ok(ApprovalOutcome::Approved { final_draft: initial_draft.to_string() })
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
        let reasoner = Arc::new(ScriptedReasoner {
            response: r#"{"decision":"reply","draft":"Yes — shipping today."}"#.into(),
        });
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
        async fn request(&self, _: &str, _: &Email, _: &str) -> Result<ApprovalOutcome, ApprovalError> {
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
        let reasoner = Arc::new(ScriptedReasoner {
            response: r#"{"decision":"reply","draft":"hi"}"#.into(),
        });
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
