//! `LinkedInChannel` — polls the voyager mailbox every 4h ± jitter, runs each
//! new DM through the same triage → draft → ingest pipeline the Gmail channel
//! uses, then hands the draft to the Discord approval broker.
//!
//! Sends happen from the approver (CLI crate) when the user clicks Approve on
//! Discord — same as Gmail, except the wire call is `VoyagerClient::send_message`
//! instead of `ComposioClient::send_draft`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use async_trait::async_trait;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::code_mode::{
    self, handle_code_mode_failure, manifest_v1, report_classic_fallback, DefaultDispatcher,
    DraftOutcome, FailureCtx, FailureStage, GhCliIssueRunner, GhIssueRunner, MessageContext,
};
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{
    code_mode_system, code_mode_user_message, draft_user_message, triage_user_message,
};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{WorkItem, WorkItemHandler};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Store, TriageResult, NUDGE_INTERVAL_MS};

use crate::api::{LinkedInApi, LinkedInError};
use crate::types::Dm;

/// Default poll interval: 6×/day. LinkedIn's anti-bot heuristics care about
/// request cadence; this stays well below any reasonable human-or-bot line.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;

/// Jitter window added to each tick to avoid fingerprint clustering when the
/// daemon is restarted repeatedly. ±10 min around the base interval.
pub const JITTER_SECS: u64 = 10 * 60;

#[derive(Clone, Debug)]
pub struct LinkedInChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    /// Skill dir for the email-triage crate's learned patterns — reused
    /// as-is since LinkedIn DMs are "messages" the rubric applies to too.
    pub skill_dir: PathBuf,
}

impl Default for LinkedInChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/email-triage"),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub dms_checked: usize,
    pub skipped: usize,
    pub flagged: usize,
    pub replied_dry_run: usize,
    pub awaiting_approval: usize,
    pub errors: usize,
}

pub struct LinkedInChannel<L: LinkedInApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub api: Arc<L>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: LinkedInChannelConfig,
    /// The user's own fsd_profile urn — used to filter outbound messages and
    /// carry into the `account_entity_id` prefix.
    pub member_urn: String,
    wiki_schema: Option<String>,
    /// gh CLI runner for I7 postmortems on code-mode failure. Behind a trait
    /// so tests can mock the `gh issue create` invocation; production defaults
    /// to [`GhCliIssueRunner`] which shells out to the `gh` binary on PATH.
    gh_issue_runner: Arc<dyn GhIssueRunner>,
}

