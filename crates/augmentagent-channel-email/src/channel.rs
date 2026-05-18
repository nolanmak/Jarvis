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

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use async_trait::async_trait;

use augmentagent_approval_discord::{ApprovalBroker, NoopBroker};
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message, SkillPrompt, TRIAGE_SYSTEM};
use augmentagent_channel_core::trigger::{WorkItem, WorkItemHandler};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, RetryableReply, Store, TriageResult, NUDGE_INTERVAL_MS};

use crate::gmail::{extract_bare_email, GmailApi};

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
                            .process_email(&skill.system, &learned, &account.entity_id, email)
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

    /// Run one email through the full triage → draft → approve → ingest
    /// pipeline. This is the single shared per-email entry point: the bespoke
    /// `poll_once` account loop calls it directly, and `GmailWorkHandler`
    /// (the `ChannelRunner` cutover path) calls it after rehydrating the
    /// `Email` from a `WorkItem` payload. Both therefore exercise byte-identical
    /// dispatch logic — the cutover is a driver swap, not a behavior change.
    pub async fn process_email(
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
        // If a prior dispatch already produced a pending or errored action for
        // this email, leave it alone — the existing Discord card or the retry
        // tick will carry it forward. Without this gate, every poll cycle on
        // an unread email would spawn a fresh draft + approval card.
        if self.store.has_open_action(&email.message_id)? {
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
                let reason = decision.reason.as_deref().unwrap_or("flagged");
                self.store.log_flagged_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    reason,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                println!(
                    "[flag] {} from={} reason={}",
                    email.message_id, email.from, reason
                );
                // Post the heads-up to Discord. Best-effort — failure to reach
                // the broker shouldn't abort the flag flow (wiki ingest still
                // runs below and the email stays marked complete).
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
                // --- 1b. BACKPRESSURE (#99). Before spending an Opus draft
                // call + a Discord card, check the approval backlog. If it's
                // at/over the cap, downgrade Reply -> Flag: the user still
                // gets a heads-up but we skip the expensive draft and don't
                // pile another card onto an already-deep queue. The cap is
                // env-tunable; default 25 mirrors the digest enumeration cap.
                let max_pending: i64 = std::env::var("AUGMENTAGENT_MAX_PENDING_DRAFTS")
                    .ok()
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|n| *n >= 0)
                    .unwrap_or(25);
                let pending_now = self.store.pending_reply_count().unwrap_or(0);
                if pending_now >= max_pending {
                    let base = decision.reason.as_deref().unwrap_or("reply-worthy");
                    let reason = format!(
                        "draft queue full ({pending_now} pending ≥ cap {max_pending}); \
                         downgraded to flag — {base}"
                    );
                    warn!(
                        message_id = %email.message_id,
                        from = %email.from,
                        pending = pending_now,
                        cap = max_pending,
                        "reply downgraded to flag: approval queue at capacity"
                    );
                    self.store.log_flagged_action(
                        &email.message_id,
                        email.thread_id.as_deref(),
                        &email.from,
                        &email.subject,
                        Some(&email.body),
                        &reason,
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                    println!(
                        "[flag:backpressure] {} from={} pending={} cap={}",
                        email.message_id, email.from, pending_now, max_pending
                    );
                    if let Err(e) = self
                        .approvals
                        .post_flag_notice(
                            &email,
                            &format!(
                                "Reply-worthy, but the approval queue is full \
                                 ({pending_now} drafts waiting). Clear some via \
                                 `augmentagent approvals` then this can be re-drafted. \
                                 Context: {base}"
                            ),
                        )
                        .await
                    {
                        warn!(
                            message_id = %email.message_id,
                            "post_flag_notice (backpressure) failed: {e}"
                        );
                    }
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Flag,
                        decision.reason.as_deref(),
                        None,
                        IngestTrigger::Triaged,
                    );
                    return Ok(Some(DispatchOutcome::Flagged));
                }

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
                let tone_block =
                    pick_tone_block(&self.store, entity_id, &email.from);
                let thread_block = self.fetch_thread_block(entity_id, &email).await;
                // #36: fast Haiku archetype pick → composed fragment, gated by
                // AUGMENTAGENT_DRAFT_ARCHETYPES=1 (resolver no-ops otherwise).
                let archetype_block =
                    augmentagent_channel_core::archetype::resolve_archetype_block(
                        self.reasoner.as_ref(),
                        &email,
                        "reply",
                    )
                    .await;
                // #35 Phase 2: pre-resolve structured asks (scheduling /
                // calendly / share_doc / intro) and inject concrete values.
                // Gated by AUGMENTAGENT_ASK_RESOLVE=live + per-resolver flags;
                // empty string (today's behavior) for off/shadow or when no
                // ask clears the confidence floor.
                let resolved_asks_block =
                    augmentagent_channel_core::resolve_asks_block(
                        &self.reasoner,
                        augmentagent_channel_core::AskResolveMode::from_env(),
                        &email.body,
                        self.build_resolve_ctx(entity_id),
                    )
                    .await;
                let draft_prompt = draft_user_message(
                    &email,
                    &wiki_hint,
                    &tone_block,
                    &thread_block,
                    &archetype_block,
                    &resolved_asks_block,
                );
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
            // Capture / Meeting are wave-A wiki-ingest-only kinds emitted by
            // the voice and gcal channels respectively. The email triage
            // model is not allowed to return them; if one shows up it's a
            // model bug, so we log and skip rather than crash.
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "email triage returned non-email decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                Ok(Some(DispatchOutcome::Skipped))
            }
        }
    }

    /// Build the resolver context for #35 Phase 2 ask resolution.
    ///
    /// `entity_id` scopes calendar/Drive lookups to the current account.
    /// Composio-backed free/busy + Drive clients are constructed ONLY when
    /// `COMPOSIO_API_KEY` is present; absent it, `freebusy`/`drive` stay
    /// `None` and the scheduling/share_doc resolvers self-gate to no-ops
    /// (calendly/intro need no network). `resolve_asks_block` itself is gated
    /// on `AUGMENTAGENT_ASK_RESOLVE=live`, so this is never reached otherwise.
    fn build_resolve_ctx(
        &self,
        entity_id: &str,
    ) -> augmentagent_channel_core::ResolveCtx {
        let mut ctx = augmentagent_channel_core::ResolveCtx {
            entity_id: Some(entity_id.to_string()),
            calendar_id: "primary".into(),
            wiki_root: self.config.wiki_root.clone(),
            freebusy: None,
            drive: None,
        };
        if let Ok(key) = std::env::var("COMPOSIO_API_KEY") {
            if !key.trim().is_empty() {
                let client = std::sync::Arc::new(
                    augmentagent_channel_core::ComposioResolveClient::new(key),
                );
                ctx.freebusy = Some(client.clone());
                ctx.drive = Some(client);
            }
        }
        ctx
    }

    /// Build the `<thread_history>` block for thread-aware drafting (#32).
    ///
    /// Gated behind `AUGMENTAGENT_THREAD_AWARE=1` — unset/anything-else returns
    /// an empty string, which `draft_user_message` treats as "no thread block"
    /// (prompt is then byte-identical to pre-#32 behavior). Best-effort: a
    /// fetch failure logs a warning and degrades to the empty block rather
    /// than aborting the draft. The inbound message itself is excluded so the
    /// model doesn't see it twice (it's already in the `<email>` block).
    async fn fetch_thread_block(
        &self,
        entity_id: &str,
        email: &augmentagent_store::Email,
    ) -> String {
        if std::env::var("AUGMENTAGENT_THREAD_AWARE").as_deref() != Ok("1") {
            return String::new();
        }
        let Some(thread_id) = email.thread_id.as_deref() else {
            return String::new();
        };
        // Last ~6 fetched so that after dropping the current inbound we still
        // have ~5 prior turns of verbatim context (the issue's Phase 1 target).
        const MAX_THREAD_MSGS: u32 = 6;
        let msgs = match self
            .gmail
            .fetch_thread_messages(entity_id, thread_id, MAX_THREAD_MSGS)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    message_id = %email.message_id,
                    thread_id = %thread_id,
                    "thread-aware: fetch_thread_messages failed, drafting without history: {e}"
                );
                return String::new();
            }
        };
        let prior: Vec<(String, String, String)> = msgs
            .into_iter()
            .filter(|m| m.message_id != email.message_id)
            .map(|m| (m.from, m.date, m.body))
            .collect();
        if prior.is_empty() {
            return String::new();
        }
        let block =
            augmentagent_channel_core::prompt::format_thread_history(&prior);
        if !block.is_empty() {
            info!(
                message_id = %email.message_id,
                thread_id = %thread_id,
                prior_messages = prior.len(),
                "thread-aware: injecting <thread_history>"
            );
        }
        block
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
        let (action_id, existing_draft_id) = match existing_action_id {
            Some(id) => {
                self.store.update_action_status(
                    &id,
                    ActionStatus::Pending,
                    Some(&initial_draft),
                    None,
                )?;
                // Reuse any Gmail draft from a prior attempt — only the
                // post_approval step can have failed after create_draft set the
                // draftId, so we shouldn't ask Composio for a second draft.
                let prior = self
                    .store
                    .get_action_with_email(&id)?
                    .and_then(|a| a.draft_id);
                (id, prior)
            }
            None => (
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&initial_draft),
                    ActionStatus::Pending,
                )?,
                None,
            ),
        };

        let draft_id = match existing_draft_id {
            Some(d) => d,
            None => match self
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
                Ok(id) => {
                    self.store.set_action_draft_id(&action_id, &id)?;
                    id
                }
                Err(e) => {
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Error,
                        None,
                        Some(&format!("create_draft: {e}")),
                    )?;
                    return Err(e.into());
                }
            },
        };

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

        // Mark this row as the active nudge so the scheduler's serial-queue
        // logic (`find_active_nudge`/`find_next_to_promote`) treats it as
        // "currently being shown" and won't re-post it on the next 60s tick.
        if let Err(e) = self
            .store
            .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
        {
            warn!(action_id, "record_nudge after post_approval failed: {e}");
        }

        // Mark the email processed only after a card is up. Failures earlier
        // leave agentProcessedAt NULL so the retry tick can pick the action
        // up; the has_open_action gate in handle_email keeps the poll loop
        // from spawning a duplicate in the meantime.
        self.store
            .mark_email_processed(&email.message_id, TriageResult::Reply)?;

        info!(action_id, draft_id = %draft_id, "approval card posted");
        Ok(Some(DispatchOutcome::AwaitingApproval))
    }
}

