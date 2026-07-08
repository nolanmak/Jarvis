//! WhatsApp agent control / approval surface (#102) — the WhatsApp analogue
//! of the Discord bot.
//!
//! Implements the **same** [`ApprovalBroker`] contract the Discord broker
//! implements, so a queued draft can be approved / revised / declined from a
//! WhatsApp control thread, plus a query/command path mirroring the Discord
//! query mode (wiki / email / web questions).
//!
//! ## Differences from the Discord broker
//!
//! WhatsApp has no buttons or modals — everything is plain text. So:
//!
//! - `post_approval` sends a formatted text card to the **control chat** and
//!   remembers `action_id` as that chat's *active* card. The card explains
//!   the text verbs.
//! - Inbound messages from the control chat are parsed by
//!   [`WhatsappControlSurface::handle_control_message`]:
//!   - `approve` / `ok` / `send`              → `ApprovalActionHandler::approve`
//!   - `revise <feedback>` / `redo <feedback>`→ `ApprovalActionHandler::revise`
//!   - `decline` / `skip` / `no`              → `ApprovalActionHandler::skip`
//!   - anything else                          → `QueryHandler::answer`
//!     (wiki / email / web question, same quality as Discord query mode)
//!
//! ## Safety gates (#40 / #74 / #102)
//!
//! Reuses the #12 sidecar transport (an additional consumer of the shared
//! [`WaClient`], NOT a second client). Every outbound send is gated on:
//!
//! 1. `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED` truthy (global ban-risk
//!    kill-switch — see [`crate::channel::control_enabled`]), AND
//! 2. the control chat being in `whatsapp_outbound_allowlist`, AND
//! 3. the message originating from the *designated control chat* (the user's
//!    own self-chat / a single configured JID) — never an arbitrary contact.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::{info, warn};

use augmentagent_approval_discord::{
    ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalError, AuditCtx,
    QueryHandler,
};
use augmentagent_store::{Email, Store};

use crate::api::WaClient;
use crate::channel::control_enabled;
use crate::types::WaMessage;

/// Parsed control verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    Approve,
    Decline,
    Revise(String),
    /// Free-text query routed to the wiki/email/web `QueryHandler`.
    Query(String),
}

/// Parse a control-chat message body into a [`ControlCommand`]. Case- and
/// whitespace-insensitive on the verb; the rest of the line is the argument.
pub fn parse_control_command(body: &str) -> ControlCommand {
    let trimmed = body.trim();
    let lower = trimmed.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    match first {
        "approve" | "ok" | "okay" | "send" | "yes" | "y" | "👍" => ControlCommand::Approve,
        "decline" | "skip" | "no" | "n" | "reject" | "👎" => ControlCommand::Decline,
        "revise" | "redo" | "edit" | "change" => {
            // Everything after the verb is the feedback.
            let rest = trimmed
                .split_once(char::is_whitespace)
                .map(|(_, r)| r.trim().to_string())
                .unwrap_or_default();
            ControlCommand::Revise(rest)
        }
        _ => ControlCommand::Query(trimmed.to_string()),
    }
}

#[derive(Clone)]
pub struct WhatsappControlConfig {
    /// The single designated control chat JID (bare). Inbound from any other
    /// chat is NOT treated as a control command. Typically the user's own
    /// self-chat or a dedicated thread with the agent.
    pub control_chat_jid: String,
}

/// WhatsApp control/approval broker. Cheap to `clone()` — all state is `Arc`.
#[derive(Clone)]
pub struct WhatsappControlSurface {
    client: WaClient,
    store: Arc<Store>,
    config: WhatsappControlConfig,
    action_handler: Option<Arc<dyn ApprovalActionHandler>>,
    query_handler: Option<Arc<dyn QueryHandler>>,
    /// Most-recently-posted card per control chat. WhatsApp has no per-message
    /// buttons, so a bare `approve` applies to the chat's active card.
    active_card: Arc<Mutex<HashMap<String, String>>>,
}

impl WhatsappControlSurface {
    pub fn new(
        client: WaClient,
        store: Arc<Store>,
        config: WhatsappControlConfig,
        action_handler: Option<Arc<dyn ApprovalActionHandler>>,
        query_handler: Option<Arc<dyn QueryHandler>>,
    ) -> Self {
        Self {
            client,
            store,
            config,
            action_handler,
            query_handler,
            active_card: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// True iff `chat_jid` is the designated control chat.
    pub fn is_control_chat(&self, chat_jid: &str) -> bool {
        chat_jid == self.config.control_chat_jid
    }

    /// All three outbound gates in one place. Returns `Err(ApprovalError)`
    /// with a human reason when a send must be refused.
    fn check_send_gate(&self, chat_jid: &str) -> Result<(), ApprovalError> {
        if !control_enabled() {
            return Err(ApprovalError::Discord(
                "AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED is not set — \
                 WhatsApp control surface is disabled (ban-risk gate)"
                    .into(),
            ));
        }
        if !self.is_control_chat(chat_jid) {
            return Err(ApprovalError::Discord(format!(
                "{chat_jid} is not the designated control chat; refusing to send"
            )));
        }
        match self.store.is_whatsapp_outbound_allowed(chat_jid) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ApprovalError::Discord(format!(
                "{chat_jid} not in whatsapp_outbound_allowlist; \
                 run `augmentagent whatsapp allow-outbound {chat_jid}`"
            ))),
            Err(e) => Err(ApprovalError::Discord(format!(
                "outbound allowlist check failed: {e}"
            ))),
        }
    }

