//! `SlackChannel` — polls every active `channel_subscriptions` row with
//! `platform='slack'`, fetches messages since `last_seen_message_id`, and
//! dispatches each message by the subscription's mode.
//!
//! Multi-workspace aware: each tick refreshes the set of connected workspaces
//! from `slack_workspaces`, builds one `SlackClient` per workspace's Keychain
//! entry, and routes each subscription to the client matching its `account_id`
//! (= Slack `team_id`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

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
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{
    ActionStatus, ChannelSubscription, Email, SlackWorkspace, Store, SubscriptionMode,
    TriageResult, NUDGE_INTERVAL_MS,
};
use augmentagent_wiki::IdentityIndex;

use crate::api::{SlackClient, SlackError};
use crate::auth::SlackAuth;
use crate::types::SlackMessage;
use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// Per-workspace runtime handle: a `SlackClient` + the authenticated user id
/// used to skip our own outbound messages.
pub struct WorkspaceClient {
    pub team_id: String,
    pub client: Arc<SlackClient>,
    pub my_user_id: String,
}

/// Mirror LinkedIn + Discord's 4h cadence; Slack's API has very generous
/// rate limits so this is conservative.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;
pub const JITTER_SECS: u64 = 30 * 60;

pub const MAX_MESSAGES_PER_TICK: u32 = 200;

#[derive(Clone, Debug)]
pub struct SlackChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub skill_dir: PathBuf,
}

impl Default for SlackChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/slack-triage"),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub subscriptions_polled: usize,
    pub messages_seen: usize,
    pub priority_skipped: usize,
    pub priority_flagged: usize,
    pub priority_replied_dry_run: usize,
    pub priority_awaiting_approval: usize,
    pub digest_stored: usize,
    pub store_only_stored: usize,
    pub errors: usize,
}

pub struct SlackChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: SlackChannelConfig,
    pub identity_index: Option<Arc<IdentityIndex>>,
    wiki_schema: Option<String>,
    /// gh CLI runner for I7 postmortems on code-mode failures. Behind a trait
    /// so tests can swap in a recorder; production defaults to
    /// [`GhCliIssueRunner`] which shells out to the `gh` binary on PATH.
    gh_issue_runner: Arc<dyn GhIssueRunner>,
}