impl<G: GmailApi + 'static, R: Reasoner + 'static> GmailChannel<G, R> {
    /// Long-running driver (the production entry point used by `serve`).
    ///
    /// **#25 cutover**: the poll path no longer hand-rolls a `select!` +
    /// ticker. It is driven by the generic
    /// [`augmentagent_channel_core::ChannelRunner`] over a
    /// [`GmailInbound`](crate::GmailInbound) source wrapped in an
    /// [`InboundMessageTrigger`](augmentagent_channel_core::InboundMessageTrigger),
    /// dispatching each `WorkItem` through [`GmailWorkHandler`] — which calls
    /// the very same [`GmailChannel::process_email`] the old loop did, so
    /// triage/draft/approve/ingest/dedup behavior is unchanged.
    ///
    /// The retry queue is independent of polling (it scans the `actions`
    /// table, not the inbox) so it stays as a sibling ticker here rather than
    /// being folded into `ChannelRunner`.
    ///
    /// Takes `Arc<Self>` so the runner's handler can share the channel.
    pub async fn run_arc(
        self: Arc<Self>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        use augmentagent_channel_core::{ChannelRunner, InboundMessageTrigger};

        let source = Arc::new(crate::inbound::GmailInbound::new(
            Arc::clone(&self.store),
            Arc::clone(&self.gmail),
            self.config.per_account_limit,
        ));
        let trigger = Arc::new(InboundMessageTrigger::new(source));
        let handler = Arc::new(GmailWorkHandler::new(Arc::clone(&self)));
        let runner = Arc::new(ChannelRunner::new(
            trigger,
            handler,
            self.config.poll_interval,
            // Gmail's old loop had no post-poll jitter (only LinkedIn did).
            Duration::ZERO,
            "gmail",
        ));

        let retry_enabled = !self.config.retry_interval.is_zero();
        let retry_interval = if retry_enabled {
            self.config.retry_interval
        } else {
            Duration::from_secs(3600)
        };

        let runner_sd = shutdown.clone();
        let poll = tokio::spawn(async move { runner.run(runner_sd).await });

        let retry_self = Arc::clone(&self);
        let retry = tokio::spawn(async move {
            let mut retry_ticker = tokio::time::interval(retry_interval);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!("gmail channel: retry loop shutdown signal received");
                        return;
                    }
                    _ = retry_ticker.tick(), if retry_enabled => {
                        match retry_self.retry_once().await {
                            Ok(n) if n > 0 => info!(retried = n, "retry tick complete"),
                            Ok(_) => {}
                            Err(e) => error!("retry tick failed: {e:#}"),
                        }
                    }
                }
            }
        });

        let _ = poll.await;
        let _ = retry.await;
        Ok(())
    }
}