    /// Send text to the control chat, enforcing all gates.
    async fn send_to_control(&self, text: &str) -> Result<(), ApprovalError> {
        let jid = self.config.control_chat_jid.clone();
        self.check_send_gate(&jid)?;
        self.client
            .send_text(&jid, text)
            .await
            .map_err(|e| ApprovalError::Discord(format!("wa send_text: {e}")))?;
        Ok(())
    }

    /// Handle one inbound message from the control chat. No-op (returns
    /// `Ok(false)`) if the message isn't from the designated control chat.
    /// Returns `Ok(true)` when the message was consumed as a control command.
    pub async fn handle_control_message(
        &self,
        msg: &WaMessage,
    ) -> Result<bool, ApprovalError> {
        let chat_jid = msg.chat.bare();
        if !self.is_control_chat(&chat_jid) || msg.is_outbound() {
            return Ok(false);
        }
        let cmd = parse_control_command(&msg.text);
        match cmd {
            ControlCommand::Approve | ControlCommand::Decline | ControlCommand::Revise(_) => {
                let Some(handler) = self.action_handler.clone() else {
                    self.send_to_control("No approval handler wired (dry-run?).")
                        .await
                        .ok();
                    return Ok(true);
                };
                let action_id = {
                    let guard = self.active_card.lock().await;
                    guard.get(&chat_jid).cloned()
                };
                let Some(action_id) = action_id else {
                    self.send_to_control(
                        "No active draft to act on. Send a question instead and I'll answer it.",
                    )
                    .await
                    .ok();
                    return Ok(true);
                };
                let outcome = match &cmd {
                    ControlCommand::Approve => handler.approve(&action_id).await,
                    ControlCommand::Decline => handler.skip(&action_id).await,
                    ControlCommand::Revise(fb) => handler.revise(&action_id, fb).await,
                    ControlCommand::Query(_) => unreachable!(),
                };
                self.ack_outcome(&chat_jid, &action_id, outcome).await;
                Ok(true)
            }
            ControlCommand::Query(q) => {
                let Some(qh) = self.query_handler.clone() else {
                    self.send_to_control("Query mode is not enabled.").await.ok();
                    return Ok(true);
                };
                // WhatsApp has no Discord http/channel for side-channel audit
                // notifications; the chat JID as session id still keys the
                // reasoner's NDJSON audit log (#201).
                let audit_ctx = AuditCtx {
                    session_id: format!("wa:{chat_jid}"),
                    http: None,
                    channel_id: None,
                };
                match qh.answer(&audit_ctx, &q).await {
                    Ok(answer) => {
                        for chunk in chunk_for_whatsapp(&answer) {
                            if let Err(e) = self.send_to_control(&chunk).await {
                                warn!("control: query answer send failed: {e}");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        self.send_to_control(&format!("Query failed: {e}")).await.ok();
                    }
                }
                Ok(true)
            }
        }
    }

    /// Send a human ack for an action outcome and clear/refresh the active
    /// card pointer. `Revised` re-posts a fresh card (same `action_id`).
    async fn ack_outcome(
        &self,
        chat_jid: &str,
        action_id: &str,
        outcome: ApprovalActionOutcome,
    ) {
        match outcome {
            ApprovalActionOutcome::Approved => {
                self.active_card.lock().await.remove(chat_jid);
                self.send_to_control("Sent.").await.ok();
            }
            ApprovalActionOutcome::Skipped => {
                self.active_card.lock().await.remove(chat_jid);
                self.send_to_control("Declined — draft discarded.").await.ok();
            }
            ApprovalActionOutcome::Revised { email, draft } => {
                // Re-post the refreshed card; pointer stays on action_id.
                let _ = self.post_card(action_id, &email, &draft).await;
            }
            ApprovalActionOutcome::AlreadyResolved { status } => {
                self.active_card.lock().await.remove(chat_jid);
                self.send_to_control(&format!("Already {status} — nothing to do."))
                    .await
                    .ok();
            }
            ApprovalActionOutcome::NotFound => {
                self.active_card.lock().await.remove(chat_jid);
                self.send_to_control("That draft no longer exists.").await.ok();
            }
            ApprovalActionOutcome::Failed { message } => {
                self.send_to_control(&format!("Action failed: {message}"))
                    .await
                    .ok();
            }
        }
    }

    /// Format + send an approval card to the control chat and record it as
    /// the chat's active card.
    async fn post_card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        let card = render_card(email, draft);
        self.send_to_control(&card).await?;
        self.active_card
            .lock()
            .await
            .insert(self.config.control_chat_jid.clone(), action_id.to_string());
        info!(action_id, "whatsapp control card posted");
        Ok(())
    }
}

#[async_trait]
impl ApprovalBroker for WhatsappControlSurface {
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        self.post_card(action_id, email, draft).await
    }