impl<R: Reasoner + 'static> SlackChannel<R> {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        config: SlackChannelConfig,
        identity_index: Option<Arc<IdentityIndex>>,
    ) -> Self {
        let wiki_schema = match (&config.wiki_root, &config.wiki_schema_path) {
            (Some(root), Some(schema_path)) => {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                match layout.bootstrap() {
                    Ok(()) => std::fs::read_to_string(schema_path)
                        .ok()
                        .filter(|s| !s.trim().is_empty()),
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
            reasoner,
            approvals,
            config,
            identity_index,
            wiki_schema,
            gh_issue_runner: Arc::new(GhCliIssueRunner::new()),
        }
    }

    /// Swap the gh-CLI runner used for I7 code-mode postmortem issues.
    /// Production callers don't need this; tests pass a recording stub so the
    /// suite never actually files issues on the real repo.
    pub fn with_gh_issue_runner(mut self, runner: Arc<dyn GhIssueRunner>) -> Self {
        self.gh_issue_runner = runner;
        self
    }

    /// Build a per-workspace client map from the active rows in
    /// `slack_workspaces`. Each row's Keychain slot is loaded; failures are
    /// logged but don't abort the tick — other workspaces can still poll.
    fn load_workspace_clients(&self) -> HashMap<String, WorkspaceClient> {
        let mut map = HashMap::new();
        let workspaces = match self.store.list_active_slack_workspaces() {
            Ok(w) => w,
            Err(e) => {
                error!("list_active_slack_workspaces failed: {e:#}");
                return map;
            }
        };
        if workspaces.is_empty() {
            // Legacy single-slot fallback: before multi-workspace shipped, one
            // `SlackAuth` lived at `augmentagent/slack/default`. Let existing
            // installs keep polling until they re-connect through the dashboard.
            if let Ok(auth) = SlackAuth::load_from_default_slot() {
                if let Some(ws) = build_workspace_client_from_auth(auth) {
                    map.insert(ws.team_id.clone(), ws);
                    info!("slack: using legacy default-slot auth (one workspace)");
                    return map;
                }
            }
            debug!("no active slack_workspaces rows — nothing to poll");
            return map;
        }
        for ws in workspaces {
            match load_workspace_client(&ws) {
                Some(handle) => {
                    map.insert(handle.team_id.clone(), handle);
                }
                None => {
                    warn!(team_id = %ws.team_id, "skipping workspace: auth not available");
                }
            }
        }
        map
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("slack channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "slack poll complete"),
                        Err(e) => error!("slack poll failed: {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let clients = self.load_workspace_clients();
        if clients.is_empty() {
            debug!("no slack workspaces loaded; nothing to poll");
            return Ok(outcome);
        }
        let subs = self.store.list_active_subscriptions(PLATFORM)?;
        outcome.subscriptions_polled = subs.len();
        if subs.is_empty() {
            debug!("no active slack subscriptions; nothing to poll");
            return Ok(outcome);
        }
        for sub in subs {
            let workspace = match resolve_workspace(&sub, &clients) {
                Some(w) => w,
                None => {
                    outcome.errors += 1;
                    warn!(
                        sub_id = %sub.id,
                        account_id = ?sub.account_id,
                        "no matching slack workspace loaded — skipping subscription"
                    );
                    continue;
                }
            };
            if let Err(e) = self.poll_subscription(&sub, workspace, &mut outcome).await {
                outcome.errors += 1;
                error!(sub_id = %sub.id, channel_id = %sub.channel_id, "slack subscription poll failed: {e:#}");
            }
        }
        Ok(outcome)
    }

    async fn poll_subscription(
        &self,
        sub: &ChannelSubscription,
        workspace: &WorkspaceClient,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let messages = match workspace
            .client
            .fetch_messages(
                &sub.channel_id,
                sub.last_seen_message_id.as_deref(),
                MAX_MESSAGES_PER_TICK,
            )
            .await
        {
            Ok(msgs) => msgs,
            Err(SlackError::Composio(msg)) if msg.contains("invalid_auth") => {
                warn!(team_id = %workspace.team_id, "slack auth invalid — reconnect via dashboard or `augmentagent slack login`");
                anyhow::bail!("invalid_auth");
            }
            Err(e) => return Err(e.into()),
        };

        // Slack returns newest-first; process oldest→newest so last_seen stays monotonic.
        let mut messages = messages;
        messages.reverse();
        outcome.messages_seen += messages.len();

        let mut newest_seen: Option<String> = None;
        for msg in messages {
            newest_seen = Some(msg.ts.clone());

            if msg.user.as_deref() == Some(workspace.my_user_id.as_str()) {
                continue;
            }
            if !msg.is_default_user_message() {
                continue;
            }
            if msg.text.is_empty() {
                continue;
            }

            if let Err(e) = self.handle_message(sub, workspace, msg, outcome).await {
                outcome.errors += 1;
                error!(sub_id = %sub.id, "handle_message failed: {e:#}");
            }
        }

        if let Some(newest) = newest_seen {
            self.store.update_last_seen_message(&sub.id, &newest)?;
        }
        Ok(())
    }

    async fn handle_message(
        &self,
        sub: &ChannelSubscription,
        workspace: &WorkspaceClient,
        msg: SlackMessage,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let email = message_to_email(&msg, sub, &workspace.my_user_id);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(());
        }

        match sub.mode {
            SubscriptionMode::Priority => self.handle_priority(email, outcome).await,
            SubscriptionMode::Digest => {
                outcome.digest_stored += 1;
                Ok(())
            }
            SubscriptionMode::StoreOnly => {
                outcome.store_only_stored += 1;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::DigestOnly)?;
                Ok(())
            }
        }
    }

    async fn handle_priority(
        &self,
        email: Email,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let wiki_hint = self.wiki_hint_for_sender(&email);

        let triage = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, &wiki_hint, "");
        let raw = self.reasoner.call(&triage, &triage_prompt).await?;
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
                outcome.priority_skipped += 1;
                Ok(())
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
                outcome.priority_flagged += 1;
                Ok(())
            }
            DecisionKind::Reply => {
                // --- CODE-MODE attempt (I13, mirroring I6/I7).
                //
                // Two-step flow:
                //   1. Reasoner emits a TypeScript program that orchestrates
                //      tool calls and ends with `tools.draft("slack", body, reason)`.
                //   2. The Deno sandbox executes the program. The dispatcher's
                //      terminal `tools.draft` handler writes the actions row
                //      (mode='code', generatedSource, toolCallTrace) and stashes
                //      the action id for us to pick up below.
                //
                // On ANY failure (reasoner spawn, missing fenced block, sandbox
                // timeout, runtime exception, dispatcher error) we hand off to
                // `handle_code_mode_failure` (I7) which runs one self-repair
                // pass. If the repair lands a working code-mode draft we use
                // it. Otherwise we fall through to the classic prompt path AND
                // call `report_classic_fallback` to file the postmortem gh
                // issue + post the Discord notice.
                //
                // Slack passes empty tone/thread/archetype/resolve blocks
                // today — same shape as the classic `draft_user_message`
                // call below — so the code-mode prompt stays apples-to-apples
                // with the existing slack draft.
                let manifest = manifest_v1();
                let system_prompt = code_mode_system(&manifest);
                let user_msg =
                    code_mode_user_message(&email, &wiki_hint, "", "", "", "");
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
                };
                let message_ctx = MessageContext {
                    channel: "slack".to_string(),
                    email: email.clone(),
                    account_id: email.account_entity_id.clone(),
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
                                    augmentagent_channel_core::code_mode::CodeModeError::ReasonerFailed(other)
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
                            channel: "slack".to_string(),
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
                                    "slack code-mode self-repair succeeded"
                                );
                                (Some(action_id), None)
                            }
                            DraftOutcome::ClassicNeeded(record) => {
                                error!(
                                    message_id = %email.message_id,
                                    "slack code-mode self-repair failed; falling back to classic"
                                );
                                (None, Some(record))
                            }
                        }
                    }
                };

                // --- Code-mode success path. The dispatcher already wrote
                // the actions row with mode='code', so we just read the body
                // back and hand off to the existing approval flow.
                if let Some(action_id) = code_mode_action_id {
                    let draft_body = self
                        .store
                        .get_action_with_email(&action_id)?
                        .and_then(|a| a.action.draft_body)
                        .unwrap_or_default();

                    if self.config.dry_run {
                        // Promote the dispatcher's `Pending` row to `DryRun`
                        // so accounting matches classic. The persisted
                        // code-mode columns (mode='code', generatedSource,
                        // toolCallTrace) are untouched.
                        self.store.update_action_status(
                            &action_id,
                            ActionStatus::DryRun,
                            Some(&draft_body),
                            None,
                        )?;
                        self.store
                            .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                        println!(
                            "[slack reply dry-run:code] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
                            email.from,
                            draft_body.len(),
                            draft_body
                        );
                        self.maybe_ingest(
                            &email,
                            DecisionKind::Reply,
                            decision.reason.as_deref(),
                            Some(&draft_body),
                            IngestTrigger::DryRunDrafted,
                        );
                        outcome.priority_replied_dry_run += 1;
                        return Ok(());
                    }

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
                        warn!(action_id, "record_nudge after post_approval failed: {e}");
                    }
                    info!(action_id, message_id = %email.message_id, "slack approval card posted (code-mode)");
                    outcome.priority_awaiting_approval += 1;
                    return Ok(());
                }

                // --- Classic fallback (I7).
                //
                // Reached when code-mode failed AND self-repair didn't
                // produce a working code-mode draft. Behaviour matches the
                // pre-I13 classic prompt path — same draft call, same
                // dispatch — except we then call `report_classic_fallback`
                // to file the postmortem gh issue and post the Discord
                // notice tagged with the classic action_id.
                let skill_system =
                    std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                        .unwrap_or_default();
                let draft = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "", "", "", "", "");
                let drafted = match self.reasoner.call(&draft, &draft_prompt).await {
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
                        Some(&drafted),
                        ActionStatus::DryRun,
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    println!(
                        "[slack reply dry-run] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
                        email.from,
                        drafted.len(),
                        drafted
                    );
                    // I7: file postmortem when classic fallback was triggered
                    // by a code-mode failure. Successful repair returns above,
                    // so this branch is exclusively for "repair couldn't save it".
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
                            channel: "slack".to_string(),
                            model: code_mode_opts.model.clone(),
                            manifest_version: "v1",
                        };
                        report_classic_fallback(&failure_ctx, &record, &action_id).await;
                    }
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        decision.reason.as_deref(),
                        Some(&drafted),
                        IngestTrigger::DryRunDrafted,
                    );
                    outcome.priority_replied_dry_run += 1;
                    return Ok(());
                }

                let action_id = self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&drafted),
                    ActionStatus::Pending,
                )?;
                // I7 postmortem (non-dry-run): file before post_approval so
                // the Discord notice + the approval card land together.
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
                        channel: "slack".to_string(),
                        model: code_mode_opts.model.clone(),
                        manifest_version: "v1",
                    };
                    report_classic_fallback(&failure_ctx, record, &action_id).await;
                }
                if let Err(e) = self
                    .approvals
                    .post_approval(&action_id, &email, &drafted)
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
                info!(action_id, message_id = %email.message_id, "slack approval card posted");
                outcome.priority_awaiting_approval += 1;
                Ok(())
            }
            // Capture / Meeting are wave-A wiki-ingest-only kinds emitted by
            // the voice and gcal channels respectively — slack triage must
            // never produce them. Defensive skip if the model misbehaves.
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "slack triage returned non-message decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                outcome.priority_skipped += 1;
                Ok(())
            }
        }
    }

    fn wiki_hint_for_sender(&self, email: &Email) -> String {
        let Some(index) = &self.identity_index else {
            return String::new();
        };
        let slack_id = extract_slack_id(&email.from).unwrap_or_default();
        if slack_id.is_empty() {
            return String::new();
        }
        match index.lookup(PLATFORM, &slack_id) {
            Some(page) => format!(
                "Sender's wiki page: {} (open with Read; weight the decision by their documented tone/importance).",
                page.slug
            ),
            None => String::new(),
        }
    }

    fn maybe_ingest(
        &self,
        email: &Email,
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

/// Convert a Slack message + its owning subscription into an `Email` row.
///
/// `my_user_id` identifies the authenticated Slack user in this workspace;
/// it's embedded in the `from` tag when the message is from that user so
/// downstream self-message filters match. `account_entity_id` is stamped with
/// the subscription's `account_id` (Slack `team_id`) when present so the
/// approver can route replies back to the correct workspace without another
/// lookup. Falls back to stamping the user id when no account_id is set
/// (legacy single-workspace rows).
pub(crate) fn message_to_email(
    msg: &SlackMessage,
    sub: &ChannelSubscription,
    my_user_id: &str,
) -> Email {
    let author_id = msg.user.clone().unwrap_or_default();
    let author_label = msg
        .username
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| author_id.clone());
    let from = format!("{} <slack:{}>", author_label, author_id);
    let kind = match sub.mode {
        SubscriptionMode::Priority => "dm",
        SubscriptionMode::Digest | SubscriptionMode::StoreOnly => "digest_item",
    };
    let account_entity_id = match sub.account_id.as_deref() {
        Some(team_id) => format!("{ACCOUNT_ENTITY_ID_PREFIX}:team:{team_id}"),
        None => format!("{ACCOUNT_ENTITY_ID_PREFIX}:{my_user_id}"),
    };
    Email {
        message_id: format!("{}:{}", sub.channel_id, msg.ts),
        thread_id: Some(sub.channel_id.clone()),
        from,
        subject: String::new(),
        body: msg.text.clone(),
        date: msg.ts.clone(),
        account_entity_id: Some(account_entity_id),
        platform: PLATFORM.to_string(),
        kind: kind.to_string(),
    }
}