/// `WorkItemHandler` for the #25 `ChannelRunner` cutover.
///
/// The runner pulls unread mail as `WorkItem`s (via `GmailInbound`); this
/// handler rehydrates each into the typed `Email` and feeds it through the
/// channel's shared [`GmailChannel::process_email`] — i.e. the *identical*
/// triage → draft → approve → ingest → dedup path the bespoke `poll_once`
/// account loop runs. The only intentional delta vs the old loop: the
/// triage skill prompt is loaded once at handler construction (daemon
/// start) instead of re-read every poll cycle. The on-disk skill is static
/// for a daemon's lifetime, so dispatch decisions are unchanged; this just
/// drops a redundant per-tick file read.
pub struct GmailWorkHandler<G: GmailApi, R: Reasoner + 'static> {
    channel: Arc<GmailChannel<G, R>>,
    /// `SKILL.md` system text, loaded once (mirrors `poll_once`'s `skill.system`).
    draft_skill: String,
    /// Learned-patterns text, loaded once (mirrors `poll_once`'s `learned`).
    learned: String,
}

impl<G: GmailApi + 'static, R: Reasoner + 'static> GmailWorkHandler<G, R> {
    pub fn new(channel: Arc<GmailChannel<G, R>>) -> Self {
        let skill = SkillPrompt::load(&channel.config.skill_dir);
        let learned = skill.load_learned();
        Self {
            channel,
            draft_skill: skill.system,
            learned,
        }
    }
}