    async fn post_flag_notice(
        &self,
        email: &Email,
        reason: &str,
    ) -> Result<(), ApprovalError> {
        let from = truncate(&email.from, 120);
        let body = truncate(&email.body, 400);
        let text = format!(
            "*Heads up* — from {from}\n_reason: {reason}_\n\n{body}"
        );
        self.send_to_control(&text).await
    }
}

/// WhatsApp single-message text cap is ~65k but readability + the sidecar's
/// frame budget favor smaller chunks. Mirrors `chunk_for_discord`'s intent.
const WA_CHUNK: usize = 3500;

pub fn chunk_for_whatsapp(s: &str) -> Vec<String> {
    if s.len() <= WA_CHUNK {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in s.split_inclusive('\n') {
        if buf.len() + line.len() > WA_CHUNK && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
        }
        if line.len() > WA_CHUNK {
            // A single very long line — hard-split on char boundaries.
            for ch in line.chars() {
                if buf.len() + ch.len_utf8() > WA_CHUNK {
                    out.push(std::mem::take(&mut buf));
                }
                buf.push(ch);
            }
        } else {
            buf.push_str(line);
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn render_card(email: &Email, draft: &str) -> String {
    let from = truncate(&email.from, 120);
    let original = truncate(&email.body, 800);
    let draft = truncate(draft, 2000);
    format!(
        "*Draft reply* — to {from}\n\n\
         *Their message:*\n{original}\n\n\
         *Proposed reply:*\n{draft}\n\n\
         Reply *approve* to send, *revise <what to change>*, or *decline*."
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(3);
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_approve_synonyms() {
        for v in ["approve", "OK", " send ", "yes", "Y"] {
            assert_eq!(parse_control_command(v), ControlCommand::Approve);
        }
    }

    #[test]
    fn parses_decline_synonyms() {
        for v in ["decline", "skip", "no", "REJECT"] {
            assert_eq!(parse_control_command(v), ControlCommand::Decline);
        }
    }

    #[test]
    fn parses_revise_with_feedback() {
        assert_eq!(
            parse_control_command("revise make it shorter and warmer"),
            ControlCommand::Revise("make it shorter and warmer".into())
        );
        assert_eq!(
            parse_control_command("redo  less formal"),
            ControlCommand::Revise("less formal".into())
        );
    }

    #[test]
    fn revise_without_feedback_is_empty_string() {
        assert_eq!(
            parse_control_command("revise"),
            ControlCommand::Revise(String::new())
        );
    }

    #[test]
    fn free_text_is_a_query() {
        assert_eq!(
            parse_control_command("who is Tony Siu?"),
            ControlCommand::Query("who is Tony Siu?".into())
        );
    }

    #[test]
    fn chunk_for_whatsapp_keeps_short_intact() {
        assert_eq!(chunk_for_whatsapp("hello"), vec!["hello".to_string()]);
    }

    #[test]
    fn chunk_for_whatsapp_splits_long() {
        let big = "line\n".repeat(2000); // ~10k
        let chunks = chunk_for_whatsapp(&big);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= WA_CHUNK + 8));
        assert_eq!(chunks.concat(), big);
    }

    #[test]
    fn render_card_contains_verbs_and_draft() {
        let email = Email {
            message_id: "wa:x:1".into(),
            thread_id: Some("x".into()),
            from: "Tony <whatsapp:1@s.whatsapp.net>".into(),
            subject: String::new(),
            body: "free thursday?".into(),
            date: "d".into(),
            account_entity_id: Some("whatsapp:device:1".into()),
            platform: "whatsapp".into(),
            kind: "dm".into(),
        };
        let card = render_card(&email, "Thursday 3pm works.");
        assert!(card.contains("free thursday?"));
        assert!(card.contains("Thursday 3pm works."));
        assert!(card.contains("approve"));
        assert!(card.contains("revise"));
        assert!(card.contains("decline"));
    }
}
