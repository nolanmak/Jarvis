//! `DiscordDmChannel` — incoming DM → triage → draft → approval card.
//!
//! Mirrors the LinkedIn channel's per-message pipeline (see
//! `crates/augmentagent-channel-linkedin/src/channel.rs:179`), adapted for
//! Discord's subject-less DMs and serenity's `Message` type.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::{ApprovalBroker, DmMessageHandler};
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Email, Store, TriageResult};
use augmentagent_wiki::IdentityIndex;

use crate::{ACCOUNT_ENTITY_ID, PLATFORM};

#[derive(Clone, Debug)]
pub struct DiscordDmChannelConfig {
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    /// Skill dir for the discord rubric. Defaults to `skills/discord-triage`
    /// when constructed via [`DiscordDmChannel::new`] — callers can override
    /// to reuse the email-triage rubric during transition.
    pub skill_dir: PathBuf,
}

impl Default for DiscordDmChannelConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/discord-triage"),
        }
    }
}

pub struct DiscordDmChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    /// Approval broker for posting triage results. Populated after construction
    /// via [`DiscordDmChannel::set_approvals`] because the broker and the DM
    /// handler reference each other (broker posts approval cards for the DM
    /// channel; DM channel is the broker's `DmMessageHandler`). Start the
    /// broker, then wire the broker back into the channel.
    approvals: OnceLock<Arc<dyn ApprovalBroker>>,
    pub config: DiscordDmChannelConfig,
    /// Optional identity index — enables wiki context on triage when the
    /// sender's Discord user id is linked to a `people/*.md` page.
    pub identity_index: Option<Arc<IdentityIndex>>,
    wiki_schema: Option<String>,
}

impl<R: Reasoner + 'static> DiscordDmChannel<R> {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        config: DiscordDmChannelConfig,
        identity_index: Option<Arc<IdentityIndex>>,
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
            reasoner,
            approvals: OnceLock::new(),
            config,
            identity_index,
            wiki_schema,
        }
    }

    /// Complete the circular wiring: after the broker starts, hand it back to
    /// the channel so the triage pipeline can post approval cards.
    pub fn set_approvals(&self, broker: Arc<dyn ApprovalBroker>) {
        let _ = self.approvals.set(broker);
    }

    fn approvals(&self) -> Option<&Arc<dyn ApprovalBroker>> {
        self.approvals.get()
    }

    /// Core per-message pipeline. Called from the serenity event handler via
    /// [`DmMessageHandler::handle`].
    pub async fn handle_incoming_dm(&self, msg: &Message) -> anyhow::Result<()> {
        let email = message_to_email(msg);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            debug!(message_id = %email.message_id, "discord dm already processed; skipping");
            return Ok(());
        }

        let wiki_hint = self.wiki_hint_for_sender(&msg.author.id.get().to_string());

        // --- TRIAGE ---
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
                if let Some(approvals) = self.approvals() {
                    if let Err(e) = approvals.post_flag_notice(&email, reason).await {
                        warn!(message_id = %email.message_id, "post_flag_notice failed: {e}");
                    }
                } else {
                    warn!(message_id = %email.message_id, "approvals not wired; dropping flag notice");
                }
                self.maybe_ingest(
                    &email,
                    DecisionKind::Flag,
                    decision.reason.as_deref(),
                    None,
                    IngestTrigger::Triaged,
                );
                Ok(())
            }
            DecisionKind::Reply => {
                let skill_system =
                    std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                        .unwrap_or_default();
                let draft = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "");
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
                        "[discord reply dry-run] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
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
                    return Ok(());
                }

                let Some(approvals) = self.approvals() else {
                    error!(
                        message_id = %email.message_id,
                        "approvals not wired; cannot post approval card"
                    );
                    return Err(anyhow::anyhow!("approvals broker not wired"));
                };
                let action_id = self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&drafted),
                    ActionStatus::Pending,
                )?;
                if let Err(e) = approvals.post_approval(&action_id, &email, &drafted).await {
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Error,
                        None,
                        Some(&format!("post_approval: {e}")),
                    )?;
                    return Err(anyhow::anyhow!("post_approval: {e}"));
                }
                info!(action_id, message_id = %email.message_id, "discord approval card posted");
                Ok(())
            }
        }
    }

    fn wiki_hint_for_sender(&self, discord_user_id: &str) -> String {
        let Some(index) = &self.identity_index else {
            return String::new();
        };
        let Some(page) = index.lookup(PLATFORM, discord_user_id) else {
            return String::new();
        };
        format!(
            "Sender's wiki page: {} (use Read to open it; weight the decision by their documented tone/importance).",
            page.slug
        )
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

#[async_trait]
impl<R: Reasoner + 'static> DmMessageHandler for DiscordDmChannel<R> {
    async fn handle(&self, msg: &Message, _http: &Arc<Http>) -> anyhow::Result<()> {
        // Outbound sends happen at approval time, not here — the stored action
        // carries the channel_id in its email.thread_id and the CLI's
        // ApprovalActionHandler calls send_discord_dm on Approve.
        self.handle_incoming_dm(msg).await
    }
}

