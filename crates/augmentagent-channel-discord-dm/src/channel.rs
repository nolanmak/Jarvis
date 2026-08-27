//! `DiscordChannel` — polls every active `channel_subscriptions` row with
//! `platform='discord'`, fetches messages since `last_seen_message_id`, and
//! dispatches each message by the subscription's mode:
//!
//! - `Priority`   → full triage → draft → approval card pipeline (DM-shape)
//! - `Digest`     → upsert with `kind='digest_item'`, no Claude call
//! - `StoreOnly`  → upsert with `kind='digest_item'`, no Claude call
//!
//! The digest scheduler (crate::digest) consumes digest-mode rows on its own
//! daily tick.

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
    ActionStatus, ChannelSubscription, Email, Store, SubscriptionMode, TriageResult,
    NUDGE_INTERVAL_MS,
};
use augmentagent_wiki::IdentityIndex;

use crate::api::{DiscordClient, DiscordError};
use crate::types::Message;
use crate::{PLATFORM, ACCOUNT_ENTITY_ID_PREFIX};

/// Per-attachment download cap (200 KB). Discord uploads can be large; we
/// only need enough to seed the agent's context.
pub const MAX_ATTACHMENT_BYTES: usize = 200 * 1024;

/// Per-image download cap. Images aren't inlined into the prompt (no context
/// cost) — this only bounds disk/download; 5 MiB covers phone screenshots.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Max images saved per message. Mirrors the query path's restraint; more
/// than a few images per DM is noise for triage/draft anyway.
pub const MAX_IMAGE_ATTACHMENTS: usize = 4;
/// Aggregate cap across all attachments in a single message (500 KB).
pub const MAX_ATTACHMENTS_TOTAL_BYTES: usize = 500 * 1024;

/// Default poll interval — mirror LinkedIn's 4h cadence per the user's locked
/// decision. 4h × 30min jitter keeps the fingerprint human-looking.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;
pub const JITTER_SECS: u64 = 30 * 60;

/// Max messages to fetch per (subscription, tick). If a channel has more new
/// messages than this, the next tick picks up the rest. 100 is Discord's hard
/// cap per messages endpoint call.
pub const MAX_MESSAGES_PER_TICK: u32 = 100;

#[derive(Clone, Debug)]
pub struct DiscordChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    /// Skill dir for the triage/draft rubric. Defaults to
    /// `skills/discord-triage`.
    pub skill_dir: PathBuf,
}

impl Default for DiscordChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/discord-triage"),
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

pub struct DiscordChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub client: Arc<DiscordClient>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: DiscordChannelConfig,
    pub identity_index: Option<Arc<IdentityIndex>>,
    /// Discord user id of the authenticated account — used to skip our own
    /// outbound messages on ingest (same idea as LinkedIn's `member_urn`).
    pub my_user_id: String,
    wiki_schema: Option<String>,
    /// gh CLI runner for I7 postmortems on code-mode failures. Behind a trait
    /// so tests can mock the `gh issue create` invocation; production defaults
    /// to [`GhCliIssueRunner`] which shells out to the `gh` binary on PATH.
    gh_issue_runner: Arc<dyn GhIssueRunner>,
}

