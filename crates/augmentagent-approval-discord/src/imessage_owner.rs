//! The owner's control conversation is a surface, never a contact auto-replier.
use crate::imessage_owner_control::{self as owner_control, Control, Verb};
use crate::{
    ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalCardSurface,
    ApprovalError, AuditCtx, QueryHandler,
};
use async_trait::async_trait;
use augmentagent_store::{
    imessage_owner::{with_owner_decision, OwnerEvent},
    Email, Store,
};
use std::sync::Arc;

tokio::task_local! { static OWNER_CHAT: bool; }
pub fn owner_chat_active() -> bool {
    OWNER_CHAT.try_with(|v| *v).unwrap_or(false)
}

pub const HELP:&str="Reply to a draft with approve, 👍, skip, or revise: your changes. Or use approve <ref>, skip <ref>, revise <ref>: changes. A native 👍 reaction approves only that exact pending draft. Revised drafts need fresh approval. Other card controls: fill <ref>: missing values; refine <ref>: preset-id; schedule <ref>: time; reschedule <ref>: time; send-now <ref>; cancel-schedule <ref>; back-to-queue <ref>; recompose <ref>; decline <ref> (calendar auto-RSVP). Send presets to list refinements. For ordinary questions, just text me.";

/// Only owner-visible fields; transport metadata and component IDs stay private.
pub fn visible_discord_message(value: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    if let Some(s) = value
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        lines.push(s.to_owned());
    }
    if let Some(embeds) = value.get("embeds").and_then(|v| v.as_array()) {
        for embed in embeds {
            for key in ["title", "description", "url"] {
                if let Some(s) = embed
                    .get(key)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    lines.push(s.into());
                }
            }
            if let Some(fields) = embed.get("fields").and_then(|v| v.as_array()) {
                for field in fields {
                    if let (Some(name), Some(text)) = (
                        field.get("name").and_then(|v| v.as_str()),
                        field.get("value").and_then(|v| v.as_str()),
                    ) {
                        lines.push(format!("{name}: {text}"));
                    }
                }
            }
            for key in ["image", "thumbnail"] {
                if let Some(url) = embed
                    .get(key)
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str())
                {
                    lines.push(url.into());
                }
            }
        }
    }
    if let Some(files) = value.get("attachments").and_then(|v| v.as_array()) {
        for file in files {
            if let Some(url) = file.get("url").and_then(|v| v.as_str()) {
                lines.push(format!(
                    "{}: {url}",
                    file.get("filename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Attachment")
                ));
            }
        }
    }
    lines.join("\n\n")
}

pub fn origin_digest(token: &str, owner: u64, query: u64, approval: u64) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(token, owner, query, approval)).expect("origin serializable")
        )
    )
}
pub fn configured_origin() -> Option<String> {
    let token = std::env::var("DISCORD_BOT_TOKEN").ok()?;
    let owner = std::env::var("DISCORD_ALLOWED_USER_ID")
        .ok()?
        .parse()
        .ok()?;
    let approval = std::env::var("DISCORD_CHANNEL_ID").ok()?.parse().ok()?;
    let query = std::env::var("DISCORD_QUERY_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(approval);
    Some(origin_digest(&token, owner, query, approval))
}

/// Public archives paginate by ISO timestamp; joined private archives by ID.
/// https://docs.discord.com/developers/resources/channel#list-public-archived-threads
async fn discover_threads(
    http: &serenity::http::Http,
    root: serenity::all::ChannelId,
    activated: i64,
) -> anyhow::Result<Vec<serenity::all::GuildChannel>> {
    use serenity::http::{LightMethod, Request, Route};
    let mut result = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let mut params = vec![("limit", "100".to_string())];
        if let Some(value) = before.as_ref() {
            params.push(("before", value.replace('+', "%2B")));
        }
        let data: serenity::all::ThreadsData = http
            .fire(
                Request::new(
                    Route::ChannelArchivedPublicThreads { channel_id: root },
                    LightMethod::Get,
                )
                .params(Some(params)),
            )
            .await?;
        let last = data
            .threads
            .last()
            .and_then(|c| c.thread_metadata.as_ref())
            .and_then(|m| m.archive_timestamp);
        let next = last.map(|t| t.to_string());
        result.extend(data.threads);
        if !data.has_more
            || next.is_none()
            || next == before
            || last.is_some_and(|t| t.unix_timestamp().saturating_mul(1000) < activated)
        {
            break;
        }
        before = next;
    }
    let mut before = None;
    loop {
        let data = root
            .get_joined_archived_private_threads(http, before, Some(100))
            .await?;
        let next = data.threads.last().map(|c| c.id.get());
        result.extend(data.threads);
        if !data.has_more || next.is_none() || next == before {
            break;
        }
        before = next;
    }
    Ok(result)
}

