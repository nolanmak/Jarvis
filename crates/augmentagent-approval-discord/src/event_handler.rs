//! Serenity event handler.
//!
//! Routes button clicks + modal submits to the injected `ApprovalActionHandler`
//! (which owns sqlite + Gmail + reasoner access). Approvals are resolved via
//! the database, so old cards remain valid indefinitely.
//!
//! Also routes qualifying messages in the query channel (or DMs) to the
//! `QueryHandler` for wiki-ask answers.

use std::sync::Arc;

use serenity::all::{
    Context, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, CreateMessage, EventHandler, Interaction, Message,
    MessageReference, Ready,
};
use tracing::{debug, info, warn};

use crate::broker::BrokerState;
use crate::custom_id::{CustomId, Verb};
use crate::layout::{approval_message, extract_feedback, revise_modal};
use crate::ApprovalActionOutcome;

const DISCORD_MSG_LIMIT: usize = 1900;

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
            return;
        };

        let is_dm = msg.guild_id.is_none();
        let in_query_channel = self
            .state
            .query_channel_id
            .is_some_and(|cid| cid == msg.channel_id);
        if !is_dm && !in_query_channel {
            return;
        }

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
                // Enforce user allowlist on clicks.
                if let Some(allowed) = self.state.allowed_user_id {
                    if comp.user.id != allowed {
                        ack_ephemeral(
                            &ctx,
                            &comp,
                            "You are not authorized to approve replies on this bot.",
                        )
                        .await;
                        return;
                    }
                }
                match cid.verb {
                    Verb::Approve => {
                        // Discord gives us 3 seconds to acknowledge an
                        // interaction. Defer immediately so the user doesn't
                        // see "something went wrong", then do the slow work
                        // (send_draft) and follow up with the result.
                        if let Err(e) = defer_ephemeral(&ctx, &comp).await {
                            warn!("failed to defer Approve: {e}");
                            return;
                        }
                        let handler = self.state.action_handler.clone();
                        let action_id = cid.action_id.clone();
                        let ctx_clone = ctx.clone();
                        let comp_clone = comp.clone();
                        tokio::spawn(async move {
                            let outcome = match handler {
                                Some(h) => h.approve(&action_id).await,
                                None => ApprovalActionOutcome::Failed {
                                    message: "no action handler configured".into(),
                                },
                            };
                            followup(&ctx_clone, &comp_clone, &describe(&outcome)).await;
                        });
                    }
                    Verb::Skip => {
                        if let Err(e) = defer_ephemeral(&ctx, &comp).await {
                            warn!("failed to defer Skip: {e}");
                            return;
                        }
                        let handler = self.state.action_handler.clone();
                        let action_id = cid.action_id.clone();
                        let ctx_clone = ctx.clone();
                        let comp_clone = comp.clone();
                        tokio::spawn(async move {
                            let outcome = match handler {
                                Some(h) => h.skip(&action_id).await,
                                None => ApprovalActionOutcome::Failed {
                                    message: "no action handler configured".into(),
                                },
                            };
                            followup(&ctx_clone, &comp_clone, &describe(&outcome)).await;
                        });
                    }
                    Verb::Revise => {
                        // Opening a modal IS the response — fast enough.
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
                if let Some(allowed) = self.state.allowed_user_id {
                    if modal.user.id != allowed {
                        let _ = modal
                            .create_response(
                                &ctx.http,
                                CreateInteractionResponse::Message(
                                    CreateInteractionResponseMessage::new()
                                        .content("Not authorized.")
                                        .ephemeral(true),
                                ),
                            )
                            .await;
                        return;
                    }
                }
                // Defer the modal submission immediately — the revise work
                // (reasoner call, create_draft, delete_draft) takes well over
                // 3s, which is Discord's interaction ack deadline.
                if let Err(e) = modal
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Defer(
                            CreateInteractionResponseMessage::new().ephemeral(true),
                        ),
                    )
                    .await
                {
                    warn!("failed to defer modal submit: {e}");
                    return;
                }

                let feedback = extract_feedback(&modal.data.components).unwrap_or_default();
                let handler = self.state.action_handler.clone();
                let action_id = cid.action_id.clone();
                let approval_channel = self.state.approval_channel_id;
                let ctx_clone = ctx.clone();
                let modal_clone = modal.clone();

                tokio::spawn(async move {
                    let outcome = match handler {
                        Some(h) => h.revise(&action_id, &feedback).await,
                        None => ApprovalActionOutcome::Failed {
                            message: "no action handler configured".into(),
                        },
                    };

                    let repost = if let ApprovalActionOutcome::Revised { email, draft } =
                        &outcome
                    {
                        Some((email.clone(), draft.clone()))
                    } else {
                        None
                    };

                    // Follow up on the deferred modal interaction.
                    let message = describe(&outcome);
                    if let Err(e) = modal_clone
                        .create_followup(
                            &ctx_clone.http,
                            CreateInteractionResponseFollowup::new()
                                .content(message)
                                .ephemeral(true),
                        )
                        .await
                    {
                        warn!(action_id = %action_id, "revise: followup failed: {e}");
                    }

                    if let Some((email, draft)) = repost {
                        let msg = approval_message(&action_id, &email, &draft);
                        if let Err(e) = approval_channel.send_message(&ctx_clone.http, msg).await {
                            warn!(
                                action_id = %action_id,
                                "revise: failed to re-post approval card: {e}"
                            );
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

/// Immediate, within-budget ack for a component interaction (Approve / Skip)
/// before we start slow work. Follow up with `followup` once the work is done.
async fn defer_ephemeral(
    ctx: &Context,
    comp: &serenity::all::ComponentInteraction,
) -> Result<(), serenity::Error> {
    comp.create_response(
        &ctx.http,
        CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        ),
    )
    .await
}

/// Post the result of the deferred component interaction.
async fn followup(
    ctx: &Context,
    comp: &serenity::all::ComponentInteraction,
    message: &str,
) {
    if let Err(e) = comp
        .create_followup(
            &ctx.http,
            CreateInteractionResponseFollowup::new()
                .content(message)
                .ephemeral(true),
        )
        .await
    {
        warn!("failed to send followup: {e}");
    }
}

/// Non-deferred immediate ephemeral ack — used only for the authorization
/// rejection path, where we have no slow work to do.
async fn ack_ephemeral(
    ctx: &Context,
    comp: &serenity::all::ComponentInteraction,
    message: &str,
) {
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

fn describe(outcome: &ApprovalActionOutcome) -> String {
    match outcome {
        ApprovalActionOutcome::NotFound => {
            "No record of that approval — it may have been cleared.".into()
        }
        ApprovalActionOutcome::AlreadyResolved { status } => {
            format!("Already resolved ({status}).")
        }
        ApprovalActionOutcome::Approved => "Approved — sending.".into(),
        ApprovalActionOutcome::Skipped => "Skipped — draft discarded.".into(),
        ApprovalActionOutcome::Revised { .. } => "Revising — new draft posted below.".into(),
        ApprovalActionOutcome::Failed { message } => format!("Failed: {message}"),
    }
}

/// Split a wiki answer into Discord-friendly chunks.
pub fn chunk_for_discord(full: &str) -> Vec<String> {
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