impl<R: Reasoner + 'static> DiscordChannel<R> {
    pub fn new(
        store: Arc<Store>,
        client: Arc<DiscordClient>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        my_user_id: String,
        config: DiscordChannelConfig,
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
            client,
            reasoner,
            approvals,
            config,
            identity_index,
            my_user_id,
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

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("discord channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "discord poll complete"),
                        Err(e) => error!("discord poll failed: {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let subs = self.store.list_active_subscriptions(PLATFORM)?;
        outcome.subscriptions_polled = subs.len();
        if subs.is_empty() {
            debug!("no active discord subscriptions; nothing to poll");
            return Ok(outcome);
        }

        for sub in subs {
            if let Err(e) = self.poll_subscription(&sub, &mut outcome).await {
                outcome.errors += 1;
                error!(sub_id = %sub.id, channel_id = %sub.channel_id, "subscription poll failed: {e:#}");
            }
        }
        Ok(outcome)
    }

    async fn poll_subscription(
        &self,
        sub: &ChannelSubscription,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let messages = match self
            .client
            .fetch_messages(
                &sub.channel_id,
                sub.last_seen_message_id.as_deref(),
                MAX_MESSAGES_PER_TICK,
            )
            .await
        {
            Ok(msgs) => msgs,
            Err(DiscordError::AuthExpired) => {
                warn!(
                    "discord auth expired — run `augmentagent discord login` to re-harvest",
                );
                anyhow::bail!("auth expired");
            }
            Err(e) => return Err(e.into()),
        };

        // Discord returns newest-first; reverse so we process oldest->newest,
        // which keeps `last_seen_message_id` monotonic even if we error mid-loop.
        let mut messages = messages;
        messages.reverse();
        outcome.messages_seen += messages.len();

        let mut newest_seen: Option<String> = None;
        for msg in messages {
            newest_seen = Some(msg.id.clone());

            // Skip messages we sent ourselves (we don't reply to our own sends),
            // bot messages, and system event types.
            if msg.author.id == self.my_user_id {
                continue;
            }
            if !msg.is_default_user_message() {
                continue;
            }

            let mut msg = msg;
            self.inline_text_attachments(&mut msg).await;
            if let Err(e) = self.handle_message(sub, msg, outcome).await {
                outcome.errors += 1;
                error!(sub_id = %sub.id, "handle_message failed: {e:#}");
                // Don't halt the whole subscription; stash progress below.
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
        msg: Message,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let email = message_to_email(&msg, sub, &self.my_user_id);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(());
        }

        match sub.mode {
            SubscriptionMode::Priority => self.handle_priority(email, outcome).await,
            SubscriptionMode::Digest => {
                outcome.digest_stored += 1;
                // Leave triageResult NULL — the digest scheduler aggregates
                // by time window + channel, not by triage decision.
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

    /// Inline text-like attachments (`.txt`, `.md`, `.log`, `text/*`, etc.)
    /// as decoded UTF-8 bodies, and save image attachments to
    /// `/tmp/aa-img-<msgid>-<idx>.<ext>` referenced via `IMAGE:` marker
    /// lines (the cross-provider convention — see
    /// `augmentagent_channel_core::images`). Anything neither text- nor
    /// image-like is skipped with a debug log. Enforces a per-text-file cap
    /// ([`MAX_ATTACHMENT_BYTES`]), a per-message text total
    /// ([`MAX_ATTACHMENTS_TOTAL_BYTES`]), a per-image cap
    /// ([`MAX_IMAGE_BYTES`], oversize = skipped with a visible note, never a
    /// corrupt truncation) and an image count cap
    /// ([`MAX_IMAGE_ATTACHMENTS`]). Download failures are logged and the
    /// attachment is skipped — the message itself is still actionable.
    async fn inline_text_attachments(&self, msg: &mut Message) {
        if msg.attachments.is_empty() {
            return;
        }
        let mut total = 0usize;
        let mut appended = String::new();
        let mut images_saved = 0usize;
        for (idx, att) in msg.attachments.iter().enumerate() {
            // Images: download to /tmp/aa-img-* and reference via `IMAGE:`
            // marker lines (the cross-provider convention in
            // augmentagent_channel_core::images) instead of dropping them
            // silently — claude's Read renders images natively, and a codex
            // failover attaches them via `-i`. Previously every image DM'd
            // to the bot was invisible to the agent.
            if att.is_image_like() {
                if images_saved >= MAX_IMAGE_ATTACHMENTS {
                    appended.push_str(&format!(
                        "\n[Attachment: {} skipped — image cap ({MAX_IMAGE_ATTACHMENTS}) reached]\n",
                        att.filename
                    ));
                    continue;
                }
                match self
                    .client
                    .download_attachment(&att.url, MAX_IMAGE_BYTES)
                    .await
                {
                    Ok((bytes, truncated)) => {
                        if truncated {
                            // A truncated image is a corrupt image — say so
                            // rather than hand the model a broken file.
                            appended.push_str(&format!(
                                "\n[Attachment: {} skipped — larger than the {} image cap]\n",
                                att.filename,
                                MAX_IMAGE_BYTES
                            ));
                            continue;
                        }
                        let ext = std::path::Path::new(&att.filename)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("png")
                            .to_ascii_lowercase();
                        let path = std::path::PathBuf::from(format!(
                            "/tmp/aa-img-{}-{idx}.{ext}",
                            msg.id
                        ));
                        match tokio::fs::write(&path, &bytes).await {
                            Ok(()) => {
                                images_saved += 1;
                                appended.push_str(&format!(
                                    "\n{}\n",
                                    augmentagent_channel_core::image_marker_line(&path)
                                ));
                            }
                            Err(e) => warn!(
                                filename = %att.filename,
                                "failed to write image tempfile {}: {e}",
                                path.display()
                            ),
                        }
                    }
                    Err(e) => warn!(
                        filename = %att.filename,
                        url = %att.url,
                        "failed to download discord image attachment: {e}"
                    ),
                }
                continue;
            }
            if !att.is_text_like() {
                debug!(filename = %att.filename, "skipping non-text discord attachment");
                continue;
            }
            if total >= MAX_ATTACHMENTS_TOTAL_BYTES {
                appended.push_str(&format!(
                    "\n\n[Attachment: {} skipped — message attachment-total cap reached]\n",
                    att.filename
                ));
                continue;
            }
            let remaining_total = MAX_ATTACHMENTS_TOTAL_BYTES - total;
            let cap = MAX_ATTACHMENT_BYTES.min(remaining_total);
            match self.client.download_attachment(&att.url, cap).await {
                Ok((bytes, truncated)) => {
                    total += bytes.len();
                    let body = String::from_utf8_lossy(&bytes);
                    let trail = if truncated { "\n[truncated]\n" } else { "\n" };
                    appended.push_str(&format!(
                        "\n\n[Attachment: {} ({} bytes{})]\n{}{}",
                        att.filename,
                        bytes.len(),
                        if truncated { ", truncated" } else { "" },
                        body,
                        trail,
                    ));
                }
                Err(e) => {
                    warn!(
                        filename = %att.filename,
                        url = %att.url,
                        "failed to download discord attachment: {e}"
                    );
                }
            }
        }
        if !appended.is_empty() {
            msg.content.push_str(&appended);
        }
    }

    async fn handle_priority(
        &self,
        email: Email,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let wiki_hint = self.wiki_hint_for_sender(&email);

        // Triage
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
                // --- CODE-MODE attempt (I9). Two-step flow:
                //   1. Reasoner emits a TypeScript program that orchestrates
                //      tool calls and ends with `tools.draft("discord", body, reason)`.
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
                //
                // The Discord DM channel doesn't synthesise tone/thread/
                // archetype/resolved-asks blocks (those are email-only today),
                // so we pass empty strings — `code_mode_user_message` skips
                // empty blocks byte-for-byte.
                let manifest = manifest_v1();
                let system_prompt = code_mode_system(&manifest);
                let user_msg = code_mode_user_message(&email, &wiki_hint, "", "", "", "");
                // Opts mirror the classic `draft_opts` shape: same permission
                // mode, no allowed_tools / add_dirs — the Deno sandbox is the
                // tool surface, not the host claude CLI's Read/Grep/Glob.
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
                    channel: "discord".to_string(),
                    email: email.clone(),
                    account_id: email.account_entity_id.clone(),
                };

                // Attempt 1: original program. Capture the source on a
                // `NoCodeBlock`-vs-`RunnerError` distinction so the failure
                // handler can pass the program text to the repair prompt.
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
                    .with_wiki_hint(wiki_hint.clone());
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
                            channel: "discord".to_string(),
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

                // --- Code-mode success path: read the persisted draft body
                // out of the actions row the dispatcher just wrote, then run
                // the existing discord-dm approval-card flow against that
                // action id.
                if let Some(action_id) = code_mode_action_id {
                    let drafted = self
                        .store
                        .get_action_with_email(&action_id)?
                        .and_then(|a| a.action.draft_body)
                        .unwrap_or_default();

                    if self.config.dry_run {
                        // Promote the dispatcher's `Pending` row to `DryRun`
                        // so the dry-run accounting matches classic. The
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
                            "[discord reply dry-run:code] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
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
                        outcome.priority_replied_dry_run += 1;
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
                    info!(action_id, message_id = %email.message_id, "discord approval card posted (code-mode)");
                    outcome.priority_awaiting_approval += 1;
                    return Ok(());
                }

                // --- Classic fallback (I7). Reached when code-mode failed AND
                // self-repair didn't produce a working code-mode draft.
                // Behaviour matches the pre-code-mode classic path — same
                // draft call, same dispatch — except we then call
                // `report_classic_fallback` to file the postmortem gh issue
                // and post the Discord notice tagged with the classic
                // action_id.
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
                        "[discord reply dry-run] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
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
                            wiki_hint: wiki_hint.clone(),
                            store: Arc::clone(&self.store),
                            broker: Arc::clone(&self.approvals),
                            gh: Arc::clone(&self.gh_issue_runner),
                            email: email.clone(),
                            channel: "discord".to_string(),
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
                        channel: "discord".to_string(),
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
                info!(action_id, message_id = %email.message_id, "discord approval card posted");
                outcome.priority_awaiting_approval += 1;
                Ok(())
            }
            // Capture / Meeting are wave-A wiki-ingest-only kinds emitted by
            // the voice and gcal channels respectively — discord triage must
            // never produce them. Defensive skip if the model misbehaves.
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "discord triage returned non-message decision kind; treating as skip"
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
        let discord_id = extract_discord_id(&email.from).unwrap_or_default();
        if discord_id.is_empty() {
            return String::new();
        }
        match index.lookup(PLATFORM, &discord_id) {
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

/// Convert a Discord `Message` + its owning subscription into an `Email` row.
pub(crate) fn message_to_email(
    msg: &Message,
    sub: &ChannelSubscription,
    my_user_id: &str,
) -> Email {
    let author_label = msg.author.display_label();
    let from = format!("{} <discord:{}>", author_label, msg.author.id);
    let kind = match sub.mode {
        SubscriptionMode::Priority => "dm",
        SubscriptionMode::Digest | SubscriptionMode::StoreOnly => "digest_item",
    };
    Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: msg.id.clone(),
        thread_id: Some(msg.channel_id.clone()),
        from,
        subject: String::new(),
        body: msg.content.clone(),
        date: msg.timestamp.clone(),
        account_entity_id: Some(format!("{ACCOUNT_ENTITY_ID_PREFIX}:{my_user_id}")),
        platform: PLATFORM.to_string(),
        kind: kind.to_string(),
    }
}

/// Parse a Discord user id out of the `from` field shape
/// `"<display> <discord:<user_id>>"`. Returns `None` if the tag isn't present.
fn extract_discord_id(from: &str) -> Option<String> {
    let start = from.rfind("<discord:")? + "<discord:".len();
    let end = from[start..].find('>')?;
    Some(from[start..start + end].to_string())
}

/// ±JITTER_SECS around 0; non-crypto PRNG via nanosecond tail.
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

    /// Disable `gh issue create` for this test binary — channel tests build
    /// the production channel (which wires GhCliIssueRunner), and without
    /// this every `cargo test --workspace` files REAL postmortem issues on
    /// the repo (#780: ~70 filed by test runs). Mirrors the email crate.
    static GH_DISABLE_INIT: std::sync::Once = std::sync::Once::new();
    fn disable_gh_for_tests() {
        GH_DISABLE_INIT.call_once(|| {
            std::env::set_var("AUGMENTAGENT_GH_DISABLE", "1");
        });
    }
    use super::*;
    use crate::types::{Attachment, User};
    use augmentagent_channel_core::ReasonerOpts;
    use augmentagent_store::{ChannelSubscription, SubscriptionMode};

    fn sub_with_mode(mode: SubscriptionMode) -> ChannelSubscription {
        ChannelSubscription {
            id: "sub1".into(),
            platform: PLATFORM.into(),
            channel_id: "ch1".into(),
            display_name: "test dm".into(),
            mode,
            active: true,
            account_id: None,
            last_seen_message_id: None,
            last_digest_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn sample_msg(id: &str, content: &str) -> Message {
        Message {
            id: id.into(),
            channel_id: "ch1".into(),
            author: User {
                id: "peer".into(),
                username: "alice".into(),
                global_name: Some("Alice Wonder".into()),
                bot: false,
            },
            content: content.into(),
            timestamp: "2026-04-21T00:00:00+00:00".into(),
            edited_timestamp: None,
            attachments: Vec::<Attachment>::new(),
            message_type: 0,
        }
    }

    #[test]
    fn message_to_email_priority_uses_dm_kind() {
        let msg = sample_msg("m1", "hi");
        let sub = sub_with_mode(SubscriptionMode::Priority);
        let email = message_to_email(&msg, &sub, "me");
        assert_eq!(email.platform, "discord");
        assert_eq!(email.kind, "dm");
        assert_eq!(email.thread_id.as_deref(), Some("ch1"));
        assert!(email.from.contains("<discord:peer>"));
        assert!(email.from.starts_with("Alice Wonder"));
    }

    #[test]
    fn message_to_email_digest_uses_digest_item_kind() {
        let msg = sample_msg("m2", "hi");
        let sub = sub_with_mode(SubscriptionMode::Digest);
        let email = message_to_email(&msg, &sub, "me");
        assert_eq!(email.kind, "digest_item");
    }

    #[test]
    fn message_to_email_store_only_uses_digest_item_kind() {
        let msg = sample_msg("m3", "hi");
        let sub = sub_with_mode(SubscriptionMode::StoreOnly);
        let email = message_to_email(&msg, &sub, "me");
        assert_eq!(email.kind, "digest_item");
    }

    #[test]
    fn extract_discord_id_parses_from_tag() {
        assert_eq!(
            extract_discord_id("Alice Wonder <discord:12345>"),
            Some("12345".into())
        );
        assert_eq!(extract_discord_id("alice@example.com"), None);
    }

    #[test]
    fn jitter_stays_in_window() {
        for _ in 0..100 {
            let j = jitter_secs();
            assert!(j <= 2 * JITTER_SECS);
        }
    }

    /// Stub reasoner that returns a scripted response per call. Mirrors the
    /// pattern used in LinkedIn / Gmail channel tests.
    struct ScriptedReasoner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(resps: I) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    resps.into_iter().map(String::from).collect(),
                ),
            }
        }
    }

    #[async_trait::async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(
            &self,
            _opts: &ReasonerOpts,
            _user_message: &str,
        ) -> anyhow::Result<String> {
            let mut q = self.responses.lock().unwrap();
            Ok(q.pop_front()
                .unwrap_or_else(|| r#"{"decision":"skip","reason":"stub"}"#.to_string()))
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
            _action_id: &str,
            _email: &augmentagent_store::Email,
            _draft: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.approvals.lock().unwrap() += 1;
            Ok(())
        }

        async fn post_flag_notice(
            &self,
            _email: &augmentagent_store::Email,
            _reason: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.flags.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        use rusqlite::Connection;
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            // Mirror src/db.ts schema for the tables our tests exercise.
            // `Store::migrate` adds the platform/kind columns + channel_subscriptions.
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY,
                    messageId TEXT NOT NULL,
                    threadId TEXT,
                    fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    originalBody TEXT,
                    draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending',
                    errorMessage TEXT,
                    createdAt INTEGER NOT NULL,
                    updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY,
                    threadId TEXT,
                    fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    body TEXT,
                    receivedAt TEXT,
                    accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL,
                    triageResult TEXT,
                    agentProcessedAt INTEGER
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
    ) -> DiscordChannel<R> {
        // DiscordClient::new needs tokio runtime-alive; give it a throwaway
        // auth. Tests don't actually invoke the client (we call
        // `handle_priority` directly), so network never gets touched.
        let auth = crate::auth::DiscordAuth {
            user_id: "me".into(),
            token: "t".into(),
            super_properties_b64: "eyJvcyI6Ik1hYyJ9".into(),
            user_agent: "test".into(),
        };
        let client = Arc::new(DiscordClient::new(auth).unwrap());
        disable_gh_for_tests();
        DiscordChannel::new(
            store,
            client,
            reasoner,
            approvals,
            "me".into(),
            DiscordChannelConfig {
                dry_run: false,
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("skills/discord-triage"),
                poll_interval: Duration::from_secs(1),
            },
            None,
        )
    }

    #[tokio::test]
    async fn priority_skip_records_skipped_action() {
        let (store, _file) = tmp_store();
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"newsletter-ish"}"#,
        ]));
        let broker: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = build_channel(store.clone(), reasoner, Arc::clone(&broker));

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let msg = sample_msg("m1", "spam");
        let email = message_to_email(&msg, &sub, &ch.my_user_id);
        store.upsert_email(&email).unwrap();

        let mut outcome = PollOutcome::default();
        ch.handle_priority(email, &mut outcome).await.unwrap();

        assert_eq!(outcome.priority_skipped, 1);
        assert_eq!(outcome.priority_awaiting_approval, 0);
    }

    #[tokio::test]
    async fn priority_reply_posts_approval() {
        let (store, _file) = tmp_store();
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"asks a question"}"#,
            "sure, happy to help",
        ]));
        let counting = Arc::new(CountingBroker::default());
        let broker: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), reasoner, broker);

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let msg = sample_msg("m1", "do you have 15 min tomorrow?");
        let email = message_to_email(&msg, &sub, &ch.my_user_id);
        store.upsert_email(&email).unwrap();

        let mut outcome = PollOutcome::default();
        ch.handle_priority(email, &mut outcome).await.unwrap();

        assert_eq!(outcome.priority_awaiting_approval, 1);
        assert_eq!(*counting.approvals.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn priority_flag_posts_flag_notice() {
        let (store, _file) = tmp_store();
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"unclear ask"}"#,
        ]));
        let counting = Arc::new(CountingBroker::default());
        let broker: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), reasoner, broker);

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let msg = sample_msg("m1", "hey");
        let email = message_to_email(&msg, &sub, &ch.my_user_id);
        store.upsert_email(&email).unwrap();

        let mut outcome = PollOutcome::default();
        ch.handle_priority(email, &mut outcome).await.unwrap();

        assert_eq!(outcome.priority_flagged, 1);
        assert_eq!(*counting.flags.lock().unwrap(), 1);
    }
}