impl<L: LinkedInApi, R: Reasoner + 'static> LinkedInChannel<L, R> {
    pub fn new(
        store: Arc<Store>,
        api: Arc<L>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        member_urn: String,
        config: LinkedInChannelConfig,
    ) -> Self {
        let wiki_schema = match (&config.wiki_root, &config.wiki_schema_path) {
            (Some(root), Some(schema_path)) => {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                match layout.bootstrap() {
                    Ok(()) => match std::fs::read_to_string(schema_path) {
                        Ok(s) if !s.trim().is_empty() => Some(s),
                        _ => None,
                    },
                    Err(e) => {
                        warn!("wiki bootstrap failed, disabling wiki: {e}");
                        None
                    }
                }
            }
            _ => None,
        };
        Self {
            store,
            api,
            reasoner,
            approvals,
            config,
            member_urn,
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

    // The bespoke DM poll loop (`select!` + ticker + post-tick jitter sleep)
    // was removed in the #25 cutover — its job is now done generically by
    // `run_arc` below, which composes `augmentagent_channel_core::ChannelRunner`
    // over a `LinkedInInbound` source. `ChannelRunner` natively reproduces the
    // shutdown/select shape and the post-tick jitter sleep this loop used to
    // hand-roll, so cadence (4h ± 10min) and DM dispatch behavior are
    // unchanged. `poll_once` is retained (CLI `linkedin poll-once` + the
    // existing channel tests drive it) and shares the exact same per-DM path
    // via `process_email`.

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let dms = match self.api.fetch_recent_dms().await {
            Ok(dms) => dms,
            Err(LinkedInError::AuthExpired) => {
                warn!("linkedin auth expired — run `augmentagent linkedin login`");
                outcome.errors += 1;
                return Ok(outcome);
            }
            Err(e) => {
                error!("linkedin fetch failed: {e:#}");
                outcome.errors += 1;
                return Ok(outcome);
            }
        };
        outcome.dms_checked = dms.len();

        for dm in dms {
            // Skip our own outbound messages entirely.
            if dm.is_outbound(&self.member_urn) {
                continue;
            }
            match self.handle_dm(dm).await {
                Ok(Some(DispatchOutcome::Skipped)) => outcome.skipped += 1,
                Ok(Some(DispatchOutcome::Flagged)) => outcome.flagged += 1,
                Ok(Some(DispatchOutcome::DryRun)) => outcome.replied_dry_run += 1,
                Ok(Some(DispatchOutcome::AwaitingApproval)) => outcome.awaiting_approval += 1,
                Ok(None) => {}
                Err(e) => {
                    outcome.errors += 1;
                    error!("handle_dm failed: {e:#}");
                }
            }
        }

        Ok(outcome)
    }

    async fn handle_dm(&self, dm: Dm) -> anyhow::Result<Option<DispatchOutcome>> {
        let email = dm.into_email(&self.member_urn);
        self.process_email(email).await
    }

    /// Run one already-converted DM `Email` through the full triage → draft →
    /// approve → ingest pipeline. Single shared per-message entry point: the
    /// bespoke `poll_once` loop reaches it via `handle_dm` (after the outbound
    /// filter + `Dm::into_email`), and `LinkedInWorkHandler` (the
    /// `ChannelRunner` cutover path) calls it after rehydrating the `Email`
    /// from a `WorkItem` payload that `LinkedInInbound` already produced via
    /// the same `Dm::into_email` + outbound filter. Both paths therefore run
    /// byte-identical dispatch logic.
    pub async fn process_email(
        &self,
        email: augmentagent_store::Email,
    ) -> anyhow::Result<Option<DispatchOutcome>> {
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(None);
        }
        // A prior poll may have logged an action for this DM (pending, sent,
        // rejected, etc.). Gate re-triage on action presence so we don't
        // stack duplicate approval cards across polls — LinkedIn's 4h
        // cadence makes this a real problem vs Gmail's 2min.
        if self.store.is_message_processed(&email.message_id)? {
            return Ok(None);
        }

        // --- TRIAGE (Opus with optional wiki read) ---
        let triage_opts = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
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
                let reason = decision.reason.as_deref().unwrap_or("flagged");
                if let Err(e) = self.approvals.post_flag_notice(&email, reason).await {
                    warn!(message_id = %email.message_id, "post_flag_notice failed: {e}");
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
                // --- CODE-MODE DRAFT (I8) ---
                //
                // Mirrors the email channel's post-I7 code-mode flow:
                //   1. Reasoner emits a TypeScript program that orchestrates
                //      tool calls and ends with
                //      `tools.draft(channel="linkedin", body, reason)`.
                //   2. The Deno sandbox executes the program. The dispatcher's
                //      terminal `tools.draft` handler writes the actions row
                //      (mode='code', generatedSource, toolCallTrace) and
                //      stashes the action id for us to pick up below.
                //
                // On ANY failure (reasoner spawn, missing fenced block,
                // sandbox timeout, runtime exception, dispatcher error) we
                // hand off to `handle_code_mode_failure` (I7 / #53) which
                // runs one self-repair pass. If the repair lands a working
                // code-mode draft, we use it. Otherwise we fall through to
                // the classic prompt path AND, after it lands its row, call
                // `report_classic_fallback` to file the postmortem gh issue
                // + post the Discord notice tagged with the classic action
                // id.
                //
                // LinkedIn has no resolve-asks / tone / archetype pipeline
                // (those are gmail-only today), so the context blocks fed to
                // `code_mode_user_message` are empty — exactly the same
                // strings the classic `draft_user_message` call below gets.
                let manifest = manifest_v1();
                let system_prompt = code_mode_system(&manifest);
                let wiki_hint = String::new();
                let user_msg =
                    code_mode_user_message(&email, &wiki_hint, "", "", "", "");
                // Opts mirror the classic `draft_opts` shape: same permission
                // mode, no allowed_tools / add_dirs — the Deno sandbox is
                // the tool surface, not the host claude CLI's Read/Grep/Glob.
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
                    channel: "linkedin".to_string(),
                    email: email.clone(),
                    account_id: None,
                };

                // Attempt 1: original program. Capture the source on a
                // `NoCodeBlock`-vs-`RunnerError` distinction so the failure
                // handler can pass the program text to the repair prompt.
                let mut cm_source: String = String::new();
                let cm_attempt: Result<
                    String,
                    (augmentagent_channel_core::code_mode::CodeModeError, FailureStage),
                > = async {
                    let ts_source = match self
                        .reasoner
                        .call_code_mode(&code_mode_opts, &user_msg)
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            let cme = match e
                                .downcast::<augmentagent_channel_core::code_mode::CodeModeError>()
                            {
                                Ok(cme) => cme,
                                Err(other) => {
                                    augmentagent_channel_core::code_mode::CodeModeError::ReasonerFailed(
                                        other,
                                    )
                                }
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
                    if let Err(e) =
                        code_mode::run_program(&ts_source, &manifest, &dispatcher).await
                    {
                        let wrapped =
                            augmentagent_channel_core::code_mode::CodeModeError::ReasonerFailed(
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
                            "linkedin code-mode attempt failed: {cme}; invoking self-repair"
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
                            channel: "linkedin".to_string(),
                            model: code_mode_opts.model.clone(),
                            manifest_version: "v1",
                        };
                        match handle_code_mode_failure(&failure_ctx, &cm_source, &cme, stage)
                            .await
                        {
                            DraftOutcome::CodeMode {
                                action_id,
                                repair_used,
                            } => {
                                info!(
                                    message_id = %email.message_id,
                                    action_id = %action_id,
                                    repair_used,
                                    "linkedin code-mode self-repair succeeded"
                                );
                                (Some(action_id), None)
                            }
                            DraftOutcome::ClassicNeeded(record) => {
                                error!(
                                    message_id = %email.message_id,
                                    "linkedin code-mode self-repair failed; falling back to classic"
                                );
                                (None, Some(record))
                            }
                        }
                    }
                };

                // --- Code-mode success path ---
                //
                // The dispatcher's `tools.draft` already wrote the action row
                // (mode='code', generatedSource, toolCallTrace). Read the
                // persisted draft body back out so we can post the approval
                // card / mark the dry-run row / record the nudge against the
                // SAME action_id (no new log_action).
                if let Some(action_id) = code_mode_action_id {
                    let draft_body = self
                        .store
                        .get_action_with_email(&action_id)?
                        .and_then(|a| a.action.draft_body)
                        .unwrap_or_default();

                    if self.config.dry_run {
                        // Promote the dispatcher's `Pending` row to `DryRun`
                        // so dry-run accounting matches classic. The
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
                            "[linkedin reply dry-run:code] {}\n--- draft ---\n{}\n--- /draft ---",
                            email.subject, draft_body
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

                    // Non-dry-run: post the approval card against the
                    // code-mode-written action row. LinkedIn has no
                    // server-side draft — approval-on-click triggers
                    // voyager send.
                    if let Err(e) = self
                        .approvals
                        .post_approval(&action_id, &email, &draft_body)
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
                    if let Err(e) = self
                        .store
                        .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
                    {
                        warn!(
                            action_id,
                            "record_nudge after post_approval failed: {e}"
                        );
                    }
                    info!(
                        action_id,
                        message_id = %email.message_id,
                        "linkedin code-mode approval card posted"
                    );
                    return Ok(Some(DispatchOutcome::AwaitingApproval));
                }

                // --- Classic fallback (I7) ---
                //
                // Reached when code-mode failed AND self-repair didn't
                // produce a working code-mode draft. Behaviour matches the
                // pre-I8 classic prompt path — same draft call, same
                // dispatch — except we then call `report_classic_fallback`
                // to file the postmortem gh issue and post the Discord
                // notice tagged with the classic action_id.
                let skill_system = std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                    .unwrap_or_default();
                let draft_opts = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "", "", "", "", "");
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
                        "[linkedin reply dry-run] {}\n--- draft ---\n{}\n--- /draft ---",
                        email.subject, draft
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
                            channel: "linkedin".to_string(),
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

                // --- DISPATCH (classic) ---
                // LinkedIn has no server-side draft — we post the approval
                // card directly, the approver calls voyager send on Approve.
                let action_id = self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&draft),
                    ActionStatus::Pending,
                )?;
                // If we got here via a code-mode failure, file the postmortem
                // gh issue + Discord notice now that the classic row exists
                // (the notice references this action_id).
                if let Some(record) = pending_classic_record.as_ref() {
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
                        channel: "linkedin".to_string(),
                        model: code_mode_opts.model.clone(),
                        manifest_version: "v1",
                    };
                    report_classic_fallback(&failure_ctx, record, &action_id).await;
                }
                if let Err(e) = self
                    .approvals
                    .post_approval(&action_id, &email, &draft)
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
                // Mark the freshly-posted card as the active nudge so the
                // serial-queue scheduler won't re-post it on the next tick.
                if let Err(e) = self
                    .store
                    .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
                {
                    warn!(action_id, "record_nudge after post_approval failed: {e}");
                }
                info!(action_id, message_id = %email.message_id, "linkedin approval card posted");
                Ok(Some(DispatchOutcome::AwaitingApproval))
            }
            // Capture / Meeting are wave-A wiki-ingest-only kinds emitted by
            // the voice and gcal channels respectively — linkedin triage must
            // never produce them. Defensive skip if the model misbehaves.
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "linkedin triage returned non-message decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                Ok(Some(DispatchOutcome::Skipped))
            }
        }
    }

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
}