/// Parse a Slack user id out of the `from` field shape
/// `"<display> <slack:<user_id>>"`.
fn extract_slack_id(from: &str) -> Option<String> {
    let start = from.rfind("<slack:")? + "<slack:".len();
    let end = from[start..].find('>')?;
    Some(from[start..start + end].to_string())
}

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

/// Pick the correct workspace client for a subscription. If the subscription
/// names an `account_id` that we've loaded, use it. If the subscription's
/// `account_id` is `None` (legacy rows from before multi-workspace) and
/// there's exactly one loaded workspace, fall through to it so the poller
/// keeps working without a manual DB fixup.
fn resolve_workspace<'a>(
    sub: &ChannelSubscription,
    clients: &'a HashMap<String, WorkspaceClient>,
) -> Option<&'a WorkspaceClient> {
    if let Some(team_id) = sub.account_id.as_deref() {
        return clients.get(team_id);
    }
    if clients.len() == 1 {
        return clients.values().next();
    }
    None
}

fn load_workspace_client(ws: &SlackWorkspace) -> Option<WorkspaceClient> {
    match SlackAuth::load_for_team(&ws.team_id) {
        Ok(auth) => build_workspace_client_from_auth(auth),
        Err(e) => {
            warn!(team_id = %ws.team_id, "slack auth load failed: {e}");
            None
        }
    }
}

