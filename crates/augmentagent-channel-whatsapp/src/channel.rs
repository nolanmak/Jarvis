//! `WhatsappChannel` — owns the sidecar [`WaClient`], drains its inbound
//! event stream, and runs each new 1:1 message through the shared
//! triage → draft → approval pipeline. Mirrors the `TelegramBotChannel`
//! structure 1:1 so debugging one means debugging both.
//!
//! ## Transport
//!
//! The whatsmeow Go sidecar owns the linked-device session and pushes
//! `received-message` events over the UDS. The channel buffers them in an
//! `mpsc` and drains the buffer on each `poll_once` / `next_work_items`
//! call — poll-only semantics over a long-lived socket, exactly as #12 §4
//! specifies.
//!
//! ## Ban-risk gating (#40 / #74 / #102)
//!
//! WhatsApp bans bot-like accounts aggressively, so this channel is
//! conservative by default:
//!
//! - **Inbound**: a chat is triaged only if its bare JID has an explicit row
//!   in `whatsapp_inbound_allowlist`. Even *reading* requires opt-in.
//! - **Outbound**: the approver refuses to send unless the chat is in
//!   `whatsapp_outbound_allowlist` AND `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED`
//!   is set (the global ban-risk kill-switch). The channel never sends
//!   directly — sends happen from the CLI approver on user Approve, same as
//!   every other channel.
//! - Groups / broadcast lists are dropped unconditionally (v1 = 1:1 only).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
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
use augmentagent_channel_core::trigger::{InboundSource, Trigger, WorkItem};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{
    ActionStatus, Email, Store, TriageResult, NUDGE_INTERVAL_MS,
};

use crate::api::WaClient;
use crate::types::{WaEvent, WaMessage};
use crate::PLATFORM;

/// Drain cadence. The socket is long-lived so events arrive in real time;
/// this is just the buffer-drain interval, kept at the channel-wide 4h
/// triage budget so the reasoner load matches the rest of the daemon.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;
pub const JITTER_SECS: u64 = 10 * 60;

/// Global ban-risk kill-switch env var. The approver checks this before any
/// outbound WhatsApp send; the channel surfaces its state in `poll_once`
/// logging so it's obvious when control is disabled.
pub const CONTROL_ENABLED_ENV: &str = "AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED";

/// True iff the global ban-risk gate is enabled. Truthy values: `1`, `true`,
/// `yes` (case-insensitive). Anything else (incl. unset) = disabled.
pub fn control_enabled() -> bool {
    std::env::var(CONTROL_ENABLED_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct WhatsappChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub skill_dir: PathBuf,
    /// The linked device's phone (E.164 digits). Carried into
    /// `account_entity_id` so the approver routes sends back to this device.
    pub phone: String,
}

impl Default for WhatsappChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/whatsapp-triage"),
            phone: String::new(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub events_drained: usize,
    pub messages_dispatched: usize,
    pub groups_dropped: usize,
    pub not_allowlisted_dropped: usize,
    pub outbound_skipped: usize,
    pub skipped: usize,
    pub flagged: usize,
    pub replied_dry_run: usize,
    pub awaiting_approval: usize,
    pub logged_out: bool,
    pub errors: usize,
}

/// Buffered inbound messages drained from the sidecar event stream.
type Inbox = Arc<Mutex<Vec<WaMessage>>>;

pub struct WhatsappChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: WhatsappChannelConfig,
    /// Shared sidecar client. `None` only in unit tests that pump the inbox
    /// directly without a socket.
    pub client: Option<WaClient>,
    /// Optional agent control/approval surface (#102). When set, messages
    /// from its designated control chat are routed to it (approve / revise /
    /// decline / wiki-query) instead of the inbound triage pipeline.
    pub control: Option<Arc<crate::control::WhatsappControlSurface>>,
    inbox: Inbox,
    wiki_schema: Option<String>,
    /// gh CLI runner for I7 postmortems on code-mode failures. Behind a
    /// trait so tests can swap in a recorder. Production uses
    /// [`GhCliIssueRunner`], which honours `AUGMENTAGENT_GH_DISABLE=1` as a
    /// test-time no-op.
    gh_issue_runner: Arc<dyn GhIssueRunner>,
}