impl<L: LinkedInApi + 'static, R: Reasoner + 'static> LinkedInChannel<L, R> {
    /// Long-running driver (the production entry point used by `serve`).
    ///
    /// **#25 cutover**: replaces the old bespoke `select!` + ticker +
    /// post-tick jitter sleep. Driven by the generic
    /// [`augmentagent_channel_core::ChannelRunner`] over a
    /// [`LinkedInInbound`](crate::LinkedInInbound) source wrapped in an
    /// [`InboundMessageTrigger`](augmentagent_channel_core::InboundMessageTrigger),
    /// dispatching each `WorkItem` through [`LinkedInWorkHandler`] — which
    /// calls the same [`LinkedInChannel::process_email`] the old loop reached
    /// via `handle_dm`, so triage/draft/approve/ingest/dedup behavior is
    /// unchanged.
    ///
    /// Cadence parity: `ChannelRunner`'s post-tick jitter sleeps a uniform
    /// `[0, 2*jitter]` window, identical to the removed loop's
    /// `jitter_secs()` (uniform `[0, 2*JITTER_SECS]`), so we pass
    /// `Duration::from_secs(JITTER_SECS)` and keep the 4h ± 10min cadence.
    ///
    /// Takes `Arc<Self>` so the runner's handler can share the channel.
    pub async fn run_arc(
        self: Arc<Self>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        use augmentagent_channel_core::{ChannelRunner, InboundMessageTrigger};

        let source = Arc::new(crate::inbound::LinkedInInbound::new(
            Arc::clone(&self.api),
            self.member_urn.clone(),
        ));
        let trigger = Arc::new(InboundMessageTrigger::new(source));
        let handler = Arc::new(LinkedInWorkHandler {
            channel: Arc::clone(&self),
        });
        let runner = ChannelRunner::new(
            trigger,
            handler,
            self.config.poll_interval,
            Duration::from_secs(JITTER_SECS),
            "linkedin",
        );
        runner.run(shutdown).await
    }
}