pub struct OwnerSurface {
    pub store: Arc<Store>,
    origin: Option<String>,
}
impl OwnerSurface {
    /// Gateway delivery is low latency; REST catch-up makes reconnects recover
    /// persisted owner output. Advance only after each message was durably queued.
    pub async fn catch_up(&self, http: &serenity::http::Http, offset: usize) -> anyhow::Result<()> {
        use serenity::all::{Channel, ChannelId, GetMessages, GuildId, MessageId};
        if !self.active() {
            return Ok(());
        }
        let Some(origin) = self.store.imessage_owner_context()? else {
            return Ok(());
        };
        let mut channels = std::collections::BTreeSet::new();
        let mut failures = Vec::new();
        for id in self.store.imessage_owner_discord_channels()? {
            if let Ok(id) = id.parse::<u64>() {
                channels.insert(id);
            }
        }
        if let Ok(threads) = GuildId::new(origin.guild_id).get_active_threads(http).await {
            for c in threads.threads {
                if c.parent_id.is_some_and(|p| {
                    [origin.channel_id, origin.approval_channel_id].contains(&p.get())
                }) {
                    channels.insert(c.id.get());
                }
            }
        }
        // Discover the owner's existing DM and archived native threads too;
        // neither is guaranteed to produce a gateway event after a reconnect.
        let dm = serenity::all::UserId::new(origin.owner_id)
            .create_dm_channel(http)
            .await?;
        channels.insert(dm.id.get());
        let activated = self
            .store
            .imessage_owner_binding()?
            .map(|b| b.activated_at_ms)
            .unwrap_or(i64::MAX);
        for root in [origin.channel_id, origin.approval_channel_id] {
            match discover_threads(http, ChannelId::new(root), activated).await {
                Ok(threads) => {
                    for thread in threads {
                        if thread.parent_id.is_some_and(|p| p.get() == root) {
                            channels.insert(thread.id.get());
                        }
                    }
                }
                Err(error) => failures.push(format!("thread discovery: {error}")),
            }
        }
        for id in &channels {
            if let Some(floor) = self.store.imessage_owner_discord_cursor(&id.to_string())? {
                self.store.advance_imessage_owner_discord_cursor(
                    origin.revision,
                    &id.to_string(),
                    &floor,
                )?;
            }
        }
        channels.remove(&origin.channel_id);
        channels.remove(&origin.approval_channel_id);
        let other = channels.into_iter().collect::<Vec<_>>();
        let mut selected = vec![origin.channel_id, origin.approval_channel_id];
        for i in 0..other.len().min(5) {
            selected.push(other[(offset + i) % other.len()]);
        }
        selected.sort_unstable();
        selected.dedup();
        for id in selected {
            let (parent, peer) = match ChannelId::new(id).to_channel(http).await {
                Ok(Channel::Guild(c)) => (c.parent_id.map(|p| p.get()), None),
                Ok(Channel::Private(c)) => (None, Some(c.recipient.id.get())),
                _ => continue,
            };
            let Some(cursor) = self.store.imessage_owner_discord_cursor(&id.to_string())? else {
                return Ok(());
            };
            let cursor = cursor.parse::<u64>()?;
            let mut messages = ChannelId::new(id)
                .messages(
                    http,
                    GetMessages::new().after(MessageId::new(cursor)).limit(100),
                )
                .await?;
            messages.sort_by_key(|m| m.id.get());
            for message in messages {
                self.mirror_discord(&message, parent, peer)?;
                self.store.advance_imessage_owner_discord_cursor(
                    origin.revision,
                    &id.to_string(),
                    &message.id.to_string(),
                )?;
            }
            // In-place edits keep their original message ID. Re-read a bounded
            // recent window as well as following the insertion cursor.
            let recent = ChannelId::new(id)
                .messages(http, GetMessages::new().limit(30))
                .await?;
            for message in recent {
                if message.edited_timestamp.is_some() {
                    self.mirror_discord(&message, parent, peer)?;
                }
            }
        }
        // In-place edits of an older known message may sit outside the recent
        // window after an outage. Revisit a rotating set without replaying it.
        for (channel, message) in self.store.imessage_owner_discord_sources(offset)? {
            let (parent, peer) = match ChannelId::new(channel).to_channel(http).await {
                Ok(Channel::Guild(c)) => (c.parent_id.map(|p| p.get()), None),
                Ok(Channel::Private(c)) => (None, Some(c.recipient.id.get())),
                _ => continue,
            };
            if let Ok(message) = ChannelId::new(channel)
                .message(http, MessageId::new(message))
                .await
            {
                if message.edited_timestamp.is_some() {
                    self.mirror_discord(&message, parent, peer)?;
                }
            }
        }
        anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }
    /// Called only with native Discord channel/parent metadata, never a target
    /// supplied in a user's message. DMs require their verified peer ID too.
    pub fn mirror_discord(
        &self,
        message: &serenity::all::Message,
        parent: Option<u64>,
        dm_peer: Option<u64>,
    ) -> Result<(), ApprovalError> {
        if !self.active() {
            return Ok(());
        }
        let Some(origin) = self
            .store
            .imessage_owner_context()
            .map_err(|e| ApprovalError::Discord(e.to_string()))?
        else {
            return Ok(());
        };
        if message.author.id.get() != origin.bot_id {
            return Ok(());
        }
        let channel = message.channel_id.get();
        let in_scope = if message.guild_id.is_none() {
            dm_peer == Some(origin.owner_id)
        } else {
            message.guild_id.map(|g| g.get()) == Some(origin.guild_id)
                && ([origin.channel_id, origin.approval_channel_id].contains(&channel)
                    || parent.is_some_and(|p| {
                        [origin.channel_id, origin.approval_channel_id].contains(&p)
                    }))
        };
        if !in_scope {
            return Ok(());
        }
        let Some(binding) = self
            .store
            .imessage_owner_binding()
            .map_err(|e| ApprovalError::Discord(e.to_string()))?
        else {
            return Ok(());
        };
        if let Some(floor) = self
            .store
            .imessage_owner_discord_cursor(&channel.to_string())
            .map_err(|e| ApprovalError::Discord(e.to_string()))?
        {
            self.store
                .advance_imessage_owner_discord_cursor(
                    binding.revision,
                    &channel.to_string(),
                    &floor,
                )
                .map_err(|e| ApprovalError::Discord(e.to_string()))?;
        }
        let created_ms = ((message.id.get() >> 22) + 1_420_070_400_000) as i64;
        let changed_ms = message
            .edited_timestamp
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t.to_string()).ok())
            .map(|t| t.timestamp_millis())
            .unwrap_or(created_ms);
        if changed_ms < binding.activated_at_ms {
            return Ok(());
        }
        for row in &message.components {
            for component in &row.components {
                if let serenity::all::ActionRowComponent::Button(button) = component {
                    let serenity::all::ButtonKind::NonLink { custom_id, .. } = &button.data else {
                        continue;
                    };
                    if let Some(custom) = crate::custom_id::CustomId::parse(custom_id) {
                        if matches!(
                            custom.verb,
                            crate::custom_id::Verb::SendNow
                                | crate::custom_id::Verb::CancelSchedule
                                | crate::custom_id::Verb::BackToQueue
                                | crate::custom_id::Verb::Recompose
                                | crate::custom_id::Verb::CalendarDecline
                        ) {
                            return self.post_state_card(&custom.action_id);
                        }

                        if matches!(
                            custom.verb,
                            crate::custom_id::Verb::Approve
                                | crate::custom_id::Verb::Revise
                                | crate::custom_id::Verb::Skip
                        ) {
                            if let Some(action) = self
                                .store
                                .get_action_with_email(&custom.action_id)
                                .map_err(|e| ApprovalError::Discord(e.to_string()))?
                            {
                                if action.action.status == "pending" {
                                    return self.post_card(
                                        &custom.action_id,
                                        &action.email,
                                        action.action.draft_body.as_deref().unwrap_or(""),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut text = visible_discord_message(
            &serde_json::to_value(message).map_err(|e| ApprovalError::Discord(e.to_string()))?,
        );
        let mut choices = std::collections::BTreeSet::new();
        for row in &message.components {
            for component in &row.components {
                if let serenity::all::ActionRowComponent::Button(button) = component {
                    if let serenity::all::ButtonKind::NonLink { custom_id, .. } = &button.data {
                        if let Some((verb, id)) = custom_id
                            .strip_prefix("oa:")
                            .and_then(|s| s.split_once(':'))
                        {
                            let word = if verb == "resolve" { "done" } else { verb };
                            choices.insert(format!("alert {word} {id}"));
                        } else if let Some(control) = crate::custom_id::CustomId::parse(custom_id) {
                            if matches!(
                                control.verb,
                                crate::custom_id::Verb::LoopAck
                                    | crate::custom_id::Verb::LoopDismiss
                            ) {
                                choices.insert(format!(
                                    "loop {} {}",
                                    if control.verb == crate::custom_id::Verb::LoopAck {
                                        "ack"
                                    } else {
                                        "dismiss"
                                    },
                                    control.action_id
                                ));
                            }
                        }
                    }
                }
            }
        }
        if !choices.is_empty() {
            text.push_str(&format!(
                "\nControls:\n{}",
                choices.into_iter().collect::<Vec<_>>().join("\n")
            ));
        }

        if text.trim().is_empty() {
            return Ok(());
        }
        let revision = message
            .edited_timestamp
            .map(|t| t.to_string())
            .unwrap_or_else(|| "original".into());
        self.output(
            binding.revision,
            &format!(
                "{}:{}:{}:{revision}",
                if message.guild_id.is_none() {
                    "discord-dm"
                } else if channel == origin.channel_id || parent == Some(origin.channel_id) {
                    "discord-query"
                } else {
                    "discord-notice"
                },
                message.channel_id,
                message.id
            ),
            &text,
        )
    }
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            origin: configured_origin(),
        }
    }
    pub fn for_origin(store: Arc<Store>, origin: String) -> Self {
        Self {
            store,
            origin: Some(origin),
        }
    }
    pub fn active(&self) -> bool {
        self.store
            .imessage_owner_context()
            .ok()
            .flatten()
            .is_some_and(|c| self.origin.as_deref() == Some(c.config_digest.as_str()))
    }
    fn output(&self, revision: i64, key: &str, body: &str) -> Result<(), ApprovalError> {
        if !self.active() {
            return Ok(());
        }
        let chars = body.chars().collect::<Vec<_>>();
        let pages = chars.chunks(250_000).collect::<Vec<_>>();
        for (index, page) in pages.iter().enumerate() {
            let key = if pages.len() > 1 {
                format!("{key}:page:{}", index + 1)
            } else {
                key.to_owned()
            };
            let body = if pages.len() > 1 {
                format!(
                    "Continuation {}/{}\n{}",
                    index + 1,
                    pages.len(),
                    page.iter().collect::<String>()
                )
            } else {
                page.iter().collect::<String>()
            };
            self.store
                .enqueue_imessage_owner_output(
                    revision,
                    &key,
                    &body,
                    None,
                    chrono::Utc::now().timestamp_millis(),
                )
                .map_err(|e| ApprovalError::Discord(format!("owner iMessage queue: {e}")))?;
        }
        Ok(())
    }
    pub fn post_state_card(&self, action: &str) -> Result<(), ApprovalError> {
        let row = self
            .store
            .get_action_with_email(action)
            .map_err(|e| ApprovalError::Discord(e.to_string()))?;
        let Some(row) = row else { return Ok(()) };
        let state = row.action.status.as_str();
        if state == "pending" {
            return self.post_card(
                action,
                &row.email,
                row.action.draft_body.as_deref().unwrap_or(""),
            );
        }
        let commands = match state {
            "scheduled" => "send-now / cancel-schedule / back-to-queue / reschedule: time",
            "superseded" => "recompose",
            "sent" if row.email.platform == "calendar" => "decline (calendar auto-RSVP only)",
            _ => return Ok(()),
        };
        let Some(p) = self
            .store
            .imessage_owner_presentation_in_state(action, state)
            .map_err(|e| ApprovalError::Discord(e.to_string()))?
        else {
            return Ok(());
        };
        let body=format!("{} · {}\nTo: {}\n{}\n\n{}\nScheduled at: {:?}\nRef {}\nReply {} or use a command with this Ref. A 👍 does not operate this notice.",state,p.email.platform,p.to,p.email.subject,p.draft,p.scheduled_at_ms,p.reference,commands);
        self.store
            .enqueue_imessage_owner_output(
                p.revision,
                &format!("state-card:{action}:{}", p.reference),
                &body,
                Some((action, &p.draft, &p.digest)),
                chrono::Utc::now().timestamp_millis(),
            )
            .map(|_| ())
            .map_err(|e| ApprovalError::Discord(e.to_string()))
    }
    pub fn post_card(
        &self,
        action: &str,
        _email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        if !self.active() {
            return Ok(());
        }
        let Some(presentation) = self
            .store
            .imessage_owner_presentation(action)
            .map_err(|e| ApprovalError::Discord(e.to_string()))?
        else {
            return Ok(());
        };
        if crate::interaction::draft_revision(&presentation.draft)
            != crate::interaction::draft_revision(draft)
        {
            return Err(ApprovalError::Discord(
                "draft changed before presentation".into(),
            ));
        }
        let reference = &presentation.reference;
        let email = &presentation.email;
        let summary = visible_discord_message(
            &serde_json::to_value(crate::approval_message(action, email, draft, 0))
                .map_err(|e| ApprovalError::Discord(e.to_string()))?,
        );
        let (clean, needs) = crate::split_needs_input(draft);
        let (clean, assumptions) = crate::split_assumes(&clean);
        let mut text = format!(
            "{} · {}\nTo: {}\n{}\n\n{}",
            if email.kind == "compose" {
                "New message"
            } else {
                "Draft reply"
            },
            email.platform,
            if email.platform == "gmail" {
                presentation.to.as_str()
            } else {
                email.from.as_str()
            },
            email.subject,
            clean
        );
        text.push_str(&format!("\n\nCard details:\n{summary}"));
        if !presentation.cc.is_empty() {
            text.push_str(&format!("\nCc: {}", presentation.cc));
        }
        if !presentation.bcc.is_empty() {
            text.push_str(&format!("\nBcc: {}", presentation.bcc));
        }
        if email.platform != "gmail" {
            text.push_str(&format!(
                "\nConversation: {}",
                email.thread_id.as_deref().unwrap_or("unknown")
            ));
        }
        for need in needs {
            text.push_str(&format!("\nNeeds input: {}", need.text));
        }
        for assumption in assumptions {
            text.push_str(&format!("\nAssumes: {assumption}"));
        }
        text.push_str(&format!("\n\nRef {reference}\nReply approve / 👍, revise: changes, or skip. You can also react 👍 to approve this version."));
        if text.chars().count() > 256_000 {
            return self.output(presentation.revision,&format!("oversize-card:{action}:{reference}"),
                "This draft exceeds the iMessage approval limit. Review the complete card on Discord. It cannot be approved from this text notice.");
        }
        self.store
            .enqueue_imessage_owner_output(
                presentation.revision,
                &format!("card:{action}:{reference}"),
                &text,
                Some((action, &presentation.draft, &presentation.digest)),
                chrono::Utc::now().timestamp_millis(),
            )
            .map(|_| ())
            .map_err(|e| ApprovalError::Discord(e.to_string()))
    }

    /// Caller serializes turns. The durable event claim, not an LLM, decides
    /// whether a command can execute. Handler uncertainty is never auto-retried.
    pub fn prepare_ready(&self) -> anyhow::Result<()> {
        self.store
            .reconcile_imessage_owner_echoes(chrono::Utc::now().timestamp_millis())?;
        for event in self.store.ready_imessage_owner_events()? {
            let control = event.kind != "text"
                || !matches!(
                    owner_control::parse(&event.body, event.target_guid.as_deref()),
                    Control::Chat
                )
                || owner_control::immediate_command(&event.body);
            self.store.classify_imessage_owner_event(&event, control)?;
        }
        Ok(())
    }
    pub async fn dispatch_one(
        &self,
        handler: &dyn ApprovalActionHandler,
        query: &dyn QueryHandler,
        context: AuditCtx,
    ) -> anyhow::Result<bool> {
        if self
            .dispatch_control_one(handler, query, context.clone())
            .await?
        {
            return Ok(true);
        }
        self.dispatch_chat_one(handler, query, context).await
    }
    pub async fn dispatch_control_one(
        &self,
        handler: &dyn ApprovalActionHandler,
        query: &dyn QueryHandler,
        context: AuditCtx,
    ) -> anyhow::Result<bool> {
        if !self.active() {
            return Ok(false);
        }
        self.prepare_ready()?;
        let Some(event) = self
            .store
            .claim_imessage_owner_control(chrono::Utc::now().timestamp_millis())?
        else {
            return Ok(false);
        };
        self.execute_event(event, handler, query, context).await
    }
    pub async fn dispatch_chat_one(
        &self,
        handler: &dyn ApprovalActionHandler,
        query: &dyn QueryHandler,
        context: AuditCtx,
    ) -> anyhow::Result<bool> {
        if !self.active() {
            return Ok(false);
        }
        self.prepare_ready()?;
        let Some(event) = self
            .store
            .claim_imessage_owner_event(chrono::Utc::now().timestamp_millis())?
        else {
            return Ok(false);
        };
        self.execute_event(event, handler, query, context).await
    }
    async fn execute_event(
        &self,
        event: OwnerEvent,
        handler: &dyn ApprovalActionHandler,
        query: &dyn QueryHandler,
        mut context: AuditCtx,
    ) -> anyhow::Result<bool> {
        context.session_id = format!("imessage-owner:{}:{}", event.binding_revision, event.guid);
        context.owner_authorized = true;
        if let Some(target) = event.target_guid.as_deref() {
            if let Some((source, _)) = self
                .store
                .imessage_owner_reply_source(event.binding_revision, target)?
            {
                let parts = source.splitn(3, ':').collect::<Vec<_>>();
                let channel =
                    if matches!(parts.first(), Some(&"discord-query") | Some(&"discord-dm")) {
                        parts.get(1).and_then(|s| s.parse::<u64>().ok())
                    } else if parts.first() == Some(&"result") {
                        parts
                            .get(2)
                            .and_then(|guid| {
                                self.store
                                    .imessage_owner_event_conversation(
                                        event.binding_revision,
                                        guid.split(":page:").next().unwrap_or(guid),
                                    )
                                    .ok()
                                    .flatten()
                            })
                            .and_then(|s| s.parse().ok())
                    } else {
                        None
                    };
                if let Some(channel) = channel {
                    context.channel_id = Some(serenity::all::ChannelId::new(channel));
                    if parts.first() == Some(&"discord-dm") {
                        context.guild_id = None;
                    }
                }
            }
        }
        let result = self.dispatch(&event, handler, query, &context).await;
        let ordinary_chat = event.kind == "text"
            && matches!(
                owner_control::parse(&event.body, event.target_guid.as_deref()),
                Control::Chat
            )
            && !owner_control::immediate_command(&event.body);
        let confirmed_cancel = result.as_ref().err().is_some_and(|error| {
            matches!(
                error.to_string().as_str(),
                "Discord turn was cancelled" | "Phone text turn cancelled"
            )
        });
        let uncertain = ordinary_chat && result.is_err() && !confirmed_cancel;
        let response = match result {
            Ok(text) => text,
            Err(error)
                if matches!(
                    error.to_string().as_str(),
                    "Discord turn was cancelled"
                        | "Phone text turn cancelled"
                ) =>
            {
                "Cancelled. Actions already taken remain recorded.".into()
            }
            Err(error) if error.to_string()=="Owner text stop requested; cleanup unconfirmed" =>
                "Stop requested. Provider cleanup is unconfirmed, so this conversation is paused. Check status and inspect the prior request before reset; it will not be replayed.".into(),
            Err(error) => {
                tracing::warn!(event=%event.guid,"owner iMessage operation failed: {error:#}");
                "That request failed. Check status before retrying an action that may already have run.".into()
            }
        };
        if !response.is_empty() {
            self.output(
                event.binding_revision,
                &format!("result:{}:{}", event.binding_revision, event.guid),
                &response,
            )?;
        }
        if uncertain {
            self.store
                .park_imessage_owner_event(&event, chrono::Utc::now().timestamp_millis())?;
        } else {
            self.store
                .finish_imessage_owner_event(&event, chrono::Utc::now().timestamp_millis())?;
        }
        Ok(true)
    }

    async fn dispatch(
        &self,
        event: &OwnerEvent,
        handler: &dyn ApprovalActionHandler,
        query: &dyn QueryHandler,
        context: &AuditCtx,
    ) -> anyhow::Result<String> {
        let control = match event.kind.as_str() {
            "text" => owner_control::parse(&event.body, event.target_guid.as_deref()),
            "like_add" => {
                if event.target_guid.is_none() {
                    return Ok(
                        "I couldn't identify that draft. Reply to its message or use its Ref."
                            .into(),
                    );
                }
                Control::Decision {
                    verb: Verb::Approve,
                    reference: None,
                    reply_guid: event.target_guid.clone(),
                    feedback: String::new(),
                }
            }
            _ => return Ok(String::new()),
        };
        match control {
            Control::Chat => {
                if event.body.trim().starts_with("alert ") {
                    let words = event.body.split_whitespace().collect::<Vec<_>>();
                    let [_, verb, id] = words.as_slice() else {
                        return Ok("Use alert ack|snooze|done <alert-id>.".into());
                    };
                    let verb = match *verb {
                        "ack" => "ack",
                        "snooze" => "snooze",
                        "done" => "resolve",
                        _ => return Ok("Use alert ack|snooze|done <alert-id>.".into()),
                    };
                    let decision = crate::interaction::DecisionContext {
                        surface: "imessage".into(),
                        actor: event.sender.clone(),
                        conversation: event.chat_guid.clone(),
                        interaction_id: format!(
                            "{}:{}:{}",
                            event.binding_revision, event.guid, event.kind
                        ),
                        revision: None,
                    };
                    return crate::interaction::local_decision(
                        &self.store,
                        &decision,
                        id,
                        verb,
                        "owner_alert",
                        "Owner alert control",
                        || {
                            crate::owner_alerts::control(
                                &self.store,
                                &format!("oa:{verb}:{id}"),
                                chrono::Utc::now().timestamp_millis(),
                            )
                        },
                    );
                }
                if event.body.trim().eq_ignore_ascii_case("presets") {
                    return Ok(crate::PRESETS
                        .iter()
                        .map(|p| format!("{}: {}", p.id, p.label))
                        .collect::<Vec<_>>()
                        .join("\n"));
                }
                if event.body.trim() == "pending" || event.body.trim().starts_with("pending ") {
                    let words = event.body.split_whitespace().collect::<Vec<_>>();
                    let offset = match words.as_slice() {
                        [_] => 0,
                        [_, n] => match n.parse::<usize>() {
                            Ok(n) => n,
                            Err(_) => return Ok("Use pending [offset].".into()),
                        },
                        _ => return Ok("Use pending [offset].".into()),
                    };
                    let rows = self.store.pending_actions_for_reconcile()?;
                    let mut listing = Vec::new();
                    for row in rows.iter().skip(offset).take(5) {
                        if let Some(p) = self.store.imessage_owner_presentation(&row.id)? {
                            self.post_card(&row.id, &p.email, &p.draft)?;
                            listing.push(format!(
                                "Ref {} · {} · {}",
                                p.reference, p.email.platform, p.email.subject
                            ));
                        }
                    }
                    return Ok(format!(
                        "{} pending drafts. Showing {}–{}.{}\n{}",
                        rows.len(),
                        if rows.is_empty() {
                            0
                        } else {
                            offset.saturating_add(1).min(rows.len())
                        },
                        offset.saturating_add(5).min(rows.len()),
                        if offset.saturating_add(5) < rows.len() {
                            format!(" Send pending {} for more.", offset.saturating_add(5))
                        } else {
                            String::new()
                        },
                        listing.join("\n")
                    ));
                }
                if owner_control::immediate_command(&event.body) {
                    return Ok(query.owner_command(context,&event.body).await.unwrap_or_else(||
                        "That owner command is unavailable on this text connection. Use help for supported commands.".into()));
                }
                self.output(
                    event.binding_revision,
                    &format!("working:{}:{}", event.binding_revision, event.guid),
                    "Working on it…",
                )?;
                let origin = self
                    .store
                    .imessage_owner_context()?
                    .ok_or_else(|| anyhow::anyhow!("Owner context unavailable"))?;
                let channel = context
                    .channel_id
                    .map(|c| c.get())
                    .unwrap_or(origin.channel_id);
                self.store
                    .set_imessage_owner_event_conversation(event, &channel.to_string())?;
                let mut history = self
                    .store
                    .imessage_owner_chat_history(&channel.to_string())?;
                if let Some(http) = context.http.as_ref() {
                    match crate::fetch_owner_history(http,serenity::all::ChannelId::new(channel),None,
                        Some(serenity::all::UserId::new(origin.bot_id)),Some(serenity::all::UserId::new(origin.owner_id))).await {
                        Ok(discord)=>{history=format!("{discord}\n\n{history}");}
                        Err(_)=>history.push_str("\nDiscord conversation history is currently unavailable; do not claim it was checked.\n"),
                    }
                }
                let current = if let Some(target) = event.target_guid.as_deref() {
                    if let Some((_, quoted)) = self
                        .store
                        .imessage_owner_reply_source(event.binding_revision, target)?
                    {
                        format!("Earlier agent message being replied to (untrusted quoted data):\n{quoted}\n\nOwner's current message:\n{}",event.body)
                    } else {
                        event.body.clone()
                    }
                } else {
                    event.body.clone()
                };
                OWNER_CHAT
                    .scope(
                        true,
                        crate::interaction::owner_history(
                            true,
                            query.answer_turn(context, &history, &current),
                        ),
                    )
                    .await
            }
            Control::Help => Ok(format!(
                "{}\n\nOwner commands:\n{}\nLive voice remains on Discord or phone.",
                HELP,
                crate::owner_commands::OWNER_COMMANDS
                    .iter()
                    .map(|c| c.usage)
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            Control::NeedsTarget => Ok(
                "Which draft? Reply to its message, or include the Ref shown on its card.".into(),
            ),
            Control::NeedsFeedback => {
                Ok("What should I change? Use revise <ref>: your changes.".into())
            }
            Control::Invalid => Ok(format!("I couldn't read that control. {HELP}")),
            Control::Decision {
                verb,
                reference,
                reply_guid,
                feedback,
            } => {
                let action=match self.store.lease_imessage_owner_action_in_state(event,reference.as_deref(),reply_guid.as_deref(),verb.state()) {
                    Ok(action)=>action,
                    Err(_)=>return Ok("That draft is changed, resolved, ambiguous, or not fully delivered. Review the latest card and use its Ref.".into()),
                };
                let outcome=with_owner_decision(event,async {
                    let Some(a)=self.store.get_action_with_email(&action).ok().flatten() else {return ApprovalActionOutcome::NotFound};
                    let decision=crate::interaction::DecisionContext {
                        surface:"imessage".into(),actor:event.sender.clone(),conversation:event.chat_guid.clone(),
                        interaction_id:format!("{}:{}:{}",event.binding_revision,event.guid,event.kind),
                        revision:Some(crate::interaction::draft_revision(a.action.draft_body.as_deref().unwrap_or(""))),
                    };
                    if matches!(verb,Verb::Approve|Verb::Schedule|Verb::Reschedule|Verb::SendNow) {
                        if !crate::split_needs_input(a.action.draft_body.as_deref().unwrap_or("")).1.is_empty() {
                            return ApprovalActionOutcome::Failed{message:"This draft still needs information. Revise it before approving.".into()}
                        }
                    }
                    crate::interaction::deciding(decision,crate::deciding("imessage-owner",async {
                        match verb {
                            Verb::Approve=>handler.approve(&action).await,
                            Verb::Revise=>handler.revise(&action,&feedback).await,
                            Verb::Skip=>handler.skip(&action).await,
                            Verb::Fill=>handler.revise(&action,&format!("Use these owner-supplied missing values and resolve the corresponding needs-input markers: {feedback}")).await,
                            Verb::Refine=>match crate::PRESETS.iter().find(|p|p.id==feedback.trim()) {
                                Some(p)=>handler.revise(&action,p.feedback).await,
                                None=>ApprovalActionOutcome::Failed{message:"Unknown preset. Send presets to list them.".into()},
                            },
                            Verb::Schedule|Verb::Reschedule=>match crate::timeparse::parse_send_at(&feedback,chrono::Local::now()) {
                                Ok(at) if verb==Verb::Schedule=>handler.schedule(&action,at).await,
                                Ok(at)=>handler.reschedule(&action,at).await,
                                Err(message)=>ApprovalActionOutcome::Failed{message},
                            },
                            Verb::SendNow=>handler.send_now(&action).await,
                            Verb::CancelSchedule=>handler.cancel_schedule(&action).await,
                            Verb::BackToQueue=>handler.back_to_queue(&action).await,
                            Verb::Recompose=>handler.recompose(&action).await,
                            Verb::Decline=>handler.calendar_decline(&action).await,
                        }
                    })).await
                }).await;
                // Queue the revised card and response before receipt completion
                // releases the presentation reservation.
                match outcome {
                    ApprovalActionOutcome::Approved => {
                        Ok("Approved. Check the delivery status for confirmation.".into())
                    }
                    ApprovalActionOutcome::Skipped => Ok("Skipped.".into()),
                    ApprovalActionOutcome::Revised { email, draft } => {
                        self.post_card(&action, &email, &draft)?;
                        Ok("Revised draft ready. Review it and approve again when ready.".into())
                    }
                    ApprovalActionOutcome::Failed { message } => Ok(message),
                    ApprovalActionOutcome::AlreadyResolved { status, .. } => {
                        Ok(format!("Already {status}."))
                    }
                    ApprovalActionOutcome::NotFound => {
                        Ok("That draft is no longer available.".into())
                    }
                    other => {
                        with_owner_decision(event, async { self.post_state_card(&action) }).await?;
                        Ok(crate::outcome::describe(&other))
                    }
                }
            }
        }
    }
}

#[async_trait]
impl ApprovalBroker for OwnerSurface {
    fn is_available(&self) -> bool {
        self.active()
    }
    fn can_post_notices(&self) -> bool {
        false
    }
    async fn post_approval(
        &self,
        id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        if !self.active() {
            return Err(ApprovalError::NotReady);
        }
        self.post_card(id, email, draft)
    }
    async fn post_scheduled_notice(
        &self,
        id: &str,
        _email: &Email,
        _local: &str,
        _ms: i64,
        _to: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        if !self.active() {
            return Err(ApprovalError::NotReady);
        }
        self.post_state_card(id)?;
        Ok(None)
    }
    // Generic notices are captured with their real Discord message IDs by the
    // mirror ingress. Do not invent lossy idempotency keys from prose here.
    async fn post_flag_notice(&self, _email: &Email, _reason: &str) -> Result<(), ApprovalError> {
        Err(ApprovalError::NotReady)
    }
}
#[async_trait]
impl ApprovalCardSurface for OwnerSurface {
    fn surface_name(&self) -> &'static str {
        "imessage-owner"
    }
    async fn redraw_cards(&self, id: &str, _origin: &str) {
        let Ok(Some(a)) = self.store.get_action_with_email(id) else {
            return;
        };
        if a.action.status == "pending" {
            if let Some(draft) = a.action.draft_body {
                let _ = self.post_card(id, &a.email, &draft);
            }
        } else {
            let _ = self.post_state_card(id);
            let _ = self.output(
                self.store
                    .imessage_owner_binding()
                    .ok()
                    .flatten()
                    .map(|b| b.revision)
                    .unwrap_or(0),
                &format!("state:{id}:{}", a.action.status),
                &format!(
                    "{} · {}: {}.",
                    a.email.subject, a.email.platform, a.action.status
                ),
            );
        }
    }
}
