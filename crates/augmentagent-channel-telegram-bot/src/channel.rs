//! `TelegramBotChannel` — long-poll `getUpdates` for every active row in
//! `telegram_bots`, dispatch each inbound message via `channel_subscriptions`
//! into the shared triage→draft→approval pipeline.
//!
//! Mirrors the [`SlackChannel`] structure 1:1 so debugging one means
//! debugging both.
//!
//! ## Cursor / dedup
//!
//! Telegram's `getUpdates` is offset-based: pass `offset = last_seen + 1`
//! and the server only returns updates with a strictly greater id. The
//! cursor is per-bot, not per-chat, so it lives in the `telegram_bots`
//! table (column `last_update_id`) rather than `channel_subscriptions`.
//! After each batch we persist `max(update_id) + 1` for the next call.
//!
//! ## Allowlist
//!
//! Inbound messages are only triaged if (a) they come from the bot owner's
//! DM (`owner_chat_id`) or (b) the chat has an explicit row in
//! `channel_subscriptions`. Any other chat that messages the bot is logged
//! at debug and dropped — bots are publicly addressable, so a default-open
//! channel would be a triage-loop and a DoS surface.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{InboundSource, WorkItem};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{
    ActionStatus, ChannelSubscription, Email, Store, SubscriptionMode, TelegramBot, TriageResult,
    NUDGE_INTERVAL_MS,
};
use augmentagent_wiki::IdentityIndex;

use crate::api::{TelegramBotClient, TelegramBotError, DEFAULT_LONG_POLL_SECS};
use crate::auth::TelegramBotAuth;
use crate::types::{Message, Update};
use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// Per-bot runtime handle: a `TelegramBotClient` + the `bot_id` and
/// `owner_chat_id` we read out of the keychain at boot.
pub struct BotHandle {
    pub bot_id: i64,
    pub bot_username: String,
    pub owner_chat_id: i64,
    pub client: Arc<TelegramBotClient>,
}

/// 4h cadence in line with the rest of the channels. Telegram itself is
/// fine with much faster polling (long-poll explicitly supports 50s
/// timeouts), but the orchestrator-level cadence is what the digest /
/// triage budget is sized for.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;
pub const JITTER_SECS: u64 = 30 * 60;

/// Cap on updates per `getUpdates` call. Telegram returns at most 100 by
/// default; we ask for the same so a single batch never balloons unbounded.
pub const MAX_UPDATES_PER_TICK: u32 = 100;

#[derive(Clone, Debug)]
pub struct TelegramBotChannelConfig {
    pub poll_interval: Duration,
    /// `true` = generate drafts but don't post approval cards / send.
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub skill_dir: PathBuf,
    /// Long-poll timeout passed to `getUpdates`. `0` means short-poll
    /// (used by `--dry-run` PollOnce so the CLI returns immediately).
    pub long_poll_secs: i64,
}

impl Default for TelegramBotChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/telegram-triage"),
            long_poll_secs: DEFAULT_LONG_POLL_SECS,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub bots_polled: usize,
    pub updates_seen: usize,
    pub messages_dispatched: usize,
    pub priority_skipped: usize,
    pub priority_flagged: usize,
    pub priority_replied_dry_run: usize,
    pub priority_awaiting_approval: usize,
    pub digest_stored: usize,
    pub store_only_stored: usize,
    pub disallowed_chats_dropped: usize,
    pub errors: usize,
}

pub struct TelegramBotChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: TelegramBotChannelConfig,
    pub identity_index: Option<Arc<IdentityIndex>>,
    wiki_schema: Option<String>,
}