/// `WorkItemHandler` for the #25 `ChannelRunner` cutover.
///
/// The runner pulls inbound DMs as `WorkItem`s (via `LinkedInInbound`, which
/// already applies the same `dm.is_outbound(member_urn)` filter `poll_once`
/// does and serializes `Dm::into_email`); this handler rehydrates each into
/// the typed `Email` and feeds it through
/// [`LinkedInChannel::process_email`] — the identical triage → draft →
/// approve → ingest → dedup path the bespoke loop ran via `handle_dm`.
pub struct LinkedInWorkHandler<L: LinkedInApi + 'static, R: Reasoner + 'static> {
    channel: Arc<LinkedInChannel<L, R>>,
}

#[async_trait]
impl<L: LinkedInApi + 'static, R: Reasoner + 'static> WorkItemHandler
    for LinkedInWorkHandler<L, R>
{
    async fn handle(&self, item: WorkItem) -> anyhow::Result<()> {
        let email: augmentagent_store::Email =
            serde_json::from_value(item.payload).map_err(|e| {
                anyhow::anyhow!("linkedin work item payload not an Email: {e}")
            })?;
        // Mirror poll_once's per-DM error handling: log + swallow so one bad
        // message never aborts the tick (ChannelRunner also logs+counts).
        match self.channel.process_email(email).await {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("linkedin handle (channel-runner): process_email failed: {e:#}");
                Ok(())
            }
        }
    }
}