/// Convert a serenity DM into an `Email` row.
///
/// - `message_id` = discord message snowflake, string-serialized
/// - `thread_id` = DM channel id (stable per user pair; the reply target)
/// - `from` carries the username and Discord user ID in LinkedIn-style
///   wrapper so the triage rubric can pick out the id for identity lookup
/// - `subject` empty — Discord DMs have no subject
fn message_to_email(msg: &Message) -> Email {
    let author_name = if msg.author.global_name.as_deref().map(str::is_empty) == Some(false) {
        msg.author.global_name.clone().unwrap()
    } else {
        msg.author.name.clone()
    };
    let from = format!("{} <discord:{}>", author_name, msg.author.id.get());
    Email {
        message_id: msg.id.get().to_string(),
        thread_id: Some(msg.channel_id.get().to_string()),
        from,
        subject: String::new(),
        body: msg.content.clone(),
        date: msg.timestamp.to_rfc3339().unwrap_or_default(),
        account_entity_id: Some(ACCOUNT_ENTITY_ID.to_string()),
        platform: PLATFORM.to_string(),
        kind: "dm".to_string(),
    }
}

/// Convenience: returns the `thread_id` field used for outbound reply sends
/// (the Discord DM channel id). Callers pass this to
/// [`crate::send::send_discord_dm`].
pub fn reply_channel_id(email: &Email) -> Option<u64> {
    email.thread_id.as_deref().and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_channel_id_parses_valid_thread() {
        let email = Email {
            message_id: "m".into(),
            thread_id: Some("123456789012345678".into()),
            from: "alice <discord:42>".into(),
            subject: String::new(),
            body: "hi".into(),
            date: "2026-04-21T00:00:00Z".into(),
            account_entity_id: Some(ACCOUNT_ENTITY_ID.to_string()),
            platform: PLATFORM.into(),
            kind: "dm".into(),
        };
        assert_eq!(reply_channel_id(&email), Some(123456789012345678));
    }

    #[test]
    fn reply_channel_id_rejects_non_numeric_thread() {
        let email = Email {
            message_id: "m".into(),
            thread_id: Some("not-a-number".into()),
            from: "alice <discord:42>".into(),
            subject: String::new(),
            body: "hi".into(),
            date: "2026-04-21T00:00:00Z".into(),
            account_entity_id: Some(ACCOUNT_ENTITY_ID.to_string()),
            platform: PLATFORM.into(),
            kind: "dm".into(),
        };
        assert!(reply_channel_id(&email).is_none());
    }

    #[test]
    fn reply_channel_id_none_when_thread_missing() {
        let email = Email {
            message_id: "m".into(),
            thread_id: None,
            from: "alice <discord:42>".into(),
            subject: String::new(),
            body: "hi".into(),
            date: "2026-04-21T00:00:00Z".into(),
            account_entity_id: Some(ACCOUNT_ENTITY_ID.to_string()),
            platform: PLATFORM.into(),
            kind: "dm".into(),
        };
        assert!(reply_channel_id(&email).is_none());
    }

    #[test]
    fn platform_and_account_constants_are_stable() {
        // Silent drift on these values would break the CLI's platform
        // dispatch + any SQL that filters by platform='discord'. Pin them.
        assert_eq!(PLATFORM, "discord");
        assert_eq!(ACCOUNT_ENTITY_ID, "discord:bot");
    }
}
