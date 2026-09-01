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
use augmentagent_channel_core::code_mode::{
    self, handle_code_mode_failure, manifest_v1, report_classic_fallback, DefaultDispatcher,
    DraftOutcome, FailureCtx, FailureStage, GhCliIssueRunner, GhIssueRunner, MessageContext,
};
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{
    code_mode_system, code_mode_user_message, draft_user_message, triage_user_message, SkillPrompt,
    TRIAGE_SYSTEM,
};
use augmentagent_channel_core::trigger::{WorkItem, WorkItemHandler};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, RetryableReply, Store, TriageResult, NUDGE_INTERVAL_MS};

use crate::gmail::{extract_bare_email, GmailApi};
use crate::outbound::parse_rfc2822_or_ms;
use crate::sigextract::{is_event_blast, is_human_sender, is_meeting_invite};

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
    /// #222: emails classified as event-platform / signup-confirmation
    /// blasts — wiki ingest ran, but no draft and no Discord notice.
    pub ingest_only: usize,
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
    /// gh CLI runner for I7 postmortems. Behind a trait so tests can mock
    /// the `gh issue create` invocation; production defaults to
    /// [`GhCliIssueRunner`] which shells out to the `gh` binary on PATH.
    gh_issue_runner: Arc<dyn GhIssueRunner>,
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
            gh_issue_runner: Arc::new(GhCliIssueRunner::new()),
        }
    }

    /// Swap the gh-CLI runner used for I7 postmortem issues. Production
    /// callers don't need this; tests pass a recording stub so the suite
    /// never actually files issues on the real repo.
    pub fn with_gh_issue_runner(mut self, runner: Arc<dyn GhIssueRunner>) -> Self {
        self.gh_issue_runner = runner;
        self
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
            crate::inbound::PLATFORM,
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
        // #451 — a retry whose action never got a draft re-runs the full
        // pipeline (triage included), so it needs the same prompts poll_once
        // uses, not just a reachability check on the skill dir.
        let learned = skill.load_learned();

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
            let new_count = self
                .store
                .increment_retry_count(&item.action.id, self.config.retry_max_attempts)?;
            info!(
                action_id = %item.action.id,
                attempt = new_count,
                max = self.config.retry_max_attempts,
                subject = %item.action.subject,
                "retrying reply"
            );

            let draft = item.action.draft_body.clone().unwrap_or_default();

            // #451 — an errored action with NO draft body failed *before* a
            // draft ever existed: triage threw (a Claude session-limit or a
            // malformed-JSON reply is the common case) and logged an Error row
            // with `draft = None`. Handing that to `dispatch_reply` — which is
            // what this used to do unconditionally — is wrong twice over:
            //
            //   1. It publishes an approval card whose draft is the empty
            //      string, so the user gets a card with nothing in it.
            //   2. `dispatch_reply` starts AFTER triage, so it skips the
            //      triage guards: automated-sender (#217), already-replied
            //      (#218), event-blast (#222). Newsletters that triage would
            //      never have drafted sail straight into the queue.
            //      (Meeting-invite (#834) is the exception: it also runs as
            //      a backstop inside `dispatch_reply`, so it holds on the
            //      has-a-draft retry path below too.)
            //
            // That is how the live queue reached 102 empty-draft cards from
            // Canva/Marshalls/BetaList — and, via the old backpressure cap,
            // how real human threads stopped getting drafted at all (#450).
            //
            // The Error row's own comment says the retry tick should re-triage.
            // So actually re-triage: retire the errored row (clearing the
            // `has_open_action` gate) and re-run the full pipeline, guards
            // included. `increment_retry_count` above already bounded the
            // attempts, so this cannot spin.
            if draft.trim().is_empty() {
                info!(
                    action_id = %item.action.id,
                    from = %item.action.from_email,
                    "retry: errored action has no draft; re-running triage \
                     instead of dispatching an empty draft"
                );
                if let Err(e) = self.store.update_action_status(
                    &item.action.id,
                    ActionStatus::Superseded,
                    None,
                    Some("retried: re-triaged from scratch (no draft on errored action)"),
                ) {
                    warn!(
                        action_id = %item.action.id,
                        "retry: could not retire errored action, skipping: {e:#}"
                    );
                    continue;
                }
                if let Err(e) = self
                    .process_email(&skill.system, &learned, &entity_id, item.email.clone())
                    .await
                {
                    warn!(action_id = %item.action.id, "retry re-triage failed: {e:#}");
                }
                attempted += 1;
                continue;
            }

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
                                Some(DispatchOutcome::IngestOnly) => outcome.ingest_only += 1,
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
            + outcome.awaiting_approval
            + outcome.ingest_only;
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
                // Do NOT log an action row here, and do NOT mark_email_processed.
                // A triage-stage failure (typically a transient model 529 that
                // surfaces as non-JSON text) has run NONE of the reply gates
                // (is_human_sender / is_event_blast / already-replied /
                // backpressure) and produced no draft, so it must be RE-TRIAGED
                // from scratch — never retried as a reply.
                //
                // Logging a `status='error'` row here is what made the #217
                // automated-sender filter leak: the row (1) tripped
                // `has_open_action`, permanently blocking the poll loop from
                // re-running this pipeline (the only place the gates live), and
                // (2) was scooped by `list_retryable_replies` into `retry_once`
                // -> `dispatch_reply`, which force-drafts an empty body and
                // skips every gate — surfacing newsletters/marketing as approval
                // cards. Returning Err with NO action row leaves
                // agentProcessedAt NULL and the message unread, so the next poll
                // cycle re-runs full triage — identical to how a network Err
                // from `reasoner.call` (the `?` above) already behaves.
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
                // --- 1a. AUTOMATED-SENDER GUARD (#217). The triage model
                // occasionally returns `reply` for mail from `noreply@…`,
                // `notifications@…`, ESP/bulk-sender domains, or messages
                // carrying List-Unsubscribe / "do not reply" markers. The
                // `is_human_sender` helper (see `sigextract.rs`) is the
                // canonical filter already used for wiki backfill (#120);
                // it was previously dead code on the live triage path, so
                // the bot was drafting replies to GitHub, Partiful,
                // marketing blasts, etc. Gate the Reply arm on it: when
                // the sender is non-human we route to Skip directly —
                // mirroring the existing `DecisionKind::Skip` branch
                // above — log `[skip:automated]`, and intentionally NOT
                // post a Discord flag notice (these are high-volume; we
                // don't want to noisy-page the user). Wiki ingest still
                // runs so the sender's signature/page state stays
                // consistent with the rest of the Skip path.
                if !is_human_sender(&email.from, &email.body) {
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
                        "[skip:automated] {} from={} reason=non_human_sender",
                        email.message_id, email.from,
                    );
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Skip,
                        Some("non_human_sender"),
                        None,
                        IngestTrigger::Triaged,
                    );
                    return Ok(Some(DispatchOutcome::Skipped));
                }

                // --- 1a-bis. ALREADY-REPLIED GUARD (#218). The OutboundObserver
                // (#219/#225) logs every classified SENT message into
                // `outbound_thread_log` keyed by thread. If the user has
                // already sent a reply on this thread AFTER this inbound
                // arrived — typically via the Gmail web UI or any other
                // client — there's nothing for us to draft: surfacing a
                // card now would either duplicate what the user already
                // wrote, or worse, propose a stale take. Bail with a
                // silent skip (NO `post_flag_notice` — these are
                // not noise, just no-ops). Wiki ingest still runs so the
                // sender's page stays consistent with the rest of the Skip
                // path. NOTE: when `parse_rfc2822_or_ms` returns 0
                // (unparseable date), we conservatively use `i64::MAX` as
                // the cutoff — meaning ZERO outbound rows will "look
                // newer" — so a date parse failure never causes us to
                // over-skip. The store call is a single covered-index
                // point-lookup (idx_outbound_thread_log_thread_sent), so
                // adding this check is effectively free per-email. Only
                // ships the in-process layer this PR; a live Gmail
                // thread-fetch fallback (for users whose outbound observer
                // hasn't run yet) is deferred to a follow-on. Sequenced
                // before #222's event-blast guard so an already-handled
                // event thread doesn't get re-routed through ingest-only.
                if let Some(thread_id) = email.thread_id.as_deref() {
                    let parsed = parse_rfc2822_or_ms(&email.date);
                    let after_ms = if parsed > 0 { parsed } else { i64::MAX };
                    match self.store.thread_has_user_reply_after(thread_id, after_ms) {
                        Ok(true) => {
                            self.store.log_action(
                                &email.message_id,
                                Some(thread_id),
                                &email.from,
                                &email.subject,
                                Some(&email.body),
                                None,
                                ActionStatus::Skipped,
                            )?;
                            self.store.mark_email_processed(
                                &email.message_id,
                                TriageResult::Skip,
                            )?;
                            println!(
                                "[skip:already-replied] {} thread={} from={}",
                                email.message_id, thread_id, email.from,
                            );
                            self.maybe_ingest(
                                &email,
                                DecisionKind::Skip,
                                Some("user_already_replied"),
                                None,
                                IngestTrigger::Triaged,
                            );
                            return Ok(Some(DispatchOutcome::Skipped));
                        }
                        Ok(false) => {} // fall through to event-blast / backpressure
                        Err(e) => {
                            // Defensive: a query failure here must NOT
                            // block drafting — fall through so the user
                            // still gets the card. Log loudly so the
                            // failure is visible.
                            warn!(
                                message_id = %email.message_id,
                                thread = %thread_id,
                                "thread_has_user_reply_after failed: {e:#}"
                            );
                        }
                    }
                }

                // --- 1a'. EVENT-BLAST INGEST-ONLY GUARD (#222). After the
                // automated-sender skip (#217 above) but before the
                // backpressure check (so event blasts don't burn the
                // ingest-only routing budget against the draft-queue cap).
                // The user signed up via a script for many NYC tech
                // events — Partiful, Luma, Meetup, Eventbrite, Covent,
                // Hopin, Zoom, plus generic `noreply@calendar.*` invites
                // and "See you Wed" / "Registration Confirmed" / "RSVP"
                // subjects — and explicitly doesn't want a draft, an
                // approval card, OR a Discord flag notice for any of
                // them. The wiki ingest still runs so the CRM keeps
                // learning about organizers, events, and attendees
                // (groundwork for the v2 events ledger and "who's at NY
                // Tech Week next week" queries). See `sigextract.rs::
                // is_event_blast` for the curated domain/subject/body
                // detection rules.
                if is_event_blast(&email.from, &email.subject, &email.body) {
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
                        .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                    println!(
                        "[ingest-only:event-blast] {} from={}",
                        email.message_id, email.from,
                    );
                    // Wiki ingest still runs — CRM learns about the event,
                    // organizer, location, attendees. Intentionally NO
                    // `post_flag_notice` (no Discord noise — user-stated
                    // preference).
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Flag,
                        Some("event-blast (ingest-only, no draft)"),
                        None,
                        IngestTrigger::Triaged,
                    );
                    return Ok(Some(DispatchOutcome::IngestOnly));
                }

                // --- 1a''. MEETING-INVITE GUARD (#834). Triage drafted a
                // "happy to attend, could you share an agenda?" reply to a
                // forwarded Microsoft Teams invite: the sender was human
                // (so #217 passed) and the mail looked nothing like an
                // event-platform blast (so #222 missed it). We never draft
                // responses to meeting invites — the user's calendar, not
                // the approval queue, is where these belong. Unlike the
                // event-blast route above these are low-volume and
                // personal, so we take the Flag path (Discord heads-up +
                // wiki ingest, no draft) rather than going silent. Ordered
                // after the event-blast gate so an event-platform invite
                // keeps its quieter ingest-only route. Catching it HERE,
                // rather than only at the `dispatch_reply` backstop below,
                // is what saves the draft-model call. See
                // `sigextract.rs::is_meeting_invite` for the detection
                // rules.
                if is_meeting_invite(&email.subject, &email.body, &email.attachments) {
                    return Ok(Some(self.suppress_meeting_invite(&email, None).await?));
                }

                // --- 1b. BACKPRESSURE (#99) — REMOVED in #450.
                //
                // This used to downgrade Reply -> Flag (no draft, no card)
                // whenever `pending_reply_count() >= AUGMENTAGENT_MAX_PENDING_DRAFTS`
                // (default 25). It was meant as a cost guard, but it degraded
                // into a silent kill switch: the queue could not drain on its
                // own (the outbound observer that retires answered cards was
                // off — #449) and it filled with drafts for newsletters that
                // should never have been drafted (#451). Once it crossed 25 it
                // stayed there, and from that moment EVERY reply-worthy email
                // was flagged instead of drafted — including live human
                // threads. The user's report was simply "it doesn't draft an
                // email at all for those", and the daemon log agreed:
                //
                //   reply downgraded to flag: approval queue at capacity
                //     from=<a real client on a live thread>  pending=54 cap=25
                //
                // A queue-depth number is the wrong thing to hang "does this
                // person get a reply" on. The volume problem is fixed at the
                // source instead: bulk senders no longer reach the draft phase
                // (#451), and answered cards now retire themselves (#449), so
                // the queue reflects real work. A reply-worthy email always
                // gets a draft now.
                //
                // --- 2. DRAFT phase. Code-mode (#52 / I6) is the production
                // default; on any code-mode failure we log + fall through to
                // the classic prompt path so a model hiccup or sandbox glitch
                // never blocks a reply. I7 (#53) will replace the stub
                // fallback below with the full self-repair + gh-issue +
                // Discord-notice flow.
                let wiki_hint = self
                    .config
                    .wiki_root
                    .as_ref()
                    .map(|root| {
                        let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                        augmentagent_wiki::WikiReader::new(&layout).draft_hint(&email)
                    })
                    .unwrap_or_default();
                let tone_block = pick_tone_block(&self.store, entity_id, &email.from);
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
                // #35 Phase 2/3/5: pre-resolve structured asks (scheduling /
                // calendly / meeting_link / share_doc / intro) and inject
                // concrete values. Gated by AUGMENTAGENT_ASK_RESOLVE=live +
                // per-resolver flags; an empty outcome (today's behavior) for
                // off/shadow or when no ask clears the confidence floor.
                // `unresolved` carries asks the resolver couldn't fill — they
                // ride along on the persisted draft as a needs-input marker
                // so the Discord card can surface a "Needs your input" field
                // (#35 Phase 5). Empty `unresolved` ⇒ marker is never
                // appended ⇒ draft + card stay byte-identical to pre-#35.
                let resolve_outcome = augmentagent_channel_core::resolve_asks(
                    &self.reasoner,
                    augmentagent_channel_core::AskResolveMode::from_env(),
                    &email.body,
                    self.build_resolve_ctx(entity_id),
                )
                .await;

                // --- 2a. CODE-MODE attempt. Two-step flow:
                //   1. Reasoner emits a TypeScript program that orchestrates
                //      tool calls and ends with `tools.draft(channel, body, reason)`.
                //   2. The Deno sandbox executes the program. The dispatcher's
                //      terminal `tools.draft` handler writes the actions row
                //      (mode='code', generatedSource, toolCallTrace) and stashes
                //      the action id for us to pick up below.
                //
                // On ANY failure (reasoner spawn, missing fenced block, sandbox
                // timeout, runtime exception, dispatcher error) we hand off to
                // `handle_code_mode_failure` (I7 / #53) which runs one
                // self-repair pass. If the repair lands a working code-mode
                // draft, we use it. Otherwise we fall through to the classic
                // prompt path AND, after it lands its row, call
                // `report_classic_fallback` to file the postmortem gh issue +
                // post the Discord notice.
                let manifest = manifest_v1();
                let system_prompt = code_mode_system(&manifest);
                let user_msg = code_mode_user_message(
                    &email,
                    &wiki_hint,
                    &tone_block,
                    &thread_block,
                    &archetype_block,
                    &resolve_outcome.block,
                );
                // Opts mirror `draft_opts`' shape: same permission mode, no
                // allowed_tools / add_dirs — the Deno sandbox is the tool
                // surface, not the host claude CLI's Read/Grep/Glob.
                let code_mode_opts = augmentagent_channel_core::ReasonerOpts {
                    system_prompt,
                    model: None,
                    allowed_tools: Vec::new(),
                    add_dirs: Vec::new(),
                    permission_mode: "default".into(),
                    cwd: None,
                    env: Vec::new(),
                    settings_json: None,
                    restrict_env: false,
                    audit_logger: None,
                    audit_notifier: None,
                    session_id: None,
                };
                let message_ctx = MessageContext {
                    channel: "gmail".to_string(),
                    email: email.clone(),
                    account_id: Some(entity_id.to_string()),
                };

                // Attempt 1: original program. Capture the source on a
                // `NoCodeBlock`-vs-`RunnerError` distinction so the failure
                // handler can pass the program text to the repair prompt.
                let mut cm_source: String = String::new();
                let cm_attempt: Result<String, (augmentagent_channel_core::code_mode::CodeModeError, FailureStage)> = async {
                    let ts_source = match self.reasoner.call_code_mode(&code_mode_opts, &user_msg).await {
                        Ok(s) => s,
                        Err(e) => {
                            // Downcast to CodeModeError when possible; otherwise
                            // wrap. The repair prompt only sees the error text,
                            // so the wrap fidelity is purely diagnostic.
                            let cme = match e.downcast::<augmentagent_channel_core::code_mode::CodeModeError>() {
                                Ok(cme) => cme,
                                Err(other) => augmentagent_channel_core::code_mode::CodeModeError::ReasonerFailed(other),
                            };
                            return Err((cme, FailureStage::CallCodeMode));
                        }
                    };
                    cm_source = ts_source.clone();
                    let dispatcher = DefaultDispatcher::new(
                        self.store.as_ref(),
                        message_ctx.clone(),
                        ts_source.clone(),
                    )
                    .with_wiki_hint(wiki_hint.clone());
                    if let Err(e) = code_mode::run_program(&ts_source, &manifest, &dispatcher).await {
                        // Wrap a RunnerError in CodeModeError::ReasonerFailed
                        // so the failure handler has a uniform type. The
                        // stage tag distinguishes the two layers; the
                        // wrapped error text preserves the kind+message+stack.
                        let wrapped = augmentagent_channel_core::code_mode::CodeModeError::ReasonerFailed(
                            anyhow::anyhow!("run_program: {e}"),
                        );
                        return Err((wrapped, FailureStage::RunProgram));
                    }
                    match dispatcher.last_action_id() {
                        Some(id) => Ok(id),
                        None => Err((
                            augmentagent_channel_core::code_mode::CodeModeError::ReasonerFailed(
                                anyhow::anyhow!("code-mode program produced no draft call"),
                            ),
                            FailureStage::RunProgram,
                        )),
                    }
                }
                .await;

                // Either Some(action_id) → keep going on the code-mode rail,
                // or None + carried FailureRecord → run classic, then report.
                let (code_mode_action_id, pending_classic_record): (
                    Option<String>,
                    Option<augmentagent_channel_core::code_mode::FailureRecord>,
                ) = match cm_attempt {
                    Ok(action_id) => (Some(action_id), None),
                    Err((cme, stage)) => {
                        warn!(
                            message_id = %email.message_id,
                            stage = ?stage,
                            "code-mode attempt failed: {cme}; invoking self-repair"
                        );
                        let failure_ctx = FailureCtx {
                            reasoner: self.reasoner.as_ref(),
                            opts: code_mode_opts.clone(),
                            user_msg: user_msg.clone(),
                            manifest: manifest.clone(),
                            message_ctx: message_ctx.clone(),
                            wiki_hint: wiki_hint.clone(),
                            store: Arc::clone(&self.store),
                            broker: Arc::clone(&self.approvals),
                            gh: Arc::clone(&self.gh_issue_runner),
                            email: email.clone(),
                            channel: "gmail".to_string(),
                            model: code_mode_opts.model.clone(),
                            manifest_version: "v1",
                        };
                        match handle_code_mode_failure(&failure_ctx, &cm_source, &cme, stage).await
                        {
                            DraftOutcome::CodeMode {
                                action_id,
                                repair_used,
                            } => {
                                info!(
                                    message_id = %email.message_id,
                                    action_id = %action_id,
                                    repair_used,
                                    "code-mode self-repair succeeded"
                                );
                                (Some(action_id), None)
                            }
                            DraftOutcome::ClassicNeeded(record) => {
                                error!(
                                    message_id = %email.message_id,
                                    "code-mode self-repair failed; falling back to classic"
                                );
                                (None, Some(record))
                            }
                        }
                    }
                };

                // --- 2b. Code-mode success path: read the persisted draft
                // body out of the actions row the dispatcher just wrote, then
                // hand off to the existing approval / Gmail-draft flow. The
                // downstream code (create_draft, post_approval, record_nudge,
                // mark_email_processed) is unchanged — see `dispatch_reply`'s
                // `Some(existing_action_id)` arm.
                if let Some(action_id) = code_mode_action_id {
                    let draft_body = self
                        .store
                        .get_action_with_email(&action_id)?
                        .and_then(|a| a.action.draft_body)
                        .unwrap_or_default();

                    if self.config.dry_run {
                        // Promote the dispatcher's `Pending` row to `DryRun`
                        // so daemon dry-run accounting matches classic. The
                        // persisted code-mode columns (mode='code',
                        // generatedSource, toolCallTrace) are untouched.
                        self.store.update_action_status(
                            &action_id,
                            ActionStatus::DryRun,
                            Some(&draft_body),
                            None,
                        )?;
                        self.store
                            .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                        println!(
                            "[reply dry-run:code] {} from={} subject={}\n--- draft ---\n{}\n--- /draft ---",
                            email.message_id, email.from, email.subject, draft_body,
                        );
                        self.maybe_ingest(
                            &email,
                            DecisionKind::Reply,
                            decision.reason.as_deref(),
                            Some(&draft_body),
                            IngestTrigger::DryRunDrafted,
                        );
                        return Ok(Some(DispatchOutcome::DryRun));
                    }
                    return self
                        .dispatch_reply(entity_id, email, draft_body, Some(action_id))
                        .await;
                }

                // --- 2c. Classic fallback (I7).
                //
                // Reached when code-mode failed AND self-repair didn't produce
                // a working code-mode draft. Behaviour matches the pre-#52
                // classic prompt path — same draft call, same needs-input
                // marker append, same dispatch — except we then call
                // `report_classic_fallback` to file the postmortem gh issue
                // and post the Discord notice tagged with the classic
                // action_id.
                let draft_opts = crate::reasoner::draft_opts(
                    draft_skill.to_string(),
                    self.config.wiki_root.clone(),
                );
                let draft_prompt = draft_user_message(
                    &email,
                    &wiki_hint,
                    &tone_block,
                    &thread_block,
                    &archetype_block,
                    &resolve_outcome.block,
                );
                let draft = match self.reasoner.call(&draft_opts, &draft_prompt).await {
                    Ok(s) => {
                        let body = s.trim().to_string();
                        // Attach the needs-input marker (no-op when there are
                        // no unresolved asks → byte-identical persisted draft).
                        let pairs: Vec<(String, String)> = resolve_outcome
                            .unresolved
                            .iter()
                            .map(|u| (u.kind.as_str().to_string(), u.text.clone()))
                            .collect();
                        augmentagent_approval_discord::append_needs_input_marker(&body, &pairs)
                    }
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
                    let action_id = self.store.log_action(
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
                    // I7: file postmortem when classic fallback was triggered
                    // by a code-mode failure. Successful repair never reaches
                    // here (it returns Some(action_id) above), so this branch
                    // is exclusively for "repair couldn't save it".
                    if let Some(record) = pending_classic_record {
                        let failure_ctx = FailureCtx {
                            reasoner: self.reasoner.as_ref(),
                            opts: code_mode_opts.clone(),
                            user_msg: user_msg.clone(),
                            manifest: manifest.clone(),
                            message_ctx: message_ctx.clone(),
                            wiki_hint: wiki_hint.clone(),
                            store: Arc::clone(&self.store),
                            broker: Arc::clone(&self.approvals),
                            gh: Arc::clone(&self.gh_issue_runner),
                            email: email.clone(),
                            channel: "gmail".to_string(),
                            model: code_mode_opts.model.clone(),
                            manifest_version: "v1",
                        };
                        report_classic_fallback(&failure_ctx, &record, &action_id).await;
                    }
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        decision.reason.as_deref(),
                        Some(&draft),
                        IngestTrigger::DryRunDrafted,
                    );
                    return Ok(Some(DispatchOutcome::DryRun));
                }

                // Non-dry-run classic dispatch. To make the action_id known
                // before `dispatch_reply` runs (so we can pass it to
                // `report_classic_fallback`), we pre-create the row in
                // `Pending` and let `dispatch_reply` reuse it via the
                // `existing_action_id` arm.
                let classic_action_id = if pending_classic_record.is_some() {
                    Some(self.store.log_action(
                        &email.message_id,
                        email.thread_id.as_deref(),
                        &email.from,
                        &email.subject,
                        Some(&email.body),
                        Some(&draft),
                        ActionStatus::Pending,
                    )?)
                } else {
                    None
                };
                if let (Some(record), Some(action_id)) =
                    (pending_classic_record.as_ref(), classic_action_id.as_ref())
                {
                    let failure_ctx = FailureCtx {
                        reasoner: self.reasoner.as_ref(),
                        opts: code_mode_opts.clone(),
                        user_msg: user_msg.clone(),
                        manifest: manifest.clone(),
                        message_ctx: message_ctx.clone(),
                        wiki_hint: wiki_hint.clone(),
                        store: Arc::clone(&self.store),
                        broker: Arc::clone(&self.approvals),
                        gh: Arc::clone(&self.gh_issue_runner),
                        email: email.clone(),
                        channel: "gmail".to_string(),
                        model: code_mode_opts.model.clone(),
                        manifest_version: "v1",
                    };
                    report_classic_fallback(&failure_ctx, record, action_id).await;
                }
                self.dispatch_reply(entity_id, email, draft, classic_action_id)
                    .await
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
    fn build_resolve_ctx(&self, entity_id: &str) -> augmentagent_channel_core::ResolveCtx {
        let mut ctx = augmentagent_channel_core::ResolveCtx {
            entity_id: Some(entity_id.to_string()),
            calendar_id: "primary".into(),
            wiki_root: self.config.wiki_root.clone(),
            freebusy: None,
            drive: None,
        };
        if let Ok(key) = std::env::var("COMPOSIO_API_KEY") {
            if !key.trim().is_empty() {
                let client =
                    std::sync::Arc::new(augmentagent_channel_core::ComposioResolveClient::new(key));
                ctx.freebusy = Some(client.clone());
                ctx.drive = Some(client);
            }
        }
        ctx
    }

    /// Build the `<thread_history>` block for thread-aware drafting (#32).
    ///
    /// DEFAULT-ON as of #450 (opt out with `AUGMENTAGENT_THREAD_AWARE=0`).
    ///
    /// This was gated behind an opt-in `=1` that was never set in any config,
    /// so in production the block was always empty and every reply on a
    /// multi-message thread was drafted as if the thread were one isolated
    /// message — no memory of what either side had already said. That is half
    /// of what "it doesn't draft properly for threads" meant in #450; a draft
    /// written without the back-and-forth isn't worth sending.
    ///
    /// Best-effort: a fetch failure logs a warning and degrades to the empty
    /// block rather than aborting the draft. The inbound message itself is
    /// excluded so the model doesn't see it twice (it's already in the
    /// `<email>` block).
    async fn fetch_thread_block(
        &self,
        entity_id: &str,
        email: &augmentagent_store::Email,
    ) -> String {
        let thread_aware = std::env::var("AUGMENTAGENT_THREAD_AWARE")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(true);
        if !thread_aware {
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
        let block = augmentagent_channel_core::prompt::format_thread_history(&prior);
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

    /// #629 — the reply-all Cc set for `email`: every participant seen on the
    /// thread (From/To/Cc of each message), minus the addressee (`email.from`
    /// goes on To) and minus every connected account of the owner's. Seeded
    /// from the inbound message's own headers so a failed thread fetch — or a
    /// retry row whose Email round-tripped through sqlite without headers —
    /// degrades to plain reply-all-on-the-latest-message, never to a crash.
    /// Bare addresses, first-seen order, case-insensitive dedup.
    async fn reply_all_cc(
        &self,
        entity_id: &str,
        email: &augmentagent_store::Email,
    ) -> Vec<String> {
        use crate::gmail::split_recipients;
        fn push_addrs(
            raw: &str,
            seen: &mut std::collections::HashSet<String>,
            out: &mut Vec<String>,
        ) {
            for addr in split_recipients(raw) {
                if seen.insert(addr.to_ascii_lowercase()) {
                    out.push(addr);
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        push_addrs(&email.to, &mut seen, &mut out);
        push_addrs(&email.cc, &mut seen, &mut out);
        if let Some(tid) = email.thread_id.as_deref() {
            // The issue asks for ALL prior To+Cc participants, not just the
            // latest message's — someone dropped from a recent hop (the Code
            // N Camp case) must still be recoverable from earlier hops.
            const MAX_PARTICIPANT_SCAN: u32 = 50;
            match self
                .gmail
                .fetch_thread_messages(entity_id, tid, MAX_PARTICIPANT_SCAN)
                .await
            {
                Ok(msgs) => {
                    for m in &msgs {
                        push_addrs(&m.from, &mut seen, &mut out);
                        push_addrs(&m.to, &mut seen, &mut out);
                        push_addrs(&m.cc, &mut seen, &mut out);
                    }
                }
                Err(e) => warn!(
                    message_id = %email.message_id,
                    thread_id = %tid,
                    "reply-all: thread participant fetch failed; \
                     using the inbound message's headers only: {e}"
                ),
            }
        }
        let mut excluded: std::collections::HashSet<String> = std::collections::HashSet::new();
        excluded.insert(crate::gmail::extract_bare_email(&email.from).to_ascii_lowercase());
        match self.store.get_active_gmail_accounts() {
            Ok(accounts) => {
                for a in accounts {
                    excluded.insert(a.email.to_ascii_lowercase());
                }
            }
            Err(e) => warn!("reply-all: could not list own accounts for self-exclusion: {e}"),
        }
        out.retain(|a| !excluded.contains(&a.to_ascii_lowercase()));
        out
    }

    /// Route a meeting / calendar invite off the reply rail (#834): retire the
    /// action row, mark the email terminally processed as `Flag`, post the
    /// Discord heads-up, ingest. No draft, no approval card.
    ///
    /// `existing_action_id` is `Some` when a row already exists for this
    /// message — the retry tick's errored row, or the code-mode / classic row
    /// pre-created before dispatch. Those move to `Superseded` instead of
    /// gaining a second row, which is what makes the suppression terminal:
    /// `list_retryable_replies` only selects `status='error'`, so the retry
    /// tick stops handing the invite back, and any Discord card already
    /// showing for that id is dead on its next sqlite lookup.
    async fn suppress_meeting_invite(
        &self,
        email: &augmentagent_store::Email,
        existing_action_id: Option<&str>,
    ) -> anyhow::Result<DispatchOutcome> {
        let reason = "meeting invite (no draft)";
        match existing_action_id {
            Some(id) => {
                self.store.update_action_status(
                    id,
                    ActionStatus::Superseded,
                    None,
                    Some(reason),
                )?;
            }
            None => {
                self.store.log_flagged_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    reason,
                )?;
            }
        }
        self.store
            .mark_email_processed(&email.message_id, TriageResult::Flag)?;
        println!(
            "[flag:meeting-invite] {} from={}",
            email.message_id, email.from,
        );
        // Best-effort heads-up, same as the Flag arm in `process_email`: a
        // broker failure must not abort the flow.
        if let Err(e) = self.approvals.post_flag_notice(email, reason).await {
            warn!(
                message_id = %email.message_id,
                "post_flag_notice failed: {e}"
            );
        }
        self.maybe_ingest(
            email,
            DecisionKind::Flag,
            Some(reason),
            None,
            IngestTrigger::Triaged,
        );
        Ok(DispatchOutcome::Flagged)
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
        // MEETING-INVITE BACKSTOP (#834). `retry_once` reaches here without
        // ever re-running triage whenever the errored action still carries a
        // draft body — the #451 re-triage detour only covers empty drafts —
        // so the triage-side gate above cannot see that traffic. Any invite
        // draft queued before this fix, or drafted and then stranded by a
        // `post_approval` failure, would otherwise be re-drafted and carded
        // by the very next retry tick. Checking at the single choke point
        // every reply passes through closes that hole for good.
        //
        // Only subject + body are consulted, because that is all the retry
        // path has: `list_retryable_replies` rehydrates `Email` from sqlite
        // with `attachments` empty. The reported Teams forward is caught on
        // its body alone, so the persisted shape is covered.
        if is_meeting_invite(&email.subject, &email.body, &email.attachments) {
            return Ok(Some(
                self.suppress_meeting_invite(&email, existing_action_id.as_deref())
                    .await?,
            ));
        }

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

        // #629 — replies default to reply-all: sender on To, every other
        // thread participant on Cc. The envelope is computed once per action
        // and persisted (`set_action_envelope`), so the retry tick reuses it
        // instead of re-fetching the thread, and Revise (#473) recreates the
        // draft with the same Cc instead of silently dropping it.
        let cc: Vec<String> = match self.store.get_action_envelope(&action_id) {
            Ok(Some(env)) => env
                .cc
                .as_deref()
                .map(crate::gmail::split_recipients)
                .unwrap_or_default(),
            Ok(None) => {
                let cc = self.reply_all_cc(entity_id, &email).await;
                let to_bare = crate::gmail::extract_bare_email(&email.from);
                if let Err(e) = self.store.set_action_envelope(
                    &action_id,
                    Some(&to_bare),
                    Some(&cc.join(", ")),
                    None,
                ) {
                    warn!(action_id, "reply-all: envelope persist failed: {e}");
                }
                cc
            }
            Err(e) => {
                warn!(
                    action_id,
                    "reply-all: envelope lookup failed; drafting sender-only: {e}"
                );
                Vec::new()
            }
        };

        // The Gmail draft must be the clean reply text — strip the #35
        // needs-input marker and the #785 assumes marker (both are
        // Discord-card-only carriers; they live in `actions.draftBody` so the
        // card can render their fields, but neither may reach Gmail). This is
        // the single choke point for both draft rails: the code-mode body read
        // back from the actions row and the classic draft alike. No markers ⇒
        // unchanged bytes.
        let (human_draft, needs_input_asks) =
            augmentagent_approval_discord::split_needs_input(&initial_draft);
        let (human_draft, assumed_facts) =
            augmentagent_approval_discord::split_assumes(&human_draft);
        // `split_assumes` is tolerant by design — a malformed fence stays put
        // so the card never renders half-parsed markup. A Gmail body has no
        // such luxury, so it gets the strict scrub on top.
        let gmail_body = augmentagent_approval_discord::strip_assumes_for_send(&human_draft);
        let draft_id = match existing_draft_id {
            Some(d) => d,
            None => match self
                .gmail
                .create_draft_with_cc(
                    entity_id,
                    &email.from,
                    &cc,
                    &reply_subject(&email.subject),
                    &gmail_body,
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

        // Surface the Cc set on the card the same way #473 compose cards and
        // their reposts do (`append_envelope_markers`): a `[cc: …]` display
        // marker. Card-only — `draftBody` in sqlite and the Gmail draft both
        // stay the clean reply text. The marker goes into the HUMAN part with
        // any #35 needs-input marker re-appended last: `split_needs_input`
        // (run by every card render) discards text after the marker close,
        // so appending after it would make the cc invisible on the card.
        let card_body = if cc.is_empty() {
            initial_draft.clone()
        } else {
            let with_cc = format!("{gmail_body}\n\n[cc: {}]", cc.join(", "));
            // Re-attach the card-only markers stripped above, needs-input
            // last. `split_assumes` splices its fence out on render, so the
            // cc marker survives regardless of which one sits where.
            let with_assumes =
                augmentagent_approval_discord::append_assumes_marker(&with_cc, &assumed_facts);
            if needs_input_asks.is_empty() {
                with_assumes
            } else {
                let pairs: Vec<(String, String)> = needs_input_asks
                    .iter()
                    .map(|a| (a.kind.clone(), a.text.clone()))
                    .collect();
                augmentagent_approval_discord::append_needs_input_marker(&with_assumes, &pairs)
            }
        };
        if let Err(e) = self
            .approvals
            .post_approval(&action_id, &email, &card_body)
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
    pub async fn run_arc(self: Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
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
impl<G: GmailApi + 'static, R: Reasoner + 'static> WorkItemHandler for GmailWorkHandler<G, R> {
    async fn handle(&self, item: WorkItem) -> anyhow::Result<()> {
        let email: augmentagent_store::Email = serde_json::from_value(item.payload)
            .map_err(|e| anyhow::anyhow!("gmail work item payload not an Email: {e}"))?;
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
    /// #222: terminal — sender classified as event-platform /
    /// signup-confirmation blast. Wiki ingest ran (CRM still learns about
    /// the organizer, event, attendees), but no draft was generated, no
    /// Discord approval card was queued, and no flag notice was posted.
    IngestOnly,
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

    /// Disable `gh issue create` for the entire test binary. Without this,
    /// any test that traverses the I7 code-mode-failure → classic fallback
    /// path would spawn the production gh CLI and (because dev boxes are
    /// commonly authenticated) create a real issue on
    /// `nolanmak/MyAgentAssistant`. The env-var check lives in
    /// `GhCliIssueRunner::create_issue`; setting it once at module load is
    /// cheaper than threading a mock runner through every existing
    /// reply-flow test. Tests that DO want to assert gh invocations inject
    /// a `RecordingGh` via `.with_gh_issue_runner(...)` — which bypasses
    /// the env-var check entirely.
    static GH_DISABLE_INIT: std::sync::Once = std::sync::Once::new();
    fn disable_gh_for_tests() {
        GH_DISABLE_INIT.call_once(|| {
            // SAFETY: set once at module init before any test runs; no
            // concurrent reads of this var.
            std::env::set_var("AUGMENTAGENT_GH_DISABLE", "1");
        });
    }

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
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<Option<String>, crate::gmail::GmailError> {
            Ok(None)
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
        // Belt-and-suspenders: any test that opens a store also disables
        // the real `gh` CLI for the rest of the process.
        disable_gh_for_tests();
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
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
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
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m2".into(),
                thread_id: Some("t2".into()),
                from: "user@example.com".into(),
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

    /// Mock gh-CLI runner that records every `gh issue create` call without
    /// spawning the real binary. Returns canned issue numbers starting at
    /// `next_number`. Wire into a `GmailChannel` via `.with_gh_issue_runner(...)`
    /// so I7 postmortem tests never touch the production repo.
    #[derive(Default)]
    struct RecordingGh {
        calls: std::sync::Mutex<Vec<(String, String, Vec<String>)>>,
        next_number: std::sync::Mutex<u64>,
    }
    impl RecordingGh {
        fn new(start: u64) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                next_number: std::sync::Mutex::new(start),
            }
        }
    }
    #[async_trait]
    impl GhIssueRunner for RecordingGh {
        async fn create_issue(
            &self,
            title: &str,
            body: &str,
            labels: &[&str],
        ) -> anyhow::Result<u64> {
            self.calls.lock().unwrap().push((
                title.to_string(),
                body.to_string(),
                labels.iter().map(|s| s.to_string()).collect(),
            ));
            let mut n = self.next_number.lock().unwrap();
            let out = *n;
            *n += 1;
            Ok(out)
        }
    }

    /// No-op gh runner. Kept available as a swap-in for any future tests
    /// that want to skip postmortem-issue assertions without falling back to
    /// the env-var disable hook. Today every test either uses the env hook
    /// (default path) or wires `RecordingGh` explicitly.
    #[allow(dead_code)]
    struct NoopGh;
    #[async_trait]
    impl GhIssueRunner for NoopGh {
        async fn create_issue(&self, _: &str, _: &str, _: &[&str]) -> anyhow::Result<u64> {
            Ok(0)
        }
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
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
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
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
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

    /// #217: even if the triage model returns `reply` for a non-human
    /// sender (a GitHub notification, a no-reply marketing blast, …),
    /// the live dispatch must intercept and route to Skip — no Opus
    /// draft, no Discord card, no flag notice. Without the
    /// `is_human_sender` guard this test would post an approval card.
    #[tokio::test]
    async fn reply_decision_for_automated_sender_routes_to_skip() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-bot".into(),
                thread_id: Some("t-bot".into()),
                // Classic automated sender — `noreply@` local part on a
                // domain (`github.com`) that routinely sends actionable-
                // looking notifications that occasionally fool the
                // triage model into returning `reply`.
                from: "noreply@github.com".into(), // pii-ok: synthetic test fixture
                subject: "[org/repo] PR #42 review requested".into(),
                body: "@you was requested to review PR #42. Please respond.".into(),
                date: "2026-05-27".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Force the failure mode: triage says `reply` (model bug we're
        // guarding against). If the draft phase ever ran it would pull
        // the second scripted response — assertions below prove it
        // doesn't.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"requested action"}"#,
            "Sure, I'll take a look.",
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
        // Routed to Skip, NOT replied or awaiting approval.
        assert_eq!(out.skipped, 1, "automated sender must be skipped");
        assert_eq!(out.awaiting_approval, 0, "no approval card for noreply@");
        assert_eq!(out.replied_dry_run, 0);
        assert_eq!(out.flagged, 0, "no flag — automated mail is silent skip");
        // Discord broker untouched on both rails (card + flag notice).
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        assert_eq!(broker.flag_posts.lock().unwrap().len(), 0);
        // Email is terminally processed so the next tick won't re-spawn.
        assert!(store.is_email_complete("m-bot").unwrap());
    }

    /// #222: a Partiful "Registration Confirmed" event blast where the
    /// triage model returns `reply` (the failure mode we're guarding
    /// against — the model occasionally treats RSVP confirmations as
    /// reply-worthy). The new event-blast gate must intercept after the
    /// is_human_sender check and before the backpressure block, routing
    /// to IngestOnly: no approval card, no flag notice, no Opus draft
    /// call. The wiki ingest path is still spawned (asserted indirectly
    /// via the action row + `IngestOnly` outcome — actual ingest is a
    /// best-effort fire-and-forget that's separately covered).
    #[tokio::test]
    async fn event_blast_sender_routes_to_ingest_only() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-event".into(),
                thread_id: Some("t-event".into()),
                // Partiful invite domain — matches the curated
                // EVENT_BLAST_DOMAIN_PATTERNS list. Local part is a
                // plausible human-looking display name to ensure we're
                // matching on the domain rule, not just bouncing on
                // `noreply@` (which the #217 guard would catch first).
                // Use an innocuous local part so the #217 `is_human_sender`
                // guard doesn't fire first — we want to assert the new
                // event-blast gate is what intercepts this, classifying
                // on the Partiful domain rule.
                from: "\"Partiful\" <invites@partiful-mail.com>".into(), // pii-ok: synthetic test fixture
                subject: "Registration Confirmed: NY Tech Week Drinks".into(),
                body: "You're confirmed. Add to calendar: https://example.com/cal.ics".into(),
                date: "2026-05-27".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Triage returns `reply` so we exercise the Reply arm. If the
        // gate didn't fire, the second scripted response would be
        // consumed by the draft phase and an approval card would post.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"confirm attendance"}"#,
            "See you there!",
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
        // Terminal outcome is IngestOnly, NOT replied / awaiting / flagged.
        assert_eq!(out.ingest_only, 1, "event blast must be ingest-only");
        assert_eq!(out.awaiting_approval, 0, "no approval card for event blast");
        assert_eq!(out.replied_dry_run, 0);
        assert_eq!(out.flagged, 0);
        assert_eq!(out.skipped, 0, "did not fall through to is_human_sender skip");
        // Discord broker is silent on BOTH rails — explicit user preference:
        // no notice for event blasts.
        assert_eq!(
            broker.posts.lock().unwrap().len(),
            0,
            "no approval card posted",
        );
        assert_eq!(
            broker.flag_posts.lock().unwrap().len(),
            0,
            "no flag notice posted — event blasts must be silent",
        );
        // Email is terminally processed; the next poll tick must not
        // re-spawn a draft for the same message.
        assert!(store.is_email_complete("m-event").unwrap());

        // Re-polling the same unread email must remain a no-op.
        let out2 = ch.poll_once().await.unwrap();
        assert_eq!(out2.ingest_only, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        assert_eq!(broker.flag_posts.lock().unwrap().len(), 0);
    }

    /// The forwarded Microsoft Teams invite reported in #834, with invented
    /// identities. Shared by both #834 channel tests.
    const TEAMS_INVITE_BODY: &str = "\
Passing this along.

________________________________________
Microsoft Teams meeting
Join on your computer, mobile app or room device
Join the meeting now
Meeting ID: 123 456 789
When: Wednesday, September 2 1:00 PM-2:00 PM
Where: Microsoft Teams
";

    /// #834: a forwarded Microsoft Teams meeting invite from a human sender
    /// where the triage model returns `reply` (the reported failure mode —
    /// the draft asked the organizer to confirm attendance and share an
    /// agenda). The meeting-invite gate must intercept and route to Flag:
    /// no approval card, no Opus draft, but the invite IS surfaced on
    /// Discord so the user can handle it on their calendar.
    #[tokio::test]
    async fn reply_decision_for_meeting_invite_routes_to_flag() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-invite".into(),
                thread_id: Some("t-invite".into()),
                // Human sender on a human domain — clears the #217 guard,
                // and nothing here matches the #222 event-blast lists, so
                // the new gate is provably what intercepts.
                from: "Jeffrey Walters <jeff@example.com>".into(), // pii-ok: synthetic test fixture
                subject: "FW: Updates Perry".into(),
                body: TEAMS_INVITE_BODY.into(),
                date: "2026-08-27".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Triage says `reply`. If the gate didn't fire, the second scripted
        // response would be consumed by the draft phase and an approval
        // card would post.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"confirm attendance"}"#,
            "Happy to attend — could you share an agenda?",
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
        assert_eq!(out.flagged, 1, "meeting invite must be flagged");
        assert_eq!(out.awaiting_approval, 0, "no approval card for an invite");
        assert_eq!(out.replied_dry_run, 0);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.ingest_only, 0);
        // No draft was generated — the approval rail is untouched.
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        // But the invite IS surfaced so the user can put it on the calendar.
        let flags = broker.flag_posts.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, "m-invite");
        assert!(flags[0].1.contains("meeting invite"));
        // Terminally processed — the next tick must not re-spawn a draft.
        assert!(store.is_email_complete("m-invite").unwrap());
    }

    /// #834 REGRESSION (queued/retried traffic). `dispatch_reply` runs AFTER
    /// triage, so the triage-side gate cannot see an invite draft that is
    /// already in the queue: an errored action WITH a draft body is exactly
    /// the shape #451's re-triage detour does NOT cover, so `retry_once`
    /// hands it straight to `dispatch_reply`. Seeded here as an invite draft
    /// queued before the guard existed. The backstop must suppress it
    /// terminally — no card, and never offered to the retry tick again.
    #[tokio::test]
    async fn retry_of_queued_meeting_invite_draft_is_suppressed() {
        let (store, _f) = tmp_store();
        let email = Email {
            // Empty, as `list_retryable_replies` rehydrates it: subject and
            // body are all the backstop gets on this path.
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "m-invite-queued".into(),
            thread_id: Some("t-invite".into()),
            from: "Jeffrey Walters <jeff@example.com>".into(), // pii-ok: synthetic test fixture
            subject: "FW: Updates Perry".into(),
            body: TEAMS_INVITE_BODY.into(),
            date: "2026-08-27".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&email).unwrap();
        let action_id = store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("Happy to attend — could you share an agenda?"),
                ActionStatus::Error,
            )
            .unwrap();

        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            Arc::new(StubGmail { emails: vec![] }),
            // Unscripted: suppression must never consult the model.
            Arc::new(ScriptedReasoner::new([])),
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                retry_min_gap: Duration::from_millis(0),
                ..Default::default()
            },
        );

        assert_eq!(ch.retry_once().await.unwrap(), 1);
        // Before the backstop this posted the invite draft as a card.
        assert!(broker.posts.lock().unwrap().is_empty());
        {
            let flags = broker.flag_posts.lock().unwrap();
            assert_eq!(flags.len(), 1);
            assert!(flags[0].1.contains("meeting invite"));
        }
        // Terminal on both rails: the row leaves 'error' so
        // `list_retryable_replies` can't return it, and the email is marked
        // processed so the poll loop won't re-triage it either.
        let row = store.get_action_with_email(&action_id).unwrap().unwrap();
        assert_eq!(row.action.status, "superseded");
        assert!(store.is_email_complete("m-invite-queued").unwrap());
        assert_eq!(
            ch.retry_once().await.unwrap(),
            0,
            "suppressed invite was re-queued"
        );
    }

    /// #218 — when the outbound observer has already recorded a user reply
    /// on the same thread newer than the inbound's arrival, the inbound
    /// must be skipped before any draft / approval card / flag fires.
    /// Mirrors the `is_human_sender` skip shape (no Discord noise, wiki
    /// ingest still runs, email marked complete).
    #[tokio::test]
    async fn reply_decision_skipped_when_user_already_replied_on_thread() {
        let (store, _f) = tmp_store();
        // Seed: user replied on T-already at a timestamp NEWER than the
        // inbound below. acc1 matches the entity_id in tmp_store().
        let inbound_ms = chrono::DateTime::parse_from_rfc2822(
            "Wed, 27 May 2026 12:00:00 +0000",
        )
        .unwrap()
        .timestamp_millis();
        store
            .record_outbound_thread_event(
                "acc1",
                "msg-user-reply", // pii-ok: synthetic test fixture
                Some("T-already"),
                inbound_ms + 60_000, // user replied 1 minute later
            )
            .unwrap();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-already".into(),
                thread_id: Some("T-already".into()),
                // Human sender — clears the is_human_sender guard so we
                // know it's specifically the #218 guard that fires.
                from: "client@example.com".into(), // pii-ok: synthetic test fixture
                subject: "Re: project update".into(),
                body: "any thoughts on the proposal?".into(),
                date: "Wed, 27 May 2026 12:00:00 +0000".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Triage says reply — without the #218 guard a draft + card would
        // fire. The second scripted response (the draft body) must NEVER
        // be consumed.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"asks for thoughts"}"#,
            "Here are my thoughts...",
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
        assert_eq!(out.skipped, 1, "user-already-replied inbound must skip");
        assert_eq!(out.awaiting_approval, 0, "no approval card");
        assert_eq!(out.replied_dry_run, 0);
        assert_eq!(out.flagged, 0, "silent skip — no flag notice");
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        assert_eq!(broker.flag_posts.lock().unwrap().len(), 0);
        assert!(store.is_email_complete("m-already").unwrap());
    }

    /// #218 — when no outbound row exists on this thread (or none newer
    /// than the inbound), drafting proceeds normally. Negative-case pin
    /// against an over-eager skip.
    #[tokio::test]
    async fn reply_decision_drafts_when_no_user_reply_on_thread() {
        let (store, _f) = tmp_store();
        // Seed an OUTBOUND on a DIFFERENT thread; lookup must not match.
        store
            .record_outbound_thread_event(
                "acc1",
                "msg-other-thread", // pii-ok: synthetic test fixture
                Some("T-other"),
                9_999_999_999_999,
            )
            .unwrap();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-fresh".into(),
                thread_id: Some("T-fresh".into()),
                from: "client@example.com".into(), // pii-ok: synthetic test fixture
                subject: "Quick question".into(),
                body: "do you have a minute?".into(),
                date: "Wed, 27 May 2026 12:00:00 +0000".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable question"}"#,
            "Sure — happy to help.",
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
        assert_eq!(out.awaiting_approval, 1, "must draft when no thread match");
        assert_eq!(out.skipped, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn live_reply_flow_posts_approval_card() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m3".into(),
                thread_id: Some("t3".into()),
                from: "user@example.com".into(),
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

    /// #629 — Gmail stub modeling a thread with To/Cc participants; records
    /// the Cc list handed to `create_draft_with_cc`.
    struct ThreadedGmail {
        emails: Vec<Email>,
        thread: Vec<Email>,
        thread_fetch_fails: bool,
        recorded_cc: std::sync::Mutex<Option<(String, Vec<String>)>>,
        recorded_body: std::sync::Mutex<Option<String>>,
    }
    #[async_trait]
    impl GmailApi for ThreadedGmail {
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
        async fn fetch_thread_messages(
            &self,
            _e: &str,
            _t: &str,
            _m: u32,
        ) -> Result<Vec<Email>, crate::gmail::GmailError> {
            if self.thread_fetch_fails {
                Err(crate::gmail::GmailError::Composio {
                    message: "thread fetch down".into(),
                })
            } else {
                Ok(self.thread.clone())
            }
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
        async fn create_draft_with_cc(
            &self,
            _e: &str,
            to: &str,
            cc: &[String],
            _s: &str,
            b: &str,
            _th: Option<&str>,
        ) -> Result<String, crate::gmail::GmailError> {
            *self.recorded_cc.lock().unwrap() = Some((to.to_string(), cc.to_vec()));
            *self.recorded_body.lock().unwrap() = Some(b.to_string());
            Ok("draft".into())
        }
        async fn send_draft(
            &self,
            _e: &str,
            _d: &str,
        ) -> Result<Option<String>, crate::gmail::GmailError> {
            Ok(None)
        }
        async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), crate::gmail::GmailError> {
            Ok(())
        }
    }

    /// Broker capturing the card body so tests can assert display markers.
    #[derive(Default)]
    struct BodyRecordingBroker {
        bodies: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ApprovalBroker for BodyRecordingBroker {
        async fn post_approval(
            &self,
            _action_id: &str,
            _email: &Email,
            draft: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.bodies.lock().unwrap().push(draft.to_string());
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

    fn threaded_inbound() -> Email {
        Email {
            attachments: Vec::new(),
            message_id: "m-ra".into(),
            thread_id: Some("t-ra".into()),
            from: "Matt Elder <matt@example.com>".into(),
            to: "me@x.com, Will <will@example.com>".into(), // pii-ok: synthetic; matches tmp_store seed
            cc: "zack@example.com".into(),
            subject: "Receipts".into(),
            body: "can you send those over?".into(),
            date: "2026-08-17".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    #[tokio::test]
    async fn reply_defaults_to_reply_all_across_thread() {
        let (store, _f) = tmp_store();
        // An earlier hop of the thread carries a participant (chase, milan)
        // who is absent from the latest message's headers — the exact shape
        // that lost recipients before #629.
        let mut earlier = threaded_inbound();
        earlier.message_id = "m-ra0".into();
        earlier.from = "chase@example.com".into();
        earlier.to = "matt@example.com, me@x.com".into(); // pii-ok: synthetic; matches tmp_store seed
        earlier.cc = "milan@example.com".into();
        let gmail = Arc::new(ThreadedGmail {
            emails: vec![threaded_inbound()],
            thread: vec![earlier, threaded_inbound()],
            thread_fetch_fails: false,
            recorded_cc: std::sync::Mutex::new(None),
            recorded_body: std::sync::Mutex::new(None),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"asked"}"#,
            "On it.",
        ]));
        let broker = Arc::new(BodyRecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            gmail.clone(),
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

        let (to, cc) = gmail.recorded_cc.lock().unwrap().clone().unwrap();
        // Sender goes on To (full display form, as before)…
        assert_eq!(to, "Matt Elder <matt@example.com>");
        // …and Cc holds every other participant across the thread: the
        // owner's own account (me@x.com, seeded in tmp_store) and the — pii-ok
        // sender are excluded; order is first-seen; no duplicates.
        assert_eq!(
            cc,
            vec![
                "will@example.com".to_string(),
                "zack@example.com".to_string(),
                "chase@example.com".to_string(),
                "milan@example.com".to_string(),
            ]
        );

        // Card shows the Cc set as a #473-style display marker.
        {
            let bodies = broker.bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert!(
                bodies[0].contains("[cc: will@example.com, zack@example.com"),
                "card body missing cc marker: {}",
                bodies[0]
            );
        }

        // The envelope is persisted so Revise (#473) and the retry tick
        // rebuild the draft with the same Cc.
        let (action_id, _, _, _) = store.oldest_pending_actions(1).unwrap().pop().unwrap();
        let env = store.get_action_envelope(&action_id).unwrap().unwrap();
        assert_eq!(env.to.as_deref(), Some("matt@example.com"));
        assert_eq!(
            env.cc.as_deref(),
            Some("will@example.com, zack@example.com, chase@example.com, milan@example.com")
        );
    }

    #[tokio::test]
    async fn reply_all_cc_marker_survives_a_needs_input_draft() {
        // #35 needs-input drafts end with a marker, and split_needs_input
        // (run by every card render) discards text after it — the [cc:]
        // marker must land in the human part or it never shows on the card.
        let (store, _f) = tmp_store();
        let gmail = Arc::new(ThreadedGmail {
            emails: vec![threaded_inbound()],
            thread: vec![threaded_inbound()],
            thread_fetch_fails: false,
            recorded_cc: std::sync::Mutex::new(None),
            recorded_body: std::sync::Mutex::new(None),
        });
        let marked_draft = augmentagent_approval_discord::append_needs_input_marker(
            "Happy to — what time works?",
            &[("scheduling".to_string(), "meeting time".to_string())],
        );
        let broker = Arc::new(BodyRecordingBroker::default());
        let ch = GmailChannel::new(
            store,
            gmail,
            Arc::new(ScriptedReasoner::new([])),
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        // Drive dispatch_reply directly with the already-marked draft — the
        // card construction under test lives there, and the upstream
        // triage→draft→resolver chain would need a fully scripted resolver.
        let out = ch
            .dispatch_reply("acc1", threaded_inbound(), marked_draft, None)
            .await
            .unwrap();
        assert!(matches!(out, Some(DispatchOutcome::AwaitingApproval)));
        let bodies = broker.bodies.lock().unwrap();
        let (human, asks) = augmentagent_approval_discord::split_needs_input(&bodies[0]);
        assert!(
            human.contains("[cc: will@example.com"),
            "cc marker lost from rendered card body: {}",
            bodies[0]
        );
        assert_eq!(asks.len(), 1, "needs-input ask lost: {}", bodies[0]);
    }

    #[tokio::test]
    async fn assumes_marker_reaches_the_card_but_never_the_gmail_body() {
        // #785 — the drafter's assumed-facts fence is a card-only carrier: it
        // must survive into `post_approval` (so the "⚠ Assumes" field renders)
        // and be absent from the body handed to Gmail.
        let (store, _f) = tmp_store();
        let gmail = Arc::new(ThreadedGmail {
            emails: vec![threaded_inbound()],
            thread: vec![threaded_inbound()],
            thread_fetch_fails: false,
            recorded_cc: std::sync::Mutex::new(None),
            recorded_body: std::sync::Mutex::new(None),
        });
        let draft = augmentagent_approval_discord::append_assumes_marker(
            "The 14th works.",
            &["you're free on the 14th - not verified against calendar".to_string()],
        );
        let broker = Arc::new(BodyRecordingBroker::default());
        let ch = GmailChannel::new(
            store,
            gmail.clone(),
            Arc::new(ScriptedReasoner::new([])),
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch
            .dispatch_reply("acc1", threaded_inbound(), draft, None)
            .await
            .unwrap();
        assert!(matches!(out, Some(DispatchOutcome::AwaitingApproval)));

        let sent = gmail.recorded_body.lock().unwrap().clone().unwrap();
        assert_eq!(sent, "The 14th works.", "fence leaked into the Gmail body");

        let bodies = broker.bodies.lock().unwrap();
        let (human, _) = augmentagent_approval_discord::split_needs_input(&bodies[0]);
        let (human, facts) = augmentagent_approval_discord::split_assumes(&human);
        assert_eq!(facts.len(), 1, "assumed fact lost from card: {}", bodies[0]);
        assert!(
            human.contains("[cc: will@example.com"),
            "cc marker lost from card body: {}",
            bodies[0]
        );
    }

    #[tokio::test]
    async fn reply_all_degrades_to_inbound_headers_when_thread_fetch_fails() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(ThreadedGmail {
            emails: vec![threaded_inbound()],
            thread: vec![],
            thread_fetch_fails: true,
            recorded_cc: std::sync::Mutex::new(None),
            recorded_body: std::sync::Mutex::new(None),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"asked"}"#,
            "On it.",
        ]));
        let broker = Arc::new(BodyRecordingBroker::default());
        let ch = GmailChannel::new(
            store,
            gmail.clone(),
            reasoner,
            broker,
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.awaiting_approval, 1);
        let (_, cc) = gmail.recorded_cc.lock().unwrap().clone().unwrap();
        // Thread history unavailable ⇒ still reply-all on the inbound
        // message's own To+Cc (minus self), never a crash or sender-only.
        assert_eq!(
            cc,
            vec!["will@example.com".to_string(), "zack@example.com".to_string()]
        );
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
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<Option<String>, crate::gmail::GmailError> {
            Ok(None)
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
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-retry".into(),
                thread_id: Some("t-retry".into()),
                from: "user@example.com".into(),
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

    /// #670 REGRESSION — the retry queue used to be platform-blind, so the
    /// GMAIL tick scooped up socialapi Error rows: the drafted one (a
    /// post_approval failure) went straight to `dispatch_reply` and the
    /// draftless one (a triage parse failure) was superseded and re-run through
    /// the gmail pipeline. Neither row may be read, bumped or touched here.
    #[tokio::test]
    async fn retry_tick_ignores_socialapi_error_rows() {
        let (store, _f) = tmp_store();
        let mut action_ids = Vec::new();
        for (message_id, draft) in [("m-sapi-draft", Some("sure, 3pm?")), ("m-sapi-bare", None)] {
            let email = Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: message_id.into(),
                thread_id: Some("conv-1".into()),
                from: "jane <socialapi:jane>".into(),
                subject: "[DM from jane]".into(),
                body: "you around at 3?".into(),
                date: "2026-08-21".into(),
                account_entity_id: Some("acc1".into()),
                platform: "socialapi".into(),
                kind: "dm".into(),
            };
            store.upsert_email(&email).unwrap();
            action_ids.push(
                store
                    .log_action(
                        &email.message_id,
                        email.thread_id.as_deref(),
                        &email.from,
                        &email.subject,
                        Some(&email.body),
                        draft,
                        ActionStatus::Error,
                    )
                    .unwrap(),
            );
        }

        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            Arc::new(StubGmail { emails: vec![] }),
            // Unscripted: nothing in this tick should reach the reasoner.
            Arc::new(ScriptedReasoner::new([])),
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                retry_min_gap: Duration::from_millis(0),
                ..Default::default()
            },
        );

        assert_eq!(ch.retry_once().await.unwrap(), 0);
        assert!(broker.posts.lock().unwrap().is_empty());
        for id in action_ids {
            let row = store.get_action_with_email(&id).unwrap().unwrap();
            assert_eq!(row.action.status, "error", "row {id} was re-triaged");
            assert_eq!(row.retry_count, 0, "row {id} was counted as a gmail retry");
        }
    }

    /// #451 REGRESSION — the bug that filled the queue with 102 empty cards.
    ///
    /// When TRIAGE throws (a Claude session-limit reply is the common case) the
    /// old code logged an Error action with NO draft body; the retry tick then
    /// handed that straight to `dispatch_reply`, which (a) published an approval
    /// card whose draft was the empty string and (b) began *after* triage, so it
    /// skipped the automated-sender guard entirely. Newsletters that triage would
    /// never have drafted sailed into the queue.
    ///
    /// Per #363 a triage-stage failure now logs NO action at all, so the retry
    /// tick has nothing to pick up (`retry_once == 0`) and the next poll re-runs
    /// the full pipeline, guards included — a marketing blast ends up skipped,
    /// with no card and no empty draft.
    #[tokio::test]
    async fn retry_of_triage_failure_re_triages_instead_of_publishing_empty_draft() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-blast".into(),
                thread_id: Some("t-blast".into()),
                from: "Brand <marketing@engage.examplebrand.com>".into(), // pii-ok: synthetic
                subject: "Last chance to get 15% off".into(),
                body: "Shop the sale now!".into(),
                date: "Mon, 13 Jul 2026 12:00:00 +0000".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            // 1st pass: triage blows up exactly like the live session-limit
            // reply did — not JSON, so `parse_decision` errors.
            "You've hit your session limit \u{b7} resets 9:30am",
            // Retry pass: triage now answers, and (as it did live) says reply.
            r#"{"decision":"reply","reason":"promotional but asks for action"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GmailChannel::new(
            store.clone(),
            gmail,
            Arc::clone(&reasoner),
            broker.clone(),
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                dry_run: false,
                retry_min_gap: Duration::from_millis(0),
                ..Default::default()
            },
        );

        // Pass 1: triage fails -> NO action row (per #363), no draft, no card.
        let out1 = ch.poll_once().await.unwrap();
        assert_eq!(out1.errors, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);

        // The retry tick must find nothing: a triage-stage failure logs no
        // retryable action.
        assert_eq!(
            ch.retry_once().await.unwrap(),
            0,
            "a triage-stage failure must not be retried as a reply"
        );

        // Pass 2: the next poll re-runs the full pipeline. Triage now returns
        // `reply`, but the automated-sender guard fires before drafting.
        let out2 = ch.poll_once().await.unwrap();
        assert_eq!(
            out2.skipped, 1,
            "re-triage must route the marketing sender to the skip gate"
        );

        // The load-bearing assertions. Re-triage ran, the automated-sender
        // guard fired, and NO approval card exists.
        assert_eq!(
            broker.posts.lock().unwrap().len(),
            0,
            "a marketing blast must never produce an approval card on retry"
        );
        let pending: i64 = store.pending_reply_count().unwrap();
        assert_eq!(
            pending, 0,
            "retry must not leave a pending card (this is the empty-draft bug)"
        );
        // And crucially: no action anywhere holding an empty draft body.
        let empty_drafts: i64 = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM actions \
                     WHERE status = 'pending' \
                       AND (draftBody IS NULL OR TRIM(draftBody) = '')",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(empty_drafts, 0, "empty-draft approval cards must not exist");

        // Non-vacuity: both scripted responses must have been consumed. If the
        // re-triage had NOT run, response #2 would still be queued — and an
        // exhausted ScriptedReasoner returns a `skip` stub, which would make
        // this test pass for entirely the wrong reason.
        assert!(
            reasoner.responses.lock().unwrap().is_empty(),
            "the next poll must have re-run triage (which returned `reply`); the \
             skip therefore came from the automated-sender guard, not from a stub"
        );
    }

    /// The counterpart: a real person whose triage transiently failed must
    /// still get a proper draft — via the next poll's re-triage (per #363),
    /// not via the retry tick. The fix must not throw away legitimate drafts,
    /// only stop the empty/unguarded ones.
    #[tokio::test]
    async fn retry_of_triage_failure_still_drafts_for_a_human_sender() {
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-human".into(),
                thread_id: Some("t-human".into()),
                from: "Dana Rivera <dana@example-labs.ai>".into(), // pii-ok: synthetic
                subject: "Re: Catching up + next steps".into(),
                body: "Does Thursday still work for you?".into(),
                date: "Mon, 13 Jul 2026 12:00:00 +0000".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // The draft phase may consume more than one reasoner call (code-mode is
        // tried first and falls back to the classic prompt), so every response
        // after the triage answer is the draft text — whichever call lands on
        // it, the draft body is the same.
        let reasoner = Arc::new(ScriptedReasoner::new([
            "You've hit your session limit", // triage dies
            r#"{"decision":"reply","reason":"asks to confirm a time"}"#, // retry triage
            "Thursday still works — see you then.",
            "Thursday still works — see you then.",
            "Thursday still works — see you then.",
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
                ..Default::default()
            },
        );

        assert_eq!(ch.poll_once().await.unwrap().errors, 1);
        // A triage-stage failure isn't retried; the next poll re-triages and
        // this time drafts for the human sender.
        assert_eq!(ch.retry_once().await.unwrap(), 0);
        assert_eq!(ch.poll_once().await.unwrap().awaiting_approval, 1);

        assert_eq!(
            broker.posts.lock().unwrap().len(),
            1,
            "human sender must get a card once triage succeeds on re-poll"
        );
        // The card must carry the REAL draft, not the empty string that the
        // old retry path would have published.
        let draft: String = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT COALESCE(draftBody, '') FROM actions \
                     WHERE status = 'pending' LIMIT 1",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert!(
            draft.contains("Thursday"),
            "pending card must hold the real draft, got {draft:?}"
        );
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
            self.create_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("draft-once".into())
        }
        async fn send_draft(&self, _e: &str, _d: &str) -> Result<Option<String>, crate::gmail::GmailError> {
            Ok(None)
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
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-broker-fail".into(),
                thread_id: Some("t1".into()),
                from: "user@example.com".into(),
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
        assert_eq!(
            gmail.create_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(broker.posts.lock().unwrap().len(), 0);

        // Retry tick: post_approval succeeds. create_draft must NOT be called
        // a second time — the action already has a draftId from the first pass.
        let retried = ch.retry_once().await.unwrap();
        assert_eq!(retried, 1);
        assert_eq!(
            gmail.create_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(store.is_email_complete("m-broker-fail").unwrap());
    }

    /// #217 REGRESSION: a triage-STAGE failure (a model 529 surfacing as
    /// non-JSON) must NOT create a retryable reply action. Before the fix, the
    /// parse-fail branch logged a `status='error'` row that (a) tripped
    /// `has_open_action`, blocking the poll loop from ever re-triaging, and (b)
    /// was scooped by the retry tick into `dispatch_reply`, which force-drafted
    /// an empty body and posted an approval card WITHOUT the is_human_sender
    /// gate — leaking newsletters/marketing (e.g. substack.com) as cards. With
    /// the fix there is NO action row, so the next poll re-runs full triage and
    /// the automated-sender gate skips it. This reproduces the production
    /// scenario (a Substack newsletter whose first triage 529s) through the
    /// real process_email + retry_once code paths.
    #[tokio::test]
    async fn triage_parse_failure_does_not_create_retryable_action() {
        let (store, _f) = tmp_store();
        // Automated sender: substack.com is in NON_HUMAN_DOMAIN_PATTERNS, and
        // the realistic display-name form also exercises extract_bare.
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-529".into(),
                thread_id: None,
                from: "Royal Box Weekly <royalbox@substack.com>".into(), // pii-ok: synthetic substack fixture
                subject: "The Royal Box at Wimbledon".into(),
                body: "I want to switch gears this week to my favorite sport: Tennis.".into(),
                date: "2026-06-24".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            // 1st triage call: model overloaded — non-JSON, reproduces the
            // production `raw` that broke parse_decision.
            "API Error: 529 Overloaded. This is a server-side issue, usually temporary.",
            // 2nd triage call (next poll): now succeeds and even says "reply" —
            // proving it's the is_human_sender gate, not the triage verdict,
            // that skips the newsletter.
            r#"{"decision":"reply","reason":"actionable"}"#,
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

        // --- Pass 1: triage 529s. The parse failure surfaces as an error, but
        // NO action row is logged and the email stays incomplete (unread).
        let out1 = ch.poll_once().await.unwrap();
        assert_eq!(out1.errors, 1, "triage parse failure should surface as an error");
        assert_eq!(out1.awaiting_approval, 0);
        assert_eq!(out1.skipped, 0);
        // Core regression assertion: no open (pending/error) action row. Pre-fix
        // this was TRUE (an Error row was logged) — the root of the leak.
        assert!(
            !store.has_open_action("m-529").unwrap(),
            "a triage-stage failure must NOT create an action row"
        );
        assert!(!store.is_email_complete("m-529").unwrap());
        assert_eq!(broker.posts.lock().unwrap().len(), 0);

        // --- Retry tick: with no errored row, nothing is retryable, so the leak
        // path (retry_once -> dispatch_reply, ungated) never runs.
        let retried = ch.retry_once().await.unwrap();
        assert_eq!(retried, 0, "a triage-stage failure must not be retried as a reply");
        assert_eq!(
            broker.posts.lock().unwrap().len(),
            0,
            "no approval card may be posted for a newsletter"
        );

        // --- Pass 2: the poll loop re-triages from scratch. Triage now returns
        // 'reply', but is_human_sender skips the substack sender BEFORE drafting,
        // so still no card — and the email is now terminally processed (Skip).
        let out2 = ch.poll_once().await.unwrap();
        assert_eq!(out2.skipped, 1, "re-triage routes the automated sender to the skip gate");
        assert_eq!(out2.awaiting_approval, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        assert!(store.is_email_complete("m-529").unwrap());
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
            .upsert_tone_profile("domain", "startup.io", Some("acc1"), "DOMAIN", "[]", 10)
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
            .upsert_tone_profile("domain", "startup.io", Some("acc1"), "DOMAIN", "[]", 10)
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
            .upsert_tone_profile("domain", "startup.io", Some("acc1"), "DOMAIN", "[]", 3)
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
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "cr-reply".into(),
            thread_id: Some("t-cr".into()),
            from: "user@example.com".into(),
            subject: "Ping".into(),
            body: "any update?".into(),
            date: "2026-05-18".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        let gmail = Arc::new(StubGmail {
            emails: vec![email.clone()],
        });
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
        handler.handle(email_to_work_item(&email)).await.unwrap();

        // Identical observable state to live_reply_flow_posts_approval_card.
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(store.is_email_complete("cr-reply").unwrap());

        // Re-handling the same unread email (next runner tick) must NOT
        // spawn a second card — the email-complete gate holds.
        handler.handle(email_to_work_item(&email)).await.unwrap();
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn channel_runner_handler_skip_and_flag_match_poll_once() {
        let (store, _f) = tmp_store();
        let skip_email = Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
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
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
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
        handler
            .handle(email_to_work_item(&skip_email))
            .await
            .unwrap();
        handler
            .handle(email_to_work_item(&flag_email))
            .await
            .unwrap();

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

    // ---- I6 (#52): code-mode draft path ----------------------------------
    //
    // The pre-existing tests above all rely on the classic prose-draft
    // fallback (their `ScriptedReasoner` returns plain text, not a fenced
    // TS block) — when code-mode's `extract_ts_block` fails on those
    // responses, the channel falls through to the classic path and behavior
    // is unchanged. Those tests therefore double as the "code-mode-error →
    // classic fallback" coverage for the stub-fallback path (the TODO(I7)
    // arm). The new tests below cover the OTHER half: a code-mode response
    // that does carry a fenced TS block runs through the Deno sandbox and
    // lands an `actions` row with `mode='code'`, `generatedSource`, and
    // `toolCallTrace` populated.

    /// `true` iff `deno --version` succeeds (resolved via the same env-var
    /// convention as the runner itself). Returning `false` here makes the
    /// code-mode tests print a notice and exit clean instead of failing on
    /// a dev box without Deno installed.
    fn deno_available_for_tests() -> bool {
        let bin = std::env::var("AUGMENTAGENT_DENO_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "deno".to_string());
        std::process::Command::new(&bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn code_mode_dry_run_lands_action_row_with_mode_code() {
        if !deno_available_for_tests() {
            eprintln!(
                "skipping code_mode_dry_run_lands_action_row_with_mode_code: \
                 `deno` not on PATH (set AUGMENTAGENT_DENO_BIN to override)"
            );
            return;
        }
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-cm-dryrun".into(),
                thread_id: Some("t-cm".into()),
                from: "user@example.com".into(),
                subject: "Quick q".into(),
                body: "any update?".into(),
                date: "2026-05-22".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // First response: triage → reply. Second response: a *fenced* TS
        // program → code-mode extracts it, run_program executes it, the
        // dispatcher writes the actions row.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            // Plain JS body inside a ```ts fence — `extract_ts_block`
            // matches the language tag, while the Deno runner uses indirect
            // eval (no TS-stripping), so the program itself must not carry
            // TypeScript type annotations.
            "```ts\n\
             async function main() {\n\
               await tools.draft(\"gmail\", \"thanks — shipping today\", \"answer the question\");\n\
             }\n\
             main();\n\
             ```\n",
        ]));
        let ch = GmailChannel::dry_run(
            store.clone(),
            gmail,
            reasoner,
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        // Code-mode succeeded → DryRun outcome.
        assert_eq!(out.replied_dry_run, 1, "expected 1 dry-run reply");
        assert_eq!(out.errors, 0);

        // Find the action that was just landed for this message. The dry-run
        // promotion path updates the dispatcher's Pending row to DryRun, so
        // we read it back by messageId.
        let actions = store
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT id, mode, generatedSource, toolCallTrace, draftBody, status \
                     FROM actions WHERE messageId = 'm-cm-dryrun'",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(actions.len(), 1, "expected exactly one actions row");
        let (_, mode, src, trace, body, status) = &actions[0];
        assert_eq!(mode.as_deref(), Some("code"), "mode must be 'code'");
        assert!(
            src.as_deref()
                .map(|s| s.contains("tools.draft"))
                .unwrap_or(false),
            "generatedSource must contain the program; got {src:?}"
        );
        let trace_str = trace.as_deref().unwrap_or("");
        assert!(
            trace_str.contains("\"call\":\"draft\""),
            "toolCallTrace must include the draft call; got {trace_str:?}"
        );
        assert_eq!(
            body.as_deref(),
            Some("thanks — shipping today"),
            "draftBody must match what tools.draft passed"
        );
        assert_eq!(status, "dry_run", "dry-run mode must update status");
    }

    #[tokio::test]
    async fn code_mode_failure_falls_through_to_classic_path() {
        // No Deno needed: code-mode response has no fenced block, repair
        // retry also fails, classic kicks in. Verifies the I7 wiring lands a
        // `mode='classic'` row, files a `code-mode-failure` gh issue (via
        // the mock runner), and posts a Discord notice with the issue
        // number. Also confirms the I7 helper does NOT break the
        // pre-existing reply pipeline (approval card still gets posted).
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-cm-fallback".into(),
                thread_id: Some("t-cm-fb".into()),
                from: "user@example.com".into(),
                subject: "Ping".into(),
                body: "u there?".into(),
                date: "2026-05-22".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Triage → reply. Two no-fence code-mode responses (initial + repair
        // retry both fail). Fourth response feeds the classic prose-draft
        // call.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"ping"}"#,
            "no fenced block here, just prose",
            "still no fenced block — repair gave up",
            "Yes — shipping today.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let gh = Arc::new(RecordingGh::new(101));
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
        )
        .with_gh_issue_runner(gh.clone());
        let out = ch.poll_once().await.unwrap();
        // Classic path produced a draft → approval card posted.
        assert_eq!(out.awaiting_approval, 1);
        assert_eq!(out.errors, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        // Action row landed with mode='classic'. `generatedSource` carries
        // the empty original source (the first failure was a NoCodeBlock
        // before any source materialised) — we just check the column exists.
        let mode: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT mode FROM actions WHERE messageId = 'm-cm-fallback'",
                    [],
                    |r| r.get(0),
                )
                .or(Ok(None))
            })
            .unwrap();
        assert_eq!(
            mode.as_deref(),
            Some("classic"),
            "fallback path must land mode='classic'"
        );
        // I7: exactly one gh issue filed with the right label + title prefix.
        let calls = gh.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one gh issue should be filed");
        let (title, body, labels) = &calls[0];
        assert!(title.starts_with("[code-mode]"));
        assert!(body.contains("## Postmortem"));
        assert!(body.contains("**Final draft mode:** classic"));
        assert!(body.contains("**Channel:** gmail"));
        assert_eq!(labels, &vec!["code-mode-failure".to_string()]);
        // I7: Discord notice posted with the issue number (#101).
        let notices = broker.flag_posts.lock().unwrap();
        assert_eq!(notices.len(), 1, "exactly one Discord notice should fire");
        assert!(notices[0].1.contains("#101"));
        assert!(notices[0].1.contains("classic"));
    }

    /// Successful self-repair: original program has no fence → repair
    /// returns a valid fenced TS block → the Deno sandbox runs it cleanly →
    /// the row carries `mode='code'` AND a `repair_used: true` audit
    /// marker. NO gh issue is filed.
    #[tokio::test]
    async fn code_mode_self_repair_success_no_issue_filed() {
        if !deno_available_for_tests() {
            eprintln!(
                "skipping code_mode_self_repair_success_no_issue_filed: \
                 `deno` not on PATH (set AUGMENTAGENT_DENO_BIN to override)"
            );
            return;
        }
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-cm-repair-ok".into(),
                thread_id: Some("t-cm-r".into()),
                from: "user@example.com".into(),
                subject: "Re: postmortem".into(),
                body: "still good?".into(),
                date: "2026-05-22".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Triage → reply. Code-mode initial: no fence → fails. Repair: a
        // valid fenced TS program → executes cleanly. Classic call never
        // reached.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            "no fence — bug forced",
            "```ts\n\
             async function main() {\n\
               await tools.draft(\"gmail\", \"fixed by repair\", \"answer\");\n\
             }\n\
             main();\n\
             ```\n",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let gh = Arc::new(RecordingGh::new(900));
        let ch = GmailChannel::dry_run(
            store.clone(),
            gmail,
            reasoner,
            GmailChannelConfig {
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        )
        .with_gh_issue_runner(gh.clone());
        let _ = ch.approvals; // dry_run uses NoopBroker; broker var unused here
        let _ = broker;
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.replied_dry_run, 1, "repair must produce a dry-run reply");
        assert_eq!(out.errors, 0);
        // Action row: mode='code' (repair lands code-mode), trace carries
        // the repair_used marker, body is what the repaired program drafted.
        let (mode, trace, body): (Option<String>, Option<String>, Option<String>) = store
            .with_conn(|c| {
                let row = c.query_row(
                    "SELECT mode, toolCallTrace, draftBody FROM actions WHERE messageId = 'm-cm-repair-ok'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                Ok(row)
            })
            .unwrap();
        assert_eq!(mode.as_deref(), Some("code"));
        let trace_str = trace.unwrap_or_default();
        assert!(
            trace_str.contains("\"repair_used\":true"),
            "trace must carry repair_used marker; got {trace_str}"
        );
        assert_eq!(body.as_deref(), Some("fixed by repair"));
        // CRUCIAL: no gh issue was filed for the successful repair.
        assert_eq!(
            gh.calls.lock().unwrap().len(),
            0,
            "successful repair must not file a gh issue"
        );
    }

    /// Failure with Deno available: forced repair-also-fails program causes
    /// classic fallback. Verifies `generatedSource` on the classic row
    /// carries the ORIGINAL (failed) program text for audit.
    #[tokio::test]
    async fn code_mode_repair_runtime_error_falls_through_to_classic() {
        if !deno_available_for_tests() {
            eprintln!(
                "skipping code_mode_repair_runtime_error_falls_through_to_classic: \
                 `deno` not on PATH"
            );
            return;
        }
        let (store, _f) = tmp_store();
        let gmail = Arc::new(StubGmail {
            emails: vec![Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: "m-cm-repair-fail".into(),
                thread_id: Some("t-cm-rf".into()),
                from: "user@example.com".into(),
                subject: "Quick q".into(),
                body: "thoughts?".into(),
                date: "2026-05-22".into(),
                account_entity_id: Some("acc1".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            }],
        });
        // Original: a fenced program that throws. Repair: a fenced program
        // that ALSO throws. Classic call: a plain prose draft.
        let throws_original =
            "```ts\nasync function main() { throw new Error('original boom'); }\nmain();\n```";
        let throws_repair =
            "```ts\nasync function main() { throw new Error('repair boom'); }\nmain();\n```";
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            throws_original,
            throws_repair,
            "Got it — classic kicked in.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let gh = Arc::new(RecordingGh::new(500));
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
        )
        .with_gh_issue_runner(gh.clone());
        let out = ch.poll_once().await.unwrap();
        // Approval card posted via classic path.
        assert_eq!(out.awaiting_approval, 1, "classic path posts approval");
        assert_eq!(out.errors, 0);
        // Row: mode='classic', generatedSource = original failed program.
        let (mode, gen_src): (Option<String>, Option<String>) = store
            .with_conn(|c| {
                let row = c.query_row(
                    "SELECT mode, generatedSource FROM actions \
                     WHERE messageId = 'm-cm-repair-fail'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                Ok(row)
            })
            .unwrap();
        assert_eq!(mode.as_deref(), Some("classic"));
        let src = gen_src.unwrap_or_default();
        assert!(
            src.contains("original boom"),
            "generatedSource must carry the ORIGINAL failed program; got {src}"
        );
        assert!(
            !src.contains("repair boom"),
            "generatedSource must NOT contain the repaired program; got {src}"
        );
        // gh issue + Discord notice both fired.
        assert_eq!(gh.calls.lock().unwrap().len(), 1);
        let calls = gh.calls.lock().unwrap();
        let (_, body, _) = &calls[0];
        assert!(body.contains("**Repair attempted:** yes"));
        assert!(body.contains("**Final draft mode:** classic"));
        let flag_notices = broker.flag_posts.lock().unwrap();
        assert_eq!(flag_notices.len(), 1);
        assert!(flag_notices[0].1.contains("#500"));
    }
}