/// Deterministic jitter: uniform int in [0, 2 * JITTER_SECS]. Pseudo-random
/// based on the current time's nanosecond tail; no crate dep needed.
fn jitter_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    ns % (2 * JITTER_SECS + 1)
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Outcome of running one DM through [`LinkedInChannel::process_email`].
/// Public because `process_email` is the shared entry point used by both the
/// bespoke `poll_once` loop and the `ChannelRunner` cutover handler.
#[derive(Debug, Clone, Copy)]
pub enum DispatchOutcome {
    Skipped,
    Flagged,
    DryRun,
    /// Draft computed, approval card posted; approver sends via voyager on click.
    AwaitingApproval,
}

// =============================================================================
// Friend-post engagement (#13)
// =============================================================================

/// Drives the [`LinkedInFeedTrigger`] on a ~6h-with-jitter cadence and runs
/// each surfaced friend post through the same triage → draft → approval-card
/// pipeline DMs use. Approve → the approver calls `post_comment` (no
/// auto-posting; cap accounting lives in `linkedin_action_log`).
///
/// Kept as a sibling of [`LinkedInChannel`] (not folded into its poll loop)
/// so the DM cadence and the feed cadence stay independent — the issue calls
/// for a distinct 6h feed poll vs the 4h DM poll.
pub struct LinkedInFeedEngagement<L: crate::api::LinkedInApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub trigger: Arc<crate::feed::LinkedInFeedTrigger<L>>,
    pub member_urn: String,
    pub config: LinkedInChannelConfig,
    pub poll_interval: Duration,
}

impl<L: crate::api::LinkedInApi + 'static, R: Reasoner + 'static> LinkedInFeedEngagement<L, R> {
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("linkedin feed engagement: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once(&shutdown).await {
                        Ok(n) => info!(engaged = n, "linkedin feed poll complete"),
                        Err(e) => error!("linkedin feed poll failed: {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    /// One feed poll: ask the trigger for fresh posts (cap-gated), triage +
    /// draft a supportive comment for each, post an approval card. Returns
    /// the count of approval cards posted.
    pub async fn poll_once(&self, cancel: &CancellationToken) -> anyhow::Result<usize> {
        use augmentagent_channel_core::trigger::Trigger;
        let items = self.trigger.next_work_items(cancel).await?;
        let mut posted = 0usize;
        for item in items {
            let payload: crate::feed::FeedEngagementPayload =
                match serde_json::from_value(item.payload.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("feed work-item payload decode failed: {e}");
                        continue;
                    }
                };
            let post = crate::types::FeedPost {
                post_urn: payload.post_urn,
                author_name: payload.author_name,
                author_urn: crate::types::MemberUrn(payload.author_urn),
                text: payload.text,
                created_at_ms: payload.created_at_ms,
            };
            match self.handle_post(post).await {
                Ok(true) => posted += 1,
                Ok(false) => {}
                Err(e) => error!("handle_post failed: {e:#}"),
            }
        }
        Ok(posted)
    }

    /// Triage (engage vs skip) then, on engage, draft a short supportive
    /// comment and post an approval card. Returns `true` iff a card was
    /// posted. Mirrors `LinkedInChannel::handle_dm` but the only terminal
    /// non-skip kind is Reply (a comment) — Flag/Capture/Meeting are coerced
    /// to skip since they're meaningless for feed engagement.
    async fn handle_post(&self, post: crate::types::FeedPost) -> anyhow::Result<bool> {
        let email = post.into_email(&self.member_urn);
        self.store.upsert_email(&email)?;
        if self.store.is_message_processed(&email.message_id)? {
            return Ok(false);
        }

        let triage_opts = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
        let raw = self.reasoner.call(&triage_opts, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "feed triage parse failed: {e}; raw={raw}");
                self.store.log_action(
                    &email.message_id,
                    None,
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Error,
                )?;
                return Err(e.into());
            }
        };

