//! Serenity event handler: routes button/modal interactions into the approval
//! broker, and routes qualifying messages into the wiki query handler.

use std::sync::Arc;

use serenity::all::{
    Context, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    EventHandler, Interaction, Message, MessageReference, Ready,
};
use tracing::{debug, info, warn};

use crate::broker::{BrokerState, DeliveryOutcome};
use crate::custom_id::{CustomId, Verb};
use crate::layout::{extract_feedback, revise_modal};
use crate::ApprovalOutcome;

const DISCORD_MSG_LIMIT: usize = 1900; // conservative under the hard 2000

pub struct Handler {
    pub state: Arc<BrokerState>,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("discord broker ready as {}", ready.user.name);
        self.state.mark_ready();
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let Some(handler) = &self.state.query_handler else {
            return; // query feature disabled
        };

        // Accept from: the designated query channel, OR any DM.
        let is_dm = msg.guild_id.is_none();
        let in_query_channel = self
            .state
            .query_channel_id
            .is_some_and(|cid| cid == msg.channel_id);
        if !is_dm && !in_query_channel {
            return;
        }

        // User allowlist (single-user bot by default).
        if let Some(allowed) = self.state.allowed_user_id {
            if msg.author.id != allowed {
                debug!(
                    "ignoring message from non-allowed user {}",
                    msg.author.id.get()
                );
                return;
            }
        }

        let question = msg.content.trim().to_string();
        if question.is_empty() {
            return;
        }

        let handler = Arc::clone(handler);
        let http = ctx.http.clone();
        let channel_id = msg.channel_id;
        let msg_id = msg.id;

        tokio::spawn(async move {
            match handler.answer(&question).await {
                Ok(answer) => {
                    for chunk in chunk_for_discord(&answer) {
                        let builder = CreateMessage::new()
                            .content(chunk)
                            .reference_message(MessageReference::from((channel_id, msg_id)));
                        if let Err(e) = channel_id.send_message(&*http, builder).await {
                            warn!("failed to post wiki answer chunk: {e}");
                            break;
                        }
                    }
                }
                Err(e) => {
                    let err_msg = format!("wiki query failed: {e}");
                    let builder = CreateMessage::new()
                        .content(err_msg)
                        .reference_message(MessageReference::from((channel_id, msg_id)));
                    if let Err(post_err) = channel_id.send_message(&*http, builder).await {
                        warn!("failed to post wiki error: {post_err}");
                    }
                }
            }
        });
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Component(comp) => {
                let Some(cid) = CustomId::parse(&comp.data.custom_id) else {
                    debug!("unrecognized custom_id: {}", comp.data.custom_id);
                    return;
                };
                match cid.verb {
                    Verb::Approve => {
                        let delivered = self.state.deliver(
                            &cid.action_id,
                            ApprovalOutcome::Approved {
                                final_draft: self.state.draft_for(&cid.action_id),
                            },
                        );
                        let msg = match delivered {
                            DeliveryOutcome::Delivered => "Approved — sending.",
                            DeliveryOutcome::Unknown => "This request has expired.",
                        };
                        ack(&ctx, &comp, msg).await;
                    }
                    Verb::Skip => {
                        let delivered =
                            self.state.deliver(&cid.action_id, ApprovalOutcome::Skipped);
                        let msg = match delivered {
                            DeliveryOutcome::Delivered => "Skipped.",
                            DeliveryOutcome::Unknown => "This request has expired.",
                        };
                        ack(&ctx, &comp, msg).await;
                    }
                    Verb::Revise => {
                        let modal = revise_modal(&cid.action_id, None);
                        if let Err(e) = comp
                            .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                            .await
                        {
                            warn!("failed to open revise modal: {e}");
                        }
                    }
                    Verb::ReviseModal => {
                        debug!("unexpected ReviseModal on component interaction");
                    }
                }
            }
            Interaction::Modal(modal) => {
                let Some(cid) = CustomId::parse(&modal.data.custom_id) else {
                    return;
                };
                if cid.verb != Verb::ReviseModal {
                    return;
                }
                let feedback = extract_feedback(&modal.data.components).unwrap_or_default();
                let delivered = self
                    .state
                    .deliver(&cid.action_id, ApprovalOutcome::Revise { feedback });
                let msg = match delivered {
                    DeliveryOutcome::Delivered => "Revising…",
                    DeliveryOutcome::Unknown => "This request has expired.",
                };
                if let Err(e) = modal
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(msg)
                                .ephemeral(true),
                        ),
                    )
                    .await
                {
                    warn!("failed to ack modal: {e}");
                }
            }
            _ => {}
        }
    }
}

async fn ack(ctx: &Context, comp: &serenity::all::ComponentInteraction, message: &str) {
    if let Err(e) = comp
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            ),
        )
        .await
    {
        warn!("failed to ack interaction: {e}");
    }
}

/// Split a wiki answer into Discord-friendly chunks (< 2000 chars each),
/// preferring paragraph then line boundaries.
pub(crate) fn chunk_for_discord(full: &str) -> Vec<String> {
    if full.len() <= DISCORD_MSG_LIMIT {
        return vec![full.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in full.split("\n\n") {
        let candidate_len = current.len() + paragraph.len() + 2;
        if candidate_len <= DISCORD_MSG_LIMIT {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(paragraph);
        } else {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            if paragraph.len() <= DISCORD_MSG_LIMIT {
                current.push_str(paragraph);
            } else {
                // Paragraph itself too long — hard-split on chars.
                for piece in hard_split(paragraph, DISCORD_MSG_LIMIT) {
                    chunks.push(piece);
                }
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn hard_split(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::with_capacity(max);
    for c in s.chars() {
        if buf.len() + c.len_utf8() > max {
            out.push(std::mem::replace(&mut buf, String::with_capacity(max)));
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_answer_is_one_chunk() {
        let chunks = chunk_for_discord("hi there");
        assert_eq!(chunks, vec!["hi there".to_string()]);
    }

    #[test]
    fn splits_on_paragraph_boundary() {
        let para = "a".repeat(1000);
        let full = format!("{para}\n\n{para}\n\n{para}");
        let chunks = chunk_for_discord(&full);
        // 3000 chars, limit ~1900 → expect 2 or 3 chunks, each under the cap
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.len() <= DISCORD_MSG_LIMIT, "chunk too long: {}", c.len());
        }
    }

    #[test]
    fn handles_oversize_single_paragraph() {
        let long = "x".repeat(5000);
        let chunks = chunk_for_discord(&long);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.len() <= DISCORD_MSG_LIMIT);
        }
    }
}