fn build_workspace_client_from_auth(auth: SlackAuth) -> Option<WorkspaceClient> {
    let team_id = auth.team_id.clone();
    let user_id = auth.user_id.clone();
    match SlackClient::new(auth) {
        Ok(c) => Some(WorkspaceClient {
            team_id,
            client: Arc::new(c),
            my_user_id: user_id,
        }),
        Err(e) => {
            warn!(team_id = %team_id, "slack client build failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::ReasonerOpts;

    fn sub_with_mode(mode: SubscriptionMode) -> ChannelSubscription {
        ChannelSubscription {
            id: "sub1".into(),
            platform: PLATFORM.into(),
            channel_id: "C1".into(),
            display_name: "#general".into(),
            mode,
            active: true,
            account_id: Some("T1".into()),
            last_seen_message_id: None,
            last_digest_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn sample_msg(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            message_type: "message".into(),
            subtype: None,
            ts: ts.into(),
            user: Some("U2".into()),
            username: Some("alice".into()),
            text: text.into(),
            thread_ts: None,
            bot_id: None,
        }
    }

    #[test]
    fn message_to_email_priority_uses_dm_kind() {
        let m = sample_msg("100.000001", "hi");
        let s = sub_with_mode(SubscriptionMode::Priority);
        let e = message_to_email(&m, &s, "me");
        assert_eq!(e.platform, "slack");
        assert_eq!(e.kind, "dm");
        assert_eq!(e.thread_id.as_deref(), Some("C1"));
        assert!(e.from.contains("<slack:U2>"));
        assert!(e.from.starts_with("alice"));
    }

    #[test]
    fn message_to_email_digest_uses_digest_item_kind() {
        let m = sample_msg("100.000001", "hi");
        let s = sub_with_mode(SubscriptionMode::Digest);
        let e = message_to_email(&m, &s, "me");
        assert_eq!(e.kind, "digest_item");
    }

    #[test]
    fn extract_slack_id_parses_tag() {
        assert_eq!(extract_slack_id("alice <slack:U123>"), Some("U123".into()));
        assert_eq!(extract_slack_id("no-tag"), None);
    }

    #[test]
    fn jitter_stays_in_window() {
        for _ in 0..50 {
            assert!(jitter_secs() <= 2 * JITTER_SECS);
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

    #[async_trait::async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _msg: &str) -> anyhow::Result<String> {
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"decision":"skip","reason":"stub"}"#.into()))
        }
    }

    #[derive(Default)]
    struct CountingBroker {
        approvals: std::sync::Mutex<usize>,
        flags: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ApprovalBroker for CountingBroker {
        async fn post_approval(
            &self,
            _id: &str,
            _e: &augmentagent_store::Email,
            _d: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.approvals.lock().unwrap() += 1;
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _e: &augmentagent_store::Email,
            _r: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.flags.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// Disable the real `gh` CLI for the duration of the test binary.
    /// `GhCliIssueRunner::create_issue` honors `AUGMENTAGENT_GH_DISABLE=1` and
    /// returns a benign error instead of shelling out, so the I7 reporter
    /// logs + continues without touching the production repo.
    static GH_DISABLE_INIT: std::sync::Once = std::sync::Once::new();
    fn disable_gh_for_tests() {
        GH_DISABLE_INIT.call_once(|| {
            // SAFETY: set once at module init before any test runs; no
            // concurrent reads of this var.
            std::env::set_var("AUGMENTAGENT_GH_DISABLE", "1");
        });
    }

    /// Mock gh-CLI runner that records every `gh issue create` call without
    /// spawning the real binary. Returns canned issue numbers starting at
    /// `next_number`. Wire into a `SlackChannel` via
    /// `.with_gh_issue_runner(...)` so I7 postmortem tests never touch the
    /// production repo.
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
    #[async_trait::async_trait]
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

    /// Broker that records every post/flag without blocking — used by I7
    /// fallback tests to assert the Discord notice carries the issue number.
    #[derive(Default)]
    struct RecordingBroker {
        posts: std::sync::Mutex<Vec<String>>,
        flag_posts: std::sync::Mutex<Vec<(String, String)>>,
    }
    #[async_trait::async_trait]
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

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        // Belt-and-suspenders: any test that opens a store also disables the
        // real `gh` CLI for the rest of the process.
        disable_gh_for_tests();
        use rusqlite::Connection;
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL, originalBody TEXT,
                    draftBody TEXT, status TEXT NOT NULL DEFAULT 'pending',
                    errorMessage TEXT, createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT, fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL, body TEXT, receivedAt TEXT,
                    accountEntityId TEXT, firstSeenAt INTEGER NOT NULL,
                    triageResult TEXT, agentProcessedAt INTEGER
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn build_channel<R: Reasoner + 'static>(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
    ) -> SlackChannel<R> {
        SlackChannel::new(
            store,
            reasoner,
            approvals,
            SlackChannelConfig {
                dry_run: false,
                poll_interval: Duration::from_secs(1),
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("skills/slack-triage"),
            },
            None,
        )
    }

    #[tokio::test]
    async fn priority_skip_records_skipped() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"newsletter"}"#,
        ]));
        let b: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = build_channel(store.clone(), r, Arc::clone(&b));
        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "deal deal");
        let e = message_to_email(&m, &sub, "me");
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_skipped, 1);
    }

    #[tokio::test]
    async fn priority_reply_posts_approval() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            "hey, works for me",
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), r, b);

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "15 min tomorrow?");
        let e = message_to_email(&m, &sub, "me");
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_awaiting_approval, 1);
        assert_eq!(*counting.approvals.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn priority_flag_posts_flag_notice() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"unclear"}"#,
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), r, b);

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "hey");
        let e = message_to_email(&m, &sub, "me");
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_flagged, 1);
        assert_eq!(*counting.flags.lock().unwrap(), 1);
    }

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

    /// Code-mode happy path (dry-run): triage returns reply, the second
    /// reasoner response is a fenced TS program whose body calls
    /// `tools.draft("slack", ...)`. The Deno sandbox executes it and the
    /// dispatcher lands an `actions` row with `mode='code'`,
    /// `generatedSource`, and `toolCallTrace`. Verifies the channel string
    /// is `slack` (I13's headline contract).
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
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            // Plain JS body inside a ```ts fence — `extract_ts_block`
            // matches the language tag, while the Deno runner uses indirect
            // eval (no TS-stripping), so the program itself must not carry
            // TypeScript type annotations.
            "```ts\n\
             async function main() {\n\
               await tools.draft(\"slack\", \"thanks — shipping today\", \"answer the question\");\n\
             }\n\
             main();\n\
             ```\n",
        ]));
        let broker: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = SlackChannel::new(
            store.clone(),
            r,
            broker,
            SlackChannelConfig {
                dry_run: true,
                poll_interval: Duration::from_secs(1),
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
            },
            None,
        );
        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "any update?");
        let e = message_to_email(&m, &sub, "me");
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_replied_dry_run, 1, "expected 1 dry-run reply");
        assert_eq!(out.errors, 0);

        // Read the actions row back and verify the code-mode columns.
        let rows = store
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT mode, generatedSource, toolCallTrace, draftBody, status \
                     FROM actions",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(rows.len(), 1, "expected exactly one actions row");
        let (mode, src, trace, body, status) = &rows[0];
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
        // The dispatcher's terminal `tools.draft` records the channel arg as
        // part of the trace. Sanity-check it reads "slack" — the headline
        // contract for I13.
        assert!(
            trace_str.contains("\"slack\""),
            "toolCallTrace must record channel=\"slack\"; got {trace_str:?}"
        );
        assert_eq!(
            body.as_deref(),
            Some("thanks — shipping today"),
            "draftBody must match what tools.draft passed"
        );
        assert_eq!(status, "dry_run", "dry-run mode must update status");
    }

    /// Code-mode failure path: neither the initial program nor the repair
    /// program carry a fenced TS block, so code-mode + self-repair both
    /// fall through to the classic prose draft. Verifies that:
    ///   - the classic action row is landed with `mode='classic'`,
    ///   - exactly one `gh issue create` invocation is filed with the
    ///     `code-mode-failure` label,
    ///   - the Discord notice fires through `post_flag_notice` carrying the
    ///     issue number,
    ///   - the existing slack approval card still posts (existing pipeline
    ///     unbroken).
    #[tokio::test]
    async fn code_mode_failure_falls_through_to_classic_path() {
        let (store, _f) = tmp_store();
        // Triage → reply. Two no-fence code-mode responses (initial + repair
        // retry both fail). Fourth response feeds the classic prose-draft
        // call.
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"ping"}"#,
            "no fenced block here, just prose",
            "still no fenced block — repair gave up",
            "Yes — shipping today.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let gh = Arc::new(RecordingGh::new(202));
        let ch = SlackChannel::new(
            store.clone(),
            r,
            broker.clone(),
            SlackChannelConfig {
                dry_run: false,
                poll_interval: Duration::from_secs(1),
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
            },
            None,
        )
        .with_gh_issue_runner(gh.clone());

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "u there?");
        let e = message_to_email(&m, &sub, "me");
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();

        assert_eq!(out.priority_awaiting_approval, 1);
        assert_eq!(out.errors, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);

        // Action row landed with mode='classic'.
        let mode: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT mode FROM actions WHERE status = 'pending'",
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

        // Exactly one gh issue filed with the right label + title prefix.
        let calls = gh.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one gh issue should be filed");
        let (title, body, labels) = &calls[0];
        assert!(title.starts_with("[code-mode]"));
        assert!(body.contains("## Postmortem"));
        assert!(body.contains("**Final draft mode:** classic"));
        assert!(body.contains("**Channel:** slack"));
        assert_eq!(labels, &vec!["code-mode-failure".to_string()]);

        // Discord notice posted with the issue number (#202).
        let notices = broker.flag_posts.lock().unwrap();
        assert_eq!(notices.len(), 1, "exactly one Discord notice should fire");
        assert!(notices[0].1.contains("#202"));
        assert!(notices[0].1.contains("classic"));
    }
}