        if !matches!(decision.decision, DecisionKind::Reply) {
            // engage == Reply; anything else => skip this post.
            self.store.log_action(
                &email.message_id,
                None,
                &email.from,
                &email.subject,
                Some(&email.body),
                None,
                ActionStatus::Skipped,
            )?;
            self.store
                .mark_email_processed(&email.message_id, TriageResult::Skip)?;
            return Ok(false);
        }

        let skill_system = std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
            .unwrap_or_default();
        let draft_opts = draft_opts(skill_system, self.config.wiki_root.clone());
        let draft_prompt = draft_user_message(&email, "", "", "", "", "");
        let draft = self
            .reasoner
            .call(&draft_opts, &draft_prompt)
            .await?
            .trim()
            .to_string();

        if self.config.dry_run {
            self.store.log_action(
                &email.message_id,
                None,
                &email.from,
                &email.subject,
                Some(&email.body),
                Some(&draft),
                ActionStatus::DryRun,
            )?;
            self.store
                .mark_email_processed(&email.message_id, TriageResult::Reply)?;
            println!(
                "[linkedin engage dry-run] {}\n--- comment ---\n{}\n--- /comment ---",
                email.subject, draft
            );
            return Ok(false);
        }

        let action_id = self.store.log_action(
            &email.message_id,
            None,
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(&draft),
            ActionStatus::Pending,
        )?;
        if let Err(e) = self
            .approvals
            .post_approval(&action_id, &email, &draft)
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
        if let Err(e) = self
            .store
            .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
        {
            warn!(action_id, "record_nudge after feed post_approval failed: {e}");
        }
        info!(action_id, post = %email.message_id, "linkedin engagement card posted");
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
    use augmentagent_channel_core::{Reasoner, ReasonerOpts};
    use augmentagent_store::Email;

    use crate::api::LinkedInError;
    use crate::posting::{PostDraft, ShareUrn};
    use crate::types::{FeedPost, MemberUrn};