#[async_trait]
impl<G: GmailApi + 'static, R: Reasoner + 'static> WorkItemHandler
    for GmailWorkHandler<G, R>
{
    async fn handle(&self, item: WorkItem) -> anyhow::Result<()> {
        let email: augmentagent_store::Email =
            serde_json::from_value(item.payload).map_err(|e| {
                anyhow::anyhow!("gmail work item payload not an Email: {e}")
            })?;
        // `poll_once` keys dispatch off the polled account's entity_id; the
        // serialized Email already carries it (`GmailInbound` fans out over
        // active accounts and stamps each). Fall back to the work-item's
        // empty string only if absent — `process_email`'s thread-fetch /
        // dispatch tolerate it the same way the loop would for a NULL.
        let entity_id = email.account_entity_id.clone().unwrap_or_default();
        // Mirror poll_once's per-email error handling: log + swallow so one
        // bad message never aborts the tick (ChannelRunner also logs+counts).
        match self
            .channel
            .process_email(&self.draft_skill, &self.learned, &entity_id, email)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("gmail handle (channel-runner): process_email failed: {e:#}");
                Ok(())
            }
        }
    }
}

/// Resolve the tone block for `to_addr` against the user's `account` profiles.
///
/// Lookup order (per #73 §6 bootstrap thresholds):
///   1. per-recipient profile (use when `sample_count >= 3`)
///   2. per-domain profile    (use when `sample_count >= 5`)
///   3. global profile        (always, if it exists)
///
/// Each tier is also rejected when its summary marks `register` as
/// `insufficient_sample` (the summarizer's signal that the corpus was too
/// thin) — we fall through to the parent scope rather than inject a
/// degraded descriptor. Returns `String::new()` when nothing usable exists,
/// which `draft_user_message` interprets as "no tone injection".
pub(crate) fn pick_tone_block(store: &Store, account: &str, to_addr: &str) -> String {
    let bare = extract_bare_email(to_addr).to_ascii_lowercase();
    let domain = bare
        .split_once('@')
        .map(|(_, d)| d.to_string())
        .unwrap_or_default();

    if let Ok(Some(p)) = store.get_tone_profile("recipient", &bare, Some(account)) {
        if p.sample_count >= 3 && !is_insufficient_sample(&p.summary) {
            return p.summary;
        }
    }
    if !domain.is_empty() {
        if let Ok(Some(p)) = store.get_tone_profile("domain", &domain, Some(account)) {
            if p.sample_count >= 5 && !is_insufficient_sample(&p.summary) {
                return p.summary;
            }
        }
    }
    if let Ok(Some(p)) = store.get_tone_profile("global", "*", Some(account)) {
        if !is_insufficient_sample(&p.summary) {
            return p.summary;
        }
    }
    String::new()
}