impl<R: Reasoner + 'static> WhatsappChannel<R> {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        client: Option<WaClient>,
        config: WhatsappChannelConfig,
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
            client,
            control: None,
            inbox: Arc::new(Mutex::new(Vec::new())),
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

    /// Attach the agent control/approval surface (#102). Builder-style so the
    /// CLI can construct the channel then layer control on once the shared
    /// `WaClient` is available.
    pub fn with_control(
        mut self,
        control: Arc<crate::control::WhatsappControlSurface>,
    ) -> Self {
        self.control = Some(control);
        self
    }

    /// Hand back the inbox + a closure-free way to push events into it. Used
    /// by the CLI to spawn the event-pump task once and share the same
    /// buffer the poll loop drains.
    pub fn inbox_handle(&self) -> Inbox {
        Arc::clone(&self.inbox)
    }

    /// Spawn the background task that reads the sidecar event channel and
    /// files inbound messages into the shared inbox. Lifecycle events
    /// (`logged-out`, `pair-success`) update the `whatsapp_devices` row.
    pub fn spawn_event_pump(
        &self,
        mut events: tokio::sync::mpsc::Receiver<WaEvent>,
        shutdown: CancellationToken,
    ) {
        let inbox = Arc::clone(&self.inbox);
        let store = Arc::clone(&self.store);
        let phone = self.config.phone.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        debug!("whatsapp event pump: shutdown");
                        return;
                    }
                    ev = events.recv() => {
                        let Some(ev) = ev else {
                            debug!("whatsapp event channel closed");
                            return;
                        };
                        match ev {
                            WaEvent::ReceivedMessage { message } => {
                                inbox.lock().await.push(message);
                            }
                            WaEvent::LoggedOut { reason } => {
                                warn!(%reason, "whatsapp device logged out — re-pair required");
                                if !phone.is_empty() {
                                    let _ = store.mark_whatsapp_device_logged_out(&phone);
                                }
                            }
                            WaEvent::PairSuccess { device_jid, user_jid } => {
                                info!(%device_jid, %user_jid, "whatsapp pair-success");
                            }
                            WaEvent::Connected => info!("whatsapp sidecar connected"),
                            WaEvent::Qr { .. } => {
                                // QR is only meaningful during `whatsapp login`;
                                // ignore here.
                            }
                        }
                    }
                }
            }
        });
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("whatsapp channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "whatsapp poll complete"),
                        Err(e) => error!("whatsapp poll failed: {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    /// Drain the buffered inbox and run each new message through the pipeline.
    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let drained: Vec<WaMessage> = {
            let mut guard = self.inbox.lock().await;
            std::mem::take(&mut *guard)
        };
        outcome.events_drained = drained.len();
        if !control_enabled() {
            debug!(
                "{CONTROL_ENABLED_ENV} unset — inbound triage still runs but \
                 outbound sends are gated off at the approver"
            );
        }

        for msg in drained {
            match self.handle_message(msg, &mut outcome).await {
                Ok(()) => {}
                Err(e) => {
                    outcome.errors += 1;
                    error!("whatsapp handle_message failed: {e:#}");
                }
            }
        }
        Ok(outcome)
    }

    async fn handle_message(
        &self,
        msg: WaMessage,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        // 1:1 only — drop groups / broadcast.
        if !msg.chat.is_personal() {
            outcome.groups_dropped += 1;
            return Ok(());
        }
        // Never triage our own outbound echoes.
        if msg.is_outbound() {
            return Ok(());
        }
        let chat_jid = msg.chat.bare();
        // Agent control surface (#102): if this is the designated control
        // chat, the message is a command/query to the agent — route it there
        // and do NOT run it through inbound triage.
        if let Some(control) = &self.control {
            if control.is_control_chat(&chat_jid) {
                match control.handle_control_message(&msg).await {
                    Ok(true) => {
                        outcome.messages_dispatched += 1;
                        return Ok(());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(%chat_jid, "control surface handling failed: {e}");
                        outcome.errors += 1;
                        return Ok(());
                    }
                }
            }
        }
        // Inbound allowlist gate — even reading requires explicit opt-in.
        if !self.store.is_whatsapp_inbound_allowed(&chat_jid)? {
            outcome.not_allowlisted_dropped += 1;
            debug!(%chat_jid, "whatsapp chat not in inbound allowlist; dropped");
            return Ok(());
        }
        // Pure-media (no decoded text) — nothing for the reasoner.
        if msg.text.trim().is_empty() {
            return Ok(());
        }

        let email = msg.into_email(&self.config.phone);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(());
        }
        // Re-poll guard: a prior drain may have logged an action for this
        // message. Gate re-triage so we don't stack duplicate cards.
        if self.store.is_message_processed(&email.message_id)? {
            return Ok(());
        }

        outcome.messages_dispatched += 1;

        // --- TRIAGE ---
        let triage = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
        let raw = self.reasoner.call(&triage, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "whatsapp triage parse failed: {e}; raw={raw}");
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
                outcome.skipped += 1;
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
                outcome.flagged += 1;
                Ok(())
            }
            DecisionKind::Reply => {
                // --- CODE-MODE attempt (I12 / #58). Mirrors the email
                // channel's I6+I7 wiring; see
                // `crates/augmentagent-channel-email/src/channel.rs` for the
                // canonical block. The reasoner emits a TS program that ends
                // in `tools.draft("whatsapp", body, reason)`; the Deno
                // sandbox executes it, and the dispatcher's terminal handler
                // writes the actions row (mode='code', generatedSource,
                // toolCallTrace) and stashes the action id we pick up below.
                //
                // On any failure (reasoner spawn, missing fenced block,
                // sandbox timeout, runtime exception, dispatcher error) we
                // hand off to `handle_code_mode_failure` for one self-repair
                // pass. If repair lands a working draft we use it; otherwise
                // we fall through to the classic prompt path and call
                // `report_classic_fallback` to file the postmortem gh issue
                // + post the Discord notice. The whatsmeow sidecar dispatch
                // path is untouched — it only fires on user Approve from the
                // Discord card, and both rails post to the same approval
                // broker with the same action id, so the sidecar sees no
                // difference between code-mode and classic actions.
                //
                // WhatsApp has no wiki / tone / thread / archetype / ask
                // resolver wiring (unlike email), so all of those block
                // strings are passed empty and short-circuit in the prompt
                // builders.
                let manifest = manifest_v1();
                let system_prompt = code_mode_system(&manifest);
                let user_msg = code_mode_user_message(&email, "", "", "", "", "");
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
                    channel: "whatsapp".to_string(),
                    email: email.clone(),
                    account_id: Some(self.config.phone.clone()),
                };

                let mut cm_source: String = String::new();
                let cm_attempt: Result<String, (augmentagent_channel_core::code_mode::CodeModeError, FailureStage)> = async {
                    let ts_source = match self.reasoner.call_code_mode(&code_mode_opts, &user_msg).await {
                        Ok(s) => s,
                        Err(e) => {
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
                    .with_wiki_hint(String::new());
                    if let Err(e) = code_mode::run_program(&ts_source, &manifest, &dispatcher).await {
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

                let (code_mode_action_id, pending_classic_record): (
                    Option<String>,
                    Option<augmentagent_channel_core::code_mode::FailureRecord>,
                ) = match cm_attempt {
                    Ok(action_id) => (Some(action_id), None),
                    Err((cme, stage)) => {
                        warn!(
                            message_id = %email.message_id,
                            stage = ?stage,
                            "whatsapp code-mode attempt failed: {cme}; invoking self-repair"
                        );
                        let failure_ctx = FailureCtx {
                            reasoner: self.reasoner.as_ref(),
                            opts: code_mode_opts.clone(),
                            user_msg: user_msg.clone(),
                            manifest: manifest.clone(),
                            message_ctx: message_ctx.clone(),
                            wiki_hint: String::new(),
                            store: Arc::clone(&self.store),
                            broker: Arc::clone(&self.approvals),
                            gh: Arc::clone(&self.gh_issue_runner),
                            email: email.clone(),
                            channel: "whatsapp".to_string(),
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
                                    "whatsapp code-mode self-repair succeeded"
                                );
                                (Some(action_id), None)
                            }
                            DraftOutcome::ClassicNeeded(record) => {
                                error!(
                                    message_id = %email.message_id,
                                    "whatsapp code-mode self-repair failed; falling back to classic"
                                );
                                (None, Some(record))
                            }
                        }
                    }
                };

                // --- Code-mode success: read the persisted draft body the
                // dispatcher just wrote, then hand off to the existing
                // approval flow (post_approval → record_nudge → wait for
                // user Approve → whatsmeow sidecar send via the approver).
                if let Some(action_id) = code_mode_action_id {
                    let drafted = self
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
                            Some(&drafted),
                            None,
                        )?;
                        self.store
                            .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                        println!(
                            "[whatsapp reply dry-run:code] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
                            email.from,
                            drafted.len(),
                            drafted
                        );
                        self.maybe_ingest(
                            &email,
                            DecisionKind::Reply,
                            decision.reason.as_deref(),
                            Some(&drafted),
                            IngestTrigger::DryRunDrafted,
                        );
                        outcome.replied_dry_run += 1;
                        return Ok(());
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
                    if let Err(e) = self
                        .store
                        .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
                    {
                        warn!(action_id, "record_nudge after post_approval failed: {e}");
                    }
                    info!(action_id, message_id = %email.message_id, "whatsapp approval card posted (code-mode)");
                    outcome.awaiting_approval += 1;
                    return Ok(());
                }

                // --- Classic fallback (I7). Reached when code-mode failed
                // AND self-repair couldn't recover. Pre-#52 draft path,
                // plus `report_classic_fallback` after the row lands.
                let skill_system =
                    std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                        .unwrap_or_default();
                let draft = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "", "", "", "", "");
                let drafted = match self.reasoner.call(&draft, &draft_prompt).await {
                    Ok(s) => s.trim().to_string(),
                    Err(e) => {
                        error!(message_id = %email.message_id, "whatsapp draft failed: {e}");
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
                        "[whatsapp reply dry-run] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
                        email.from,
                        drafted.len(),
                        drafted
                    );
                    if let Some(record) = pending_classic_record {
                        let failure_ctx = FailureCtx {
                            reasoner: self.reasoner.as_ref(),
                            opts: code_mode_opts.clone(),
                            user_msg: user_msg.clone(),
                            manifest: manifest.clone(),
                            message_ctx: message_ctx.clone(),
                            wiki_hint: String::new(),
                            store: Arc::clone(&self.store),
                            broker: Arc::clone(&self.approvals),
                            gh: Arc::clone(&self.gh_issue_runner),
                            email: email.clone(),
                            channel: "whatsapp".to_string(),
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
                    outcome.replied_dry_run += 1;
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
                if let Some(record) = pending_classic_record.as_ref() {
                    let failure_ctx = FailureCtx {
                        reasoner: self.reasoner.as_ref(),
                        opts: code_mode_opts.clone(),
                        user_msg: user_msg.clone(),
                        manifest: manifest.clone(),
                        message_ctx: message_ctx.clone(),
                        wiki_hint: String::new(),
                        store: Arc::clone(&self.store),
                        broker: Arc::clone(&self.approvals),
                        gh: Arc::clone(&self.gh_issue_runner),
                        email: email.clone(),
                        channel: "whatsapp".to_string(),
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
                if let Err(e) = self
                    .store
                    .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
                {
                    warn!(action_id, "record_nudge after post_approval failed: {e}");
                }
                info!(action_id, message_id = %email.message_id, "whatsapp approval card posted");
                outcome.awaiting_approval += 1;
                Ok(())
            }
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "whatsapp triage returned non-message decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                outcome.skipped += 1;
                Ok(())
            }
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

/// `InboundSource` adapter — drains the buffered inbox into `WorkItem`s so
/// downstream code can consume the raw inbox via `InboundMessageTrigger`
/// without the reasoner. The production path uses
/// [`WhatsappChannel::poll_once`]; this is the Phase-2/3-shaped view.
pub struct WhatsappInbound {
    pub inbox: Inbox,
    pub phone: String,
}

#[async_trait]
impl InboundSource for WhatsappInbound {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        let drained: Vec<WaMessage> = {
            let mut g = self.inbox.lock().await;
            std::mem::take(&mut *g)
        };
        Ok(drained
            .into_iter()
            .filter(|m| m.chat.is_personal() && !m.is_outbound() && !m.text.trim().is_empty())
            .map(|m| message_to_work_item(&m))
            .collect())
    }
}

/// Build the `WorkItem` shape (#12 §4). Payload carries the full `WaMessage`
/// so handlers can reconstruct rich state without re-fetching.
pub fn message_to_work_item(msg: &WaMessage) -> WorkItem {
    WorkItem {
        platform: PLATFORM.to_string(),
        kind: "dm".to_string(),
        external_id: format!("wa:{}:{}", msg.chat.bare(), msg.id),
        payload: serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
    }
}

/// `Trigger` impl over the buffered inbox so the WhatsApp channel anchors to
/// the same work-source contract as every other platform.
#[async_trait]
impl<R: Reasoner + 'static> Trigger for WhatsappChannel<R> {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        let src = WhatsappInbound {
            inbox: Arc::clone(&self.inbox),
            phone: self.config.phone.clone(),
        };
        src.fetch_new().await
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::ReasonerOpts;
    use crate::types::Jid;

    /// Disable `gh issue create` for the whole test binary. The I7
    /// code-mode-failure → classic fallback path lives downstream of the
    /// Reply branch, and tests that traverse it would otherwise shell out
    /// to a possibly-authenticated `gh` and file real issues. The env-var
    /// hook is read by `GhCliIssueRunner::create_issue`. Tests that DO
    /// want to assert gh invocations can swap a recording stub via
    /// `with_gh_issue_runner` — which bypasses this hook.
    static GH_DISABLE_INIT: std::sync::Once = std::sync::Once::new();
    fn disable_gh_for_tests() {
        GH_DISABLE_INIT.call_once(|| {
            // SAFETY: set once at module init before any test runs; no
            // concurrent reads of this var.
            std::env::set_var("AUGMENTAGENT_GH_DISABLE", "1");
        });
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
        async fn call(&self, _o: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
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
    #[async_trait]
    impl ApprovalBroker for CountingBroker {
        async fn post_approval(
            &self,
            _id: &str,
            _e: &Email,
            _d: &str,
        ) -> Result<(), ApprovalError> {
            *self.approvals.lock().unwrap() += 1;
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _e: &Email,
            _r: &str,
        ) -> Result<(), ApprovalError> {
            *self.flags.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
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
                CREATE TABLE whatsapp_inbound_allowlist (
                    chat_jid TEXT PRIMARY KEY, enabled_at_ms INTEGER NOT NULL
                );
                CREATE TABLE whatsapp_outbound_allowlist (
                    chat_jid TEXT PRIMARY KEY, enabled_at_ms INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn channel<R: Reasoner + 'static>(
        store: Arc<Store>,
        reasoner: Arc<R>,
        broker: Arc<dyn ApprovalBroker>,
    ) -> WhatsappChannel<R> {
        // Block any gh CLI calls the I7 classic-fallback path might trigger.
        disable_gh_for_tests();
        WhatsappChannel::new(
            store,
            reasoner,
            broker,
            None,
            WhatsappChannelConfig {
                dry_run: false,
                poll_interval: Duration::from_secs(1),
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("/tmp/nonexistent-wa-skill"),
                phone: "15559998888".into(),
            },
        )
    }

    fn msg(id: &str, text: &str) -> WaMessage {
        WaMessage {
            id: id.into(),
            chat: Jid::new("15551234567@s.whatsapp.net"),
            sender: Jid::new("15551234567@s.whatsapp.net"),
            push_name: "Tony".into(),
            text: text.into(),
            timestamp: 1776630000,
            from_me: false,
        }
    }

    #[tokio::test]
    async fn not_allowlisted_chat_is_dropped() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([]));
        let b: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = channel(store, r.clone(), Arc::clone(&b));
        ch.inbox.lock().await.push(msg("m1", "hey"));
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.events_drained, 1);
        assert_eq!(out.not_allowlisted_dropped, 1);
        assert_eq!(out.messages_dispatched, 0);
        // Reasoner untouched.
        assert!(r.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn groups_are_dropped() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([]));
        let b: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = channel(store, r, Arc::clone(&b));
        let mut m = msg("g1", "group chatter");
        m.chat = Jid::new("120363001234567890@g.us");
        ch.inbox.lock().await.push(m);
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.groups_dropped, 1);
        assert_eq!(out.messages_dispatched, 0);
    }

    #[tokio::test]
    async fn allowlisted_reply_posts_card() {
        let (store, _f) = tmp_store();
        store
            .allow_whatsapp_inbound("15551234567@s.whatsapp.net")
            .unwrap();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"direct question"}"#,
            "Thursday 3pm works.",
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = channel(store, r, b);
        ch.inbox.lock().await.push(msg("m-reply", "free thursday?"));
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.awaiting_approval, 1);
        assert_eq!(*counting.approvals.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn allowlisted_flag_posts_notice() {
        let (store, _f) = tmp_store();
        store
            .allow_whatsapp_inbound("15551234567@s.whatsapp.net")
            .unwrap();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"cold outreach"}"#,
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = channel(store, r, b);
        ch.inbox.lock().await.push(msg("m-flag", "buy my course"));
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.flagged, 1);
        assert_eq!(*counting.flags.lock().unwrap(), 1);
        assert_eq!(*counting.approvals.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn outbound_echo_is_skipped() {
        let (store, _f) = tmp_store();
        store
            .allow_whatsapp_inbound("15551234567@s.whatsapp.net")
            .unwrap();
        let r = Arc::new(ScriptedReasoner::new([]));
        let b: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = channel(store, r.clone(), Arc::clone(&b));
        let mut m = msg("m-out", "sent by me");
        m.from_me = true;
        ch.inbox.lock().await.push(m);
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.messages_dispatched, 0);
        assert!(r.responses.lock().unwrap().is_empty());
    }

    #[test]
    fn control_enabled_parses_truthy() {
        std::env::set_var(CONTROL_ENABLED_ENV, "true");
        assert!(control_enabled());
        std::env::set_var(CONTROL_ENABLED_ENV, "0");
        assert!(!control_enabled());
        std::env::remove_var(CONTROL_ENABLED_ENV);
        assert!(!control_enabled());
    }

    #[tokio::test]
    async fn trigger_yields_personal_dm_work_items() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([]));
        let b: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = channel(store, r, b);
        ch.inbox.lock().await.push(msg("w1", "ping"));
        let cancel = CancellationToken::new();
        let items = ch.next_work_items(&cancel).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].platform, "whatsapp");
        assert_eq!(items[0].kind, "dm");
        assert_eq!(
            items[0].external_id,
            "wa:15551234567@s.whatsapp.net:w1"
        );
    }
}