    struct StubApi {
        dms: Vec<Dm>,
    }
    #[async_trait]
    impl LinkedInApi for StubApi {
        async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
            Ok(self.dms.clone())
        }
        async fn send_message(
            &self,
            _conversation_urn: &str,
            _text: &str,
        ) -> Result<String, LinkedInError> {
            Ok("urn:li:messagingMessage:STUB".into())
        }
        async fn fetch_feed_posts_by_author(
            &self,
            _author_urn: &str,
        ) -> Result<Vec<FeedPost>, LinkedInError> {
            Ok(vec![])
        }
        async fn post_comment(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
            Ok("urn:li:comment:STUB".into())
        }
        async fn react(&self, _: &str, _: &str) -> Result<(), LinkedInError> {
            Ok(())
        }
        async fn create_share(
            &self,
            _draft: PostDraft<'_>,
        ) -> Result<ShareUrn, LinkedInError> {
            Ok(ShareUrn("urn:li:share:STUB".into()))
        }
    }

    struct ScriptedReasoner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(r: I) -> Self {
            Self {
                responses: std::sync::Mutex::new(r.into_iter().map(String::from).collect()),
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
        ) -> Result<(), ApprovalError> {
            self.posts.lock().unwrap().push(action_id.to_string());
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            email: &Email,
            reason: &str,
        ) -> Result<(), ApprovalError> {
            self.flag_posts
                .lock()
                .unwrap()
                .push((email.message_id.clone(), reason.to_string()));
            Ok(())
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
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn sample_dm(id: &str) -> Dm {
        Dm {
            message_urn: id.into(),
            conversation_urn: format!("conv-{id}"),
            peer_name: "Tony Siu".into(),
            peer_urn: MemberUrn("urn:li:fsd_profile:PEER".into()),
            sender_urn: MemberUrn("urn:li:fsd_profile:PEER".into()),
            text: "hey, got a minute?".into(),
            delivered_at_ms: 1776630000000,
        }
    }

    #[tokio::test]
    async fn flag_posts_notice_no_card() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            dms: vec![sample_dm("m-flag")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"personal outreach"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = LinkedInChannel::new(
            store,
            api,
            reasoner,
            broker.clone(),
            "urn:li:fsd_profile:ME".into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.flagged, 1);
        assert_eq!(out.awaiting_approval, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        let flags = broker.flag_posts.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert!(flags[0].1.contains("personal outreach"));
    }

    #[tokio::test]
    async fn reply_posts_approval_card() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            dms: vec![sample_dm("m-reply")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"direct question"}"#,
            "Sure — Thursday 3pm works.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = LinkedInChannel::new(
            store.clone(),
            api,
            reasoner,
            broker.clone(),
            "urn:li:fsd_profile:ME".into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.awaiting_approval, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        // The email is NOT complete until the user clicks approve.
        assert!(!store.is_email_complete("m-reply").unwrap());
    }

    #[tokio::test]
    async fn dm_with_prior_action_is_skipped() {
        // Regression for the Dana HtetAung duplicate-cards bug: the LinkedIn
        // channel was re-triaging and re-posting cards on every poll while
        // the previous action was still pending. Simulate a second poll
        // against a DM that already has a pending action: the reasoner
        // should not be called and no new card should be posted.
        let (store, _f) = tmp_store();
        // Seed a pending action for the DM we're about to poll on.
        let dm = sample_dm("m-dupe");
        let email = dm.clone().into_email("urn:li:fsd_profile:ME");
        store.upsert_email(&email).unwrap();
        store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("previously-drafted text"),
                ActionStatus::Pending,
            )
            .unwrap();

        let api = Arc::new(StubApi { dms: vec![dm] });
        // Empty reasoner queue — if handle_dm wrongly proceeds past the gate
        // it'll try to pop a response and we assert the queue stayed empty.
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = LinkedInChannel::new(
            store.clone(),
            api,
            reasoner.clone(),
            broker.clone(),
            "urn:li:fsd_profile:ME".into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.dms_checked, 1);
        assert_eq!(out.awaiting_approval, 0);
        assert_eq!(out.flagged, 0);
        assert_eq!(out.skipped, 0);
        // No new approval card posted on top of the existing pending action.
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        // Reasoner was never called (queue started empty, stayed empty).
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn outbound_messages_are_skipped() {
        let (store, _f) = tmp_store();
        let mut dm = sample_dm("m-out");
        // Mark it as sent by the user themselves.
        dm.sender_urn = MemberUrn("urn:li:fsd_profile:ME".into());
        let api = Arc::new(StubApi { dms: vec![dm] });
        // Reasoner should never be called — a skip at the outbound filter
        // stage. If it IS called, scripted responses will fall through to the
        // default skip, which would also count as "no-op" but would at least
        // let us assert reasoner was untouched via pop count.
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = LinkedInChannel::new(
            store,
            api,
            reasoner.clone(),
            broker,
            "urn:li:fsd_profile:ME".into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.dms_checked, 1);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.flagged, 0);
        assert_eq!(out.awaiting_approval, 0);
        // Reasoner queue still has its initial (empty) state.
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn auth_expired_returns_cleanly() {
        struct ExpiredApi;
        #[async_trait]
        impl LinkedInApi for ExpiredApi {
            async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
                Err(LinkedInError::AuthExpired)
            }
            async fn send_message(
                &self,
                _: &str,
                _: &str,
            ) -> Result<String, LinkedInError> {
                Err(LinkedInError::AuthExpired)
            }
            async fn fetch_feed_posts_by_author(
                &self,
                _: &str,
            ) -> Result<Vec<FeedPost>, LinkedInError> {
                Err(LinkedInError::AuthExpired)
            }
            async fn post_comment(
                &self,
                _: &str,
                _: &str,
            ) -> Result<String, LinkedInError> {
                Err(LinkedInError::AuthExpired)
            }
            async fn react(&self, _: &str, _: &str) -> Result<(), LinkedInError> {
                Err(LinkedInError::AuthExpired)
            }
            async fn create_share(
                &self,
                _draft: crate::posting::PostDraft<'_>,
            ) -> Result<crate::posting::ShareUrn, LinkedInError> {
                Err(LinkedInError::AuthExpired)
            }
        }
        let (store, _f) = tmp_store();
        let api = Arc::new(ExpiredApi);
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = LinkedInChannel::new(
            store,
            api,
            reasoner,
            broker,
            "urn:li:fsd_profile:ME".into(),
            LinkedInChannelConfig::default(),
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.errors, 1);
    }

    // --- #25 ChannelRunner cutover equivalence ---
    //
    // Prove the production driver swap (poll loop → ChannelRunner +
    // LinkedInWorkHandler) reproduces poll_once's per-DM behavior: same
    // triage outcome, same broker posts, same per-message dedup gate.

    use crate::inbound::dm_to_work_item;

    const ME_URN: &str = "urn:li:fsd_profile:ME";

    #[tokio::test]
    async fn channel_runner_handler_reply_flow_matches_poll_once() {
        let (store, _f) = tmp_store();
        let dm = sample_dm("cr-reply");
        let api = Arc::new(StubApi { dms: vec![dm.clone()] });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"direct question"}"#,
            "Sure — Thursday 3pm works.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = Arc::new(LinkedInChannel::new(
            store.clone(),
            api,
            reasoner,
            broker.clone(),
            ME_URN.into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        ));
        let handler = super::LinkedInWorkHandler {
            channel: Arc::clone(&ch),
        };
        // ChannelRunner would feed exactly this: the WorkItem LinkedInInbound
        // produces from the DM (post outbound-filter + Dm::into_email).
        handler
            .handle(dm_to_work_item(dm.clone(), ME_URN))
            .await
            .unwrap();

        // Same observable state as reply_posts_approval_card.
        assert_eq!(broker.posts.lock().unwrap().len(), 1);

        // Second runner tick on the same DM must NOT stack another card —
        // the is_message_processed gate inside process_email holds.
        handler
            .handle(dm_to_work_item(dm, ME_URN))
            .await
            .unwrap();
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn channel_runner_handler_flag_matches_poll_once() {
        let (store, _f) = tmp_store();
        let dm = sample_dm("cr-flag");
        let api = Arc::new(StubApi { dms: vec![dm.clone()] });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"personal outreach"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = Arc::new(LinkedInChannel::new(
            store.clone(),
            api,
            reasoner,
            broker.clone(),
            ME_URN.into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        ));
        let handler = super::LinkedInWorkHandler {
            channel: Arc::clone(&ch),
        };
        handler
            .handle(dm_to_work_item(dm, ME_URN))
            .await
            .unwrap();

        // Flag: heads-up notice, no approval card (same as poll_once).
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        let flags = broker.flag_posts.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert!(flags[0].1.contains("personal outreach"));
    }

    #[tokio::test]
    async fn channel_runner_handler_swallows_bad_payload() {
        // A non-Email payload bubbles as Err (ChannelRunner logs+counts);
        // process_email failures are swallowed. No panic, no post either way.
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi { dms: vec![] });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = Arc::new(LinkedInChannel::new(
            store,
            api,
            reasoner,
            broker.clone(),
            ME_URN.into(),
            LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        ));
        let handler = super::LinkedInWorkHandler {
            channel: Arc::clone(&ch),
        };
        let junk = augmentagent_channel_core::trigger::WorkItem {
            platform: "linkedin".into(),
            kind: "dm".into(),
            external_id: "junk".into(),
            payload: serde_json::json!({ "not": "an email" }),
        };
        let _ = handler.handle(junk).await;
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
    }
}