/// True when the summarizer JSON signaled that it didn't have enough samples
/// to produce a useful descriptor. We do a substring check on the raw text
/// rather than parse the JSON because any parser hiccup should not silently
/// promote a degraded descriptor into a draft prompt.
fn is_insufficient_sample(summary: &str) -> bool {
    summary.contains("\"insufficient_sample\"")
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

/// Outcome of running one email through [`GmailChannel::process_email`].
/// Public because `process_email` is the shared entry point used by both the
/// bespoke `poll_once` loop and the `ChannelRunner` cutover handler.
#[derive(Debug, Clone, Copy)]
pub enum DispatchOutcome {
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
    use augmentagent_channel_core::ReasonerOpts;
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
                platform: "gmail".into(),
                kind: "dm".into(),
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
                platform: "gmail".into(),
                kind: "dm".into(),
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
                platform: "gmail".into(),
                kind: "dm".into(),
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
                platform: "gmail".into(),
                kind: "dm".into(),
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
                platform: "gmail".into(),
                kind: "dm".into(),
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
        // Email IS complete: dispatching a Reply marks the email processed so
        // the next poll cycle won't spawn a duplicate draft + card while the
        // user is deciding. Final outcome (sent/rejected/timed_out) flows
        // through the action's status, not the email's gate.
        assert!(store.is_email_complete("m3").unwrap());

        // Re-polling the same unread email must not spawn a second action.
        let out2 = ch.poll_once().await.unwrap();
        assert_eq!(out2.awaiting_approval, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
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
                platform: "gmail".into(),
                kind: "dm".into(),
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
        // as Error; not marked complete (so the retry tick can pick it up).
        let out1 = ch.poll_once().await.unwrap();
        assert_eq!(out1.awaiting_approval, 0);
        assert_eq!(out1.errors, 1);
        assert!(!store.is_email_complete("m-retry").unwrap());
        assert_eq!(broker.posts.lock().unwrap().len(), 0);

        // Retry tick: create_draft now succeeds, approval card posted.
        let retried = ch.retry_once().await.unwrap();
        assert_eq!(retried, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        // Successful dispatch on the retry path marks the email processed.
        assert!(store.is_email_complete("m-retry").unwrap());
    }

    /// Counts create_draft invocations so the retry-no-double-draft test can
    /// assert Gmail isn't asked for a second draft when the prior attempt
    /// already succeeded at create_draft and only failed at post_approval.
    struct CountingGmail {
        emails: Vec<Email>,
        create_calls: std::sync::atomic::AtomicU32,
    }
    #[async_trait]
    impl GmailApi for CountingGmail {
        async fn fetch_unread(&self, _e: &str, _l: u32) -> Result<Vec<Email>, crate::gmail::GmailError> {
            Ok(self.emails.clone())
        }
        async fn fetch_with_query(&self, _e: &str, _q: &str, _l: u32) -> Result<Vec<Email>, crate::gmail::GmailError> {
            Ok(self.emails.clone())
        }
        async fn update_draft(&self, _e: &str, _d: &str, _t: &str, _s: &str, _b: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
        async fn create_draft(&self, _e: &str, _t: &str, _s: &str, _b: &str, _th: Option<&str>) -> Result<String, crate::gmail::GmailError> {
            self.create_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("draft-once".into())
        }
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
        async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
    }

    /// Broker that fails post_approval the first N times, then succeeds. Used
    /// to simulate Discord transient outages.
    struct FlakyBroker {
        posts: std::sync::Mutex<Vec<String>>,
        fail_remaining: std::sync::Mutex<u32>,
    }
    #[async_trait]
    impl ApprovalBroker for FlakyBroker {
        async fn post_approval(
            &self,
            action_id: &str,
            _email: &Email,
            _draft: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            let mut lock = self.fail_remaining.lock().unwrap();
            if *lock > 0 {
                *lock -= 1;
                return Err(augmentagent_approval_discord::ApprovalError::Discord(
                    "synthetic broker outage".into(),
                ));
            }
            self.posts.lock().unwrap().push(action_id.to_string());
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _email: &Email,
            _reason: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            Ok(())
        }
    }

    /// When create_draft already succeeded on a prior attempt and only
    /// post_approval failed, the retry tick must NOT call create_draft again
    /// (which would orphan a duplicate Gmail draft).
    #[tokio::test]
    async fn retry_after_post_approval_failure_reuses_existing_draft() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(CountingGmail {
            emails: vec![Email {
                message_id: "m-broker-fail".into(),
                thread_id: Some("t1".into()),
                from: "user@client.com".into(),
                subject: "ping".into(),
                body: "free Friday?".into(),
                date: "2026-04-20".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
            create_calls: std::sync::atomic::AtomicU32::new(0),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            "Friday 2pm works.",
        ]));
        let broker = Arc::new(FlakyBroker {
            posts: std::sync::Mutex::new(Vec::new()),
            fail_remaining: std::sync::Mutex::new(1),
        });
        let ch = GmailChannel::new(
            store.clone(),
            gmail.clone(),
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                retry_min_gap: Duration::from_millis(0),
                ..Default::default()
            },
        );

        // First pass: triage + draft + create_draft OK, post_approval FAILS.
        let out1 = ch.poll_once().await.unwrap();
        assert_eq!(out1.errors, 1);
        assert_eq!(gmail.create_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);

        // Retry tick: post_approval succeeds. create_draft must NOT be called
        // a second time — the action already has a draftId from the first pass.
        let retried = ch.retry_once().await.unwrap();
        assert_eq!(retried, 1);
        assert_eq!(gmail.create_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(store.is_email_complete("m-broker-fail").unwrap());
    }

    // --- tone-mirror lookup (#73 §4) ---

    #[test]
    fn pick_tone_block_returns_empty_when_no_profiles() {
        let (store, _f) = tmp_store();
        let out = super::pick_tone_block(&store, "acc1", "alex@startup.io");
        assert!(out.is_empty());
    }

    #[test]
    fn pick_tone_block_prefers_recipient_above_threshold() {
        let (store, _f) = tmp_store();
        store
            .upsert_tone_profile("global", "*", Some("acc1"), "GLOBAL", "[]", 50)
            .unwrap();
        store
            .upsert_tone_profile(
                "domain",
                "startup.io",
                Some("acc1"),
                "DOMAIN",
                "[]",
                10,
            )
            .unwrap();
        store
            .upsert_tone_profile(
                "recipient",
                "alex@startup.io",
                Some("acc1"),
                "RECIPIENT",
                "[]",
                4,
            )
            .unwrap();
        let out = super::pick_tone_block(&store, "acc1", "Alex <alex@startup.io>");
        assert_eq!(out, "RECIPIENT");
    }

    #[test]
    fn pick_tone_block_falls_through_when_recipient_under_threshold() {
        let (store, _f) = tmp_store();
        store
            .upsert_tone_profile("global", "*", Some("acc1"), "GLOBAL", "[]", 50)
            .unwrap();
        store
            .upsert_tone_profile(
                "domain",
                "startup.io",
                Some("acc1"),
                "DOMAIN",
                "[]",
                10,
            )
            .unwrap();
        // sample_count=2 → below the 3-message recipient threshold.
        store
            .upsert_tone_profile(
                "recipient",
                "alex@startup.io",
                Some("acc1"),
                "RECIPIENT",
                "[]",
                2,
            )
            .unwrap();
        let out = super::pick_tone_block(&store, "acc1", "alex@startup.io");
        assert_eq!(out, "DOMAIN");
    }

    #[test]
    fn pick_tone_block_falls_through_to_global_when_domain_thin() {
        let (store, _f) = tmp_store();
        store
            .upsert_tone_profile("global", "*", Some("acc1"), "GLOBAL", "[]", 50)
            .unwrap();
        // sample_count=3 → below the 5-message domain threshold.
        store
            .upsert_tone_profile(
                "domain",
                "startup.io",
                Some("acc1"),
                "DOMAIN",
                "[]",
                3,
            )
            .unwrap();
        let out = super::pick_tone_block(&store, "acc1", "alex@startup.io");
        assert_eq!(out, "GLOBAL");
    }

    #[test]
    fn pick_tone_block_skips_insufficient_sample_markers() {
        let (store, _f) = tmp_store();
        store
            .upsert_tone_profile("global", "*", Some("acc1"), "GLOBAL", "[]", 50)
            .unwrap();
        // Per-recipient row exists with enough samples but the summarizer
        // came back with "insufficient_sample" — we must fall through.
        store
            .upsert_tone_profile(
                "recipient",
                "alex@startup.io",
                Some("acc1"),
                "{\"register\":\"insufficient_sample\"}",
                "[]",
                4,
            )
            .unwrap();
        let out = super::pick_tone_block(&store, "acc1", "alex@startup.io");
        assert_eq!(out, "GLOBAL");
    }

    // --- #25 ChannelRunner cutover equivalence ---
    //
    // These prove the production driver swap (poll loop → ChannelRunner +
    // GmailWorkHandler) reproduces poll_once's per-email behavior exactly:
    // same triage decisions, same broker posts, same email-complete dedup.

    use crate::inbound::email_to_work_item;

    #[tokio::test]
    async fn channel_runner_handler_reply_flow_matches_poll_once() {
        let (store, _f) = tmp_store();
        let email = Email {
            message_id: "cr-reply".into(),
            thread_id: Some("t-cr".into()),
            from: "user@client.com".into(),
            subject: "Ping".into(),
            body: "any update?".into(),
            date: "2026-05-18".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        let gmail = Arc::new(StubGmail { emails: vec![email.clone()] });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"ping"}"#,
            "Yes — shipping today.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = Arc::new(GmailChannel::new(
            store.clone(),
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        ));
        // Drive the cutover handler exactly as ChannelRunner would: one
        // WorkItem rehydrated from the GmailInbound serialization.
        let handler = super::GmailWorkHandler::new(Arc::clone(&ch));
        handler
            .handle(email_to_work_item(&email))
            .await
            .unwrap();

        // Identical observable state to live_reply_flow_posts_approval_card.
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(store.is_email_complete("cr-reply").unwrap());

        // Re-handling the same unread email (next runner tick) must NOT
        // spawn a second card — the email-complete gate holds.
        handler
            .handle(email_to_work_item(&email))
            .await
            .unwrap();
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn channel_runner_handler_skip_and_flag_match_poll_once() {
        let (store, _f) = tmp_store();
        let skip_email = Email {
            message_id: "cr-skip".into(),
            thread_id: None,
            from: "noreply@marketing.com".into(),
            subject: "50% off!".into(),
            body: "deal deal".into(),
            date: "2026-05-18".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        let flag_email = Email {
            message_id: "cr-flag".into(),
            thread_id: None,
            from: "friend@edu.com".into(),
            subject: "Catching up".into(),
            body: "wanted to reach out".into(),
            date: "2026-05-18".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        let gmail = Arc::new(StubGmail { emails: vec![] });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"marketing"}"#,
            r#"{"decision":"flag","reason":"personal outreach"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = Arc::new(GmailChannel::new(
            store.clone(),
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        ));
        let handler = super::GmailWorkHandler::new(Arc::clone(&ch));
        handler.handle(email_to_work_item(&skip_email)).await.unwrap();
        handler.handle(email_to_work_item(&flag_email)).await.unwrap();

        // Skip: no posts at all; email complete.
        assert!(store.is_email_complete("cr-skip").unwrap());
        // Flag: heads-up notice, no approval card; email complete.
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        let flags = broker.flag_posts.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, "cr-flag");
        assert!(store.is_email_complete("cr-flag").unwrap());
    }

    #[tokio::test]
    async fn channel_runner_handler_swallows_bad_payload() {
        // ChannelRunner counts a handler error as handled-and-logged; the
        // Gmail handler additionally swallows process_email errors so one
        // bad message never aborts a tick — same as poll_once's per-email
        // error arm. A non-Email payload must therefore be a benign no-op.
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail { emails: vec![] });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = Arc::new(GmailChannel::new(
            store,
            gmail,
            reasoner,
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        ));
        let handler = super::GmailWorkHandler::new(Arc::clone(&ch));
        let junk = augmentagent_channel_core::trigger::WorkItem {
            platform: "gmail".into(),
            kind: "dm".into(),
            external_id: "junk".into(),
            payload: serde_json::json!({ "not": "an email" }),
        };
        // Payload-decode failures bubble as Err (ChannelRunner logs+counts);
        // process_email failures are swallowed. Either way: no panic, no post.
        let _ = handler.handle(junk).await;
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
    }
}