impl<R: Reasoner + 'static> TelegramBotChannel<R> {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        config: TelegramBotChannelConfig,
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
        }
    }

    /// Build a per-bot handle map from the active rows in `telegram_bots`.
    /// Each row's keychain slot is loaded; failures are logged but don't
    /// abort the tick — other bots can still poll.
    fn load_bot_handles(&self) -> HashMap<i64, BotHandle> {
        let mut map = HashMap::new();
        let bots = match self.store.list_active_telegram_bots() {
            Ok(b) => b,
            Err(e) => {
                error!("list_active_telegram_bots failed: {e:#}");
                return map;
            }
        };
        if bots.is_empty() {
            debug!("no active telegram_bots rows — nothing to poll");
            return map;
        }
        for bot in bots {
            match load_bot_handle(&bot) {
                Some(handle) => {
                    map.insert(handle.bot_id, handle);
                }
                None => warn!(
                    bot_id = bot.bot_id,
                    bot_username = %bot.bot_username,
                    "skipping bot: auth not available",
                ),
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
                    info!("telegram-bot channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "telegram-bot poll complete"),
                        Err(e) => error!("telegram-bot poll failed: {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let bots = self.load_bot_handles();
        if bots.is_empty() {
            debug!("no telegram bots loaded; nothing to poll");
            return Ok(outcome);
        }
        outcome.bots_polled = bots.len();

        // Pre-fetch active subscriptions once and index by chat_id (string).
        // The dispatcher iterates updates, not subs, so an O(1) lookup keeps
        // the hot path cheap.
        let subs = self.store.list_active_subscriptions(PLATFORM)?;
        let subs_by_chat = index_subs_by_chat(&subs);

        for (_bot_id, handle) in bots.iter() {
            if let Err(e) = self
                .poll_bot(handle, &subs_by_chat, &mut outcome)
                .await
            {
                outcome.errors += 1;
                error!(
                    bot_id = handle.bot_id,
                    bot_username = %handle.bot_username,
                    "telegram bot poll failed: {e:#}"
                );
            }
        }
        Ok(outcome)
    }

    async fn poll_bot(
        &self,
        handle: &BotHandle,
        subs_by_chat: &HashMap<String, &ChannelSubscription>,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let last_update_id = self
            .store
            .get_telegram_bot_by_id(handle.bot_id)?
            .map(|b| b.last_update_id)
            .unwrap_or(0);
        let offset = if last_update_id > 0 {
            Some(last_update_id + 1)
        } else {
            None
        };
        let updates = match handle
            .client
            .get_updates(offset, self.config.long_poll_secs)
            .await
        {
            Ok(u) => u,
            Err(TelegramBotError::Api { code, description })
                if code == 401 || description.contains("Unauthorized") =>
            {
                warn!(
                    bot_id = handle.bot_id,
                    "telegram auth invalid — token revoked? re-run `augmentagent telegram-bot login`"
                );
                anyhow::bail!("telegram unauthorized: {description}");
            }
            Err(e) => return Err(e.into()),
        };

        outcome.updates_seen += updates.len();
        let mut newest_seen = last_update_id;
        for update in updates {
            if update.update_id > newest_seen {
                newest_seen = update.update_id;
            }
            // We accept message / edited_message / channel_post but only
            // route plain `message` into the triage pipeline. Edits and
            // channel posts are ack-only for now (they advance the cursor
            // but don't produce work items).
            let Some(msg) = update.message else {
                continue;
            };
            // Drop our own bot's outbound (Telegram echoes via getUpdates if
            // the bot was added to a group it then sends in).
            if msg
                .from
                .as_ref()
                .map(|u| u.id == handle.bot_id || u.is_bot)
                .unwrap_or(false)
            {
                continue;
            }
            match self
                .dispatch_message(handle, &msg, subs_by_chat, outcome)
                .await
            {
                Ok(()) => outcome.messages_dispatched += 1,
                Err(e) => {
                    outcome.errors += 1;
                    error!(
                        bot_id = handle.bot_id,
                        chat_id = msg.chat.id,
                        message_id = msg.message_id,
                        "telegram dispatch failed: {e:#}"
                    );
                }
            }
        }

        if newest_seen > last_update_id {
            self.store
                .update_telegram_bot_last_update_id(handle.bot_id, newest_seen)?;
        }
        Ok(())
    }

    async fn dispatch_message(
        &self,
        handle: &BotHandle,
        msg: &Message,
        subs_by_chat: &HashMap<String, &ChannelSubscription>,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let chat_id_str = msg.chat.id.to_string();
        // Prefer an explicit subscription if present; otherwise fall back to
        // the owner DM (Priority by definition). The owner-fallback row is
        // synthesized and not persisted — it's purely a routing convenience
        // so the user can start triaging immediately after `login` without
        // having to manually subscribe their own DM.
        let synthesized;
        let sub: &ChannelSubscription = match subs_by_chat.get(chat_id_str.as_str()) {
            Some(s) => s,
            None if msg.chat.id == handle.owner_chat_id => {
                synthesized = synthesize_owner_subscription(handle, msg);
                &synthesized
            }
            None => {
                outcome.disallowed_chats_dropped += 1;
                debug!(
                    chat_id = msg.chat.id,
                    "telegram message from non-allowlisted chat dropped"
                );
                return Ok(());
            }
        };

        let body = msg.body_text();
        if body.is_empty() && msg.voice.is_none() {
            // Pure-media (no text, no caption, not a voice memo) — nothing
            // useful to feed the reasoner. Skip without an error.
            return Ok(());
        }

        let email = message_to_email(handle, msg, sub);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(());
        }

        match sub.mode {
            SubscriptionMode::Priority => self.handle_priority(handle, msg, email, outcome).await,
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
        _handle: &BotHandle,
        _msg: &Message,
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
                error!(message_id = %email.message_id, "telegram triage parse failed: {e}; raw={raw}");
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
                let skill_system =
                    std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                        .unwrap_or_default();
                let draft = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "", "", "", "");
                let drafted = match self.reasoner.call(&draft, &draft_prompt).await {
                    Ok(s) => s.trim().to_string(),
                    Err(e) => {
                        error!(message_id = %email.message_id, "telegram draft call failed: {e}");
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
                    self.store.log_action(
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
                        "[telegram reply dry-run] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
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

                let action_id = self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&drafted),
                    ActionStatus::Pending,
                )?;
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
                info!(action_id, message_id = %email.message_id, "telegram approval card posted");
                outcome.priority_awaiting_approval += 1;
                Ok(())
            }
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "telegram triage returned non-message decision kind; treating as skip"
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
        let tg_id = extract_telegram_user_id(&email.from).unwrap_or_default();
        if tg_id.is_empty() {
            return String::new();
        }
        match index.lookup(PLATFORM, &tg_id) {
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

/// `InboundSource` adapter — exposed so other crates (or tests) can wrap
/// the bot in `InboundMessageTrigger` if they want a `Trigger`-shaped
/// surface instead of calling `poll_once` directly.
///
/// Note: the production path uses [`TelegramBotChannel::poll_once`] which
/// does its own dispatch + triage; this adapter is the "raw inbox" view
/// downstream code can use to consume `WorkItem`s without the reasoner.
pub struct TelegramBotInbound {
    pub client: Arc<TelegramBotClient>,
    pub store: Arc<Store>,
    pub bot_id: i64,
    /// Long-poll timeout (seconds). 0 = short poll.
    pub long_poll_secs: i64,
}

#[async_trait]
impl InboundSource for TelegramBotInbound {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        let last_update_id = self
            .store
            .get_telegram_bot_by_id(self.bot_id)?
            .map(|b| b.last_update_id)
            .unwrap_or(0);
        let offset = if last_update_id > 0 {
            Some(last_update_id + 1)
        } else {
            None
        };
        let updates = self.client.get_updates(offset, self.long_poll_secs).await?;
        let mut items = Vec::with_capacity(updates.len());
        let mut newest_seen = last_update_id;
        for update in updates {
            if update.update_id > newest_seen {
                newest_seen = update.update_id;
            }
            if let Some(item) = update_to_work_item(&update) {
                items.push(item);
            }
        }
        if newest_seen > last_update_id {
            self.store
                .update_telegram_bot_last_update_id(self.bot_id, newest_seen)?;
        }
        Ok(items)
    }
}

/// Build the `WorkItem` shape spec'd in #74 §4. The payload carries the
/// full `Update` JSON so downstream handlers can reconstruct rich state
/// (reply chains, captions) without re-fetching.
pub fn update_to_work_item(update: &Update) -> Option<WorkItem> {
    let msg = update.message.as_ref()?;
    let external_id = format!("tg:{}:{}", msg.chat.id, msg.message_id);
    let payload = serde_json::json!({
        "update_id": update.update_id,
        "message": msg,
    });
    let kind = if msg.chat.is_private() {
        "dm"
    } else {
        "digest_item"
    };
    Some(WorkItem {
        platform: PLATFORM.to_string(),
        kind: kind.to_string(),
        external_id,
        payload,
    })
}

/// Convert a Telegram `Message` + its owning subscription into an `Email`
/// row. Mirrors the Slack `message_to_email` shape so the existing actions
/// table + approval cards work without a schema change.
///
/// `from` is `"<display> <telegram:<user_id>>"` so the wiki identity-index
/// lookup can pull the platform user id back out at triage time.
/// `account_entity_id` carries `telegram:bot:<bot_id>` so the approval
/// handler can route Approve back through the right bot's client.
pub fn message_to_email(
    handle: &BotHandle,
    msg: &Message,
    sub: &ChannelSubscription,
) -> Email {
    let (from_label, from_id) = match msg.from.as_ref() {
        Some(u) => (u.display_label(), u.id),
        None => (msg.chat.display_label(), msg.chat.id),
    };
    let from = format!("{} <telegram:{}>", from_label, from_id);
    let kind = match sub.mode {
        SubscriptionMode::Priority => {
            if msg.chat.is_private() {
                "dm"
            } else {
                // Mention-in-group routes to dm too (the user explicitly
                // subscribed at Priority); other group chatter goes through
                // Digest/StoreOnly.
                "dm"
            }
        }
        SubscriptionMode::Digest | SubscriptionMode::StoreOnly => "digest_item",
    };
    Email {
        message_id: format!("tg:{}:{}", msg.chat.id, msg.message_id),
        thread_id: Some(msg.chat.id.to_string()),
        from,
        subject: String::new(),
        body: msg.body_text().to_string(),
        date: msg.date.to_string(),
        account_entity_id: Some(format!(
            "{ACCOUNT_ENTITY_ID_PREFIX}:bot:{}",
            handle.bot_id
        )),
        platform: PLATFORM.to_string(),
        kind: kind.to_string(),
    }
}

/// Synthesize a Priority subscription on the fly for the bot owner's DM.
/// Lets the user start triaging immediately without having to manually
/// `subscribe` their own chat after `login`.
fn synthesize_owner_subscription(handle: &BotHandle, msg: &Message) -> ChannelSubscription {
    ChannelSubscription {
        id: format!("__synthesized:{}:{}", handle.bot_id, msg.chat.id),
        platform: PLATFORM.to_string(),
        channel_id: msg.chat.id.to_string(),
        display_name: format!("DM with owner ({})", handle.owner_chat_id),
        mode: SubscriptionMode::Priority,
        active: true,
        account_id: Some(handle.bot_id.to_string()),
        last_seen_message_id: None,
        last_digest_at_ms: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn index_subs_by_chat(
    subs: &[ChannelSubscription],
) -> HashMap<String, &ChannelSubscription> {
    let mut map = HashMap::with_capacity(subs.len());
    for s in subs {
        map.insert(s.channel_id.clone(), s);
    }
    map
}

fn load_bot_handle(bot: &TelegramBot) -> Option<BotHandle> {
    let auth = match TelegramBotAuth::load_with_file_fallback(&bot.bot_username) {
        Ok(a) => a,
        Err(e) => {
            warn!(
                bot_username = %bot.bot_username,
                "telegram auth load failed: {e}"
            );
            return None;
        }
    };
    match TelegramBotClient::new(auth.bot_token.clone()) {
        Ok(c) => Some(BotHandle {
            bot_id: auth.bot_id,
            bot_username: auth.bot_username,
            owner_chat_id: auth.owner_chat_id,
            client: Arc::new(c),
        }),
        Err(e) => {
            warn!(
                bot_username = %bot.bot_username,
                "telegram client build failed: {e}"
            );
            None
        }
    }
}

/// Parse a Telegram user id out of the `from` field shape
/// `"<display> <telegram:<user_id>>"`.
pub fn extract_telegram_user_id(from: &str) -> Option<String> {
    let start = from.rfind("<telegram:")? + "<telegram:".len();
    let end = from[start..].find('>')?;
    Some(from[start..start + end].to_string())
}

/// Parse the routed `bot_id` out of `account_entity_id` shape
/// `"telegram:bot:<bot_id>"`. Used by the approval handler to pick the
/// right outbound client.
pub fn extract_bot_id(account_entity_id: &str) -> Option<i64> {
    let suffix = account_entity_id.strip_prefix("telegram:bot:")?;
    suffix.parse().ok()
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
    use crate::types::{Chat, User};
    use augmentagent_channel_core::ReasonerOpts;

    fn handle() -> BotHandle {
        BotHandle {
            bot_id: 99,
            bot_username: "triage_bot".into(),
            owner_chat_id: 12345,
            client: Arc::new(TelegramBotClient::new("123:ABC").unwrap()),
        }
    }

    fn sample_msg(message_id: i64, chat_id: i64, text: &str) -> Message {
        Message {
            message_id,
            date: 1_747_200_000,
            chat: Chat {
                id: chat_id,
                chat_type: "private".into(),
                title: None,
                username: Some("alice".into()),
                first_name: Some("Alice".into()),
                last_name: None,
            },
            from: Some(User {
                id: chat_id,
                is_bot: false,
                first_name: "Alice".into(),
                last_name: None,
                username: Some("alice".into()),
            }),
            text: Some(text.into()),
            caption: None,
            voice: None,
            reply_to_message_id: None,
            reply_to_message: None,
        }
    }

    fn sub(mode: SubscriptionMode, chat_id: &str) -> ChannelSubscription {
        ChannelSubscription {
            id: "sub1".into(),
            platform: PLATFORM.into(),
            channel_id: chat_id.into(),
            display_name: "DM".into(),
            mode,
            active: true,
            account_id: Some("99".into()),
            last_seen_message_id: None,
            last_digest_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn message_to_email_priority_uses_dm_kind_and_tags_user_id() {
        let m = sample_msg(7, 12345, "hi");
        let s = sub(SubscriptionMode::Priority, "12345");
        let h = handle();
        let e = message_to_email(&h, &m, &s);
        assert_eq!(e.platform, "telegram");
        assert_eq!(e.kind, "dm");
        assert_eq!(e.message_id, "tg:12345:7");
        assert_eq!(e.thread_id.as_deref(), Some("12345"));
        assert!(e.from.contains("<telegram:12345>"));
        assert_eq!(e.account_entity_id.as_deref(), Some("telegram:bot:99"));
    }

    #[test]
    fn message_to_email_digest_uses_digest_item_kind() {
        let m = sample_msg(7, 12345, "hi");
        let s = sub(SubscriptionMode::Digest, "12345");
        let h = handle();
        let e = message_to_email(&h, &m, &s);
        assert_eq!(e.kind, "digest_item");
    }

    #[test]
    fn extract_telegram_user_id_parses_tag() {
        assert_eq!(
            extract_telegram_user_id("Alice <telegram:12345>"),
            Some("12345".into())
        );
        assert_eq!(extract_telegram_user_id("no-tag"), None);
    }

    #[test]
    fn extract_bot_id_parses_account_entity_id() {
        assert_eq!(extract_bot_id("telegram:bot:99"), Some(99));
        assert_eq!(extract_bot_id("telegram:user:99"), None);
        assert_eq!(extract_bot_id("slack:T1"), None);
    }

    #[test]
    fn update_to_work_item_emits_dm_for_private_chat() {
        let u = Update {
            update_id: 100001,
            message: Some(sample_msg(42, 12345, "ping")),
            edited_message: None,
            channel_post: None,
        };
        let item = update_to_work_item(&u).unwrap();
        assert_eq!(item.platform, "telegram");
        assert_eq!(item.kind, "dm");
        assert_eq!(item.external_id, "tg:12345:42");
        assert_eq!(item.payload["update_id"].as_i64(), Some(100001));
        assert_eq!(item.payload["message"]["text"].as_str(), Some("ping"));
    }

    #[test]
    fn jitter_stays_in_window() {
        for _ in 0..50 {
            assert!(jitter_secs() <= 2 * JITTER_SECS);
        }
    }

    /// Scripted reasoner — copied verbatim from slack's test module per #74 §7.
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
            _e: &Email,
            _d: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.approvals.lock().unwrap() += 1;
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _e: &Email,
            _r: &str,
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
    ) -> TelegramBotChannel<R> {
        TelegramBotChannel::new(
            store,
            reasoner,
            approvals,
            TelegramBotChannelConfig {
                dry_run: false,
                poll_interval: Duration::from_secs(1),
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("skills/telegram-triage"),
                long_poll_secs: 0,
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
        let h = handle();
        let m = sample_msg(1, 12345, "spam");
        let s = sub(SubscriptionMode::Priority, "12345");
        let e = message_to_email(&h, &m, &s);
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(&h, &m, e, &mut out).await.unwrap();
        assert_eq!(out.priority_skipped, 1);
    }

    #[tokio::test]
    async fn priority_reply_posts_approval() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            "tomorrow at 3 works",
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), r, b);
        let h = handle();
        let m = sample_msg(2, 12345, "15 min tomorrow?");
        let s = sub(SubscriptionMode::Priority, "12345");
        let e = message_to_email(&h, &m, &s);
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(&h, &m, e, &mut out).await.unwrap();
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
        let h = handle();
        let m = sample_msg(3, 12345, "?");
        let s = sub(SubscriptionMode::Priority, "12345");
        let e = message_to_email(&h, &m, &s);
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(&h, &m, e, &mut out).await.unwrap();
        assert_eq!(out.priority_flagged, 1);
        assert_eq!(*counting.flags.lock().unwrap(), 1);
    }
}
