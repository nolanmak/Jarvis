//! Serenity event handler.
//!
//! Routes button clicks + modal submits to the injected `ApprovalActionHandler`
//! (which owns sqlite + Gmail + reasoner access). Approvals are resolved via
//! the database, so old cards remain valid indefinitely.
//!
//! Also routes qualifying messages in the query channel (or DMs) to the
//! `QueryHandler` for wiki-ask answers.

use std::path::PathBuf;
use std::sync::Arc;

use serenity::all::{
    ActionRowComponent, Attachment, ButtonKind, ChannelId, Context, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage,
    EventHandler, GetMessages, Interaction, Message, MessageId, MessageReference, Ready, UserId,
};
use tracing::{debug, info, warn};

use augmentagent_store::Store;

use crate::broker::BrokerState;
use crate::custom_id::{CustomId, Verb};
use crate::layout::{
    approval_message, extract_feedback, extract_fill_values, fill_ask_modal, fill_feedback,
    revise_modal, split_needs_input,
};
use crate::ApprovalActionOutcome;

const DISCORD_MSG_LIMIT: usize = 1900;

pub struct Handler {
    pub state: Arc<BrokerState>,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("discord broker ready as {}", ready.user.name);
        // Stash the bot's own user id so `message()` can tell the bot's prior
        // replies apart from the user's questions when building conversation
        // context for follow-ups.
        let bot_user_id = ready.user.id;
        let _ = self.state.bot_user_id.set(bot_user_id);
        self.state.mark_ready();

        // One-shot scrollback sweep: delete approval cards whose actions are
        // already resolved. Catches cards left from previous runs or from
        // sends that crashed before the active-path cleanup ran.
        if let Some(handler) = self.state.action_handler.clone() {
            let channel_id = self.state.approval_channel_id;
            let ctx_clone = ctx.clone();
            tokio::spawn(async move {
                sweep_resolved_cards(&ctx_clone, channel_id, bot_user_id, handler).await;
            });
        }
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

        let user_text = msg.content.trim().to_string();
        let images = filter_image_attachments(&msg.attachments);
        if user_text.is_empty() && images.is_empty() {
            return;
        }

        // Invoice config command — handled inline, not routed to the wiki
        // query handler. Allowlist was already enforced above.
        if user_text.starts_with("!invoice") {
            let reply = handle_invoice_command(self.state.invoice_store.as_deref(), &user_text);
            let builder = CreateMessage::new()
                .content(reply)
                .reference_message(MessageReference::from((msg.channel_id, msg.id)));
            if let Err(e) = msg.channel_id.send_message(&ctx.http, builder).await {
                warn!("failed to post invoice command reply: {e}");
            }
            return;
        }

        // `loop` / `/loop` — user-defined scheduled tasks (#104). Handled
        // inline like `!invoice`; never routed to the wiki query handler.
        // Leading `/` is optional — `match_loop_prefix` accepts either form
        // as long as it's word-bounded (so `loops are nice` isn't matched).
        if crate::loops::match_loop_prefix(&user_text).is_some() {
            let owner = msg.author.id.get().to_string();
            let channel_ref = msg.channel_id.get().to_string();
            let reply = crate::handle_loop_command(
                self.state.store.as_deref(),
                self.state.loop_parser.as_deref(),
                &owner,
                &channel_ref,
                &user_text,
            )
            .await;
            for chunk in chunk_for_discord(&reply) {
                let builder = CreateMessage::new()
                    .content(chunk)
                    .reference_message(MessageReference::from((msg.channel_id, msg.id)));
                if let Err(e) = msg.channel_id.send_message(&ctx.http, builder).await {
                    warn!("failed to post loop command reply: {e}");
                    break;
                }
            }
            return;
        }

        let handler = Arc::clone(handler);
        let ctx_for_history = ctx.clone();
        let http = ctx.http.clone();
        let channel_id = msg.channel_id;
        let msg_id = msg.id;
        let bot_user_id = self.state.bot_user_id.get().copied();
        let allowed_user_id = self.state.allowed_user_id;

        tokio::spawn(async move {
            // Fetch recent messages in this channel/DM so follow-up questions
            // see the prior exchange. Bounded by age + char cap inside the fn.
            let history = fetch_conversation_context(
                &ctx_for_history,
                channel_id,
                msg_id,
                bot_user_id,
                allowed_user_id,
            )
            .await;

            // Download images to /tmp so the reasoner's Read tool can open them.
            // A partial download (some succeed, some fail) is fine — we proceed
            // with whatever landed and warn about the rest.
            let downloaded = download_images(&images, msg_id.get()).await;

            let prompt = build_prompt_with_context(&history, &user_text, &downloaded);

            let result = handler.answer(&prompt).await;

            // Best-effort cleanup. Image tmpfiles aren't load-bearing for the
            // reply we're about to post, so we tolerate failures.
            for path in &downloaded {
                if let Err(e) = tokio::fs::remove_file(path).await {
                    warn!("failed to remove tempfile {}: {e}", path.display());
                }
            }

            match result {
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
                            if should_delete_source(&outcome) {
                                delete_source_message(
                                    &ctx_clone,
                                    comp_clone.channel_id,
                                    comp_clone.message.id,
                                    &action_id,
                                )
                                .await;
                            }
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
                            if should_delete_source(&outcome) {
                                delete_source_message(
                                    &ctx_clone,
                                    comp_clone.channel_id,
                                    comp_clone.message.id,
                                    &action_id,
                                )
                                .await;
                            }
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
                    Verb::FillAsk => {
                        // "Provide missing info" (#35 Phase 5). Re-read the
                        // persisted draft, decode the needs-input marker, and
                        // open a modal pre-labeled with each unresolved ask.
                        // Opening a modal IS the interaction response (fast).
                        let needs = self
                            .state
                            .store
                            .as_ref()
                            .and_then(|s| {
                                s.get_action_with_email(&cid.action_id).ok().flatten()
                            })
                            .and_then(|a| a.action.draft_body)
                            .map(|d| split_needs_input(&d).1)
                            .unwrap_or_default();
                        if needs.is_empty() {
                            ack_ephemeral(
                                &ctx,
                                &comp,
                                "Nothing left to fill in for this draft.",
                            )
                            .await;
                            return;
                        }
                        let modal = fill_ask_modal(&cid.action_id, &needs);
                        if let Err(e) = comp
                            .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                            .await
                        {
                            warn!("failed to open fill-ask modal: {e}");
                        }
                    }
                    Verb::QuickRefine => {
                        // Quick-refine StringSelect (#34): map the chosen
                        // preset id → canned feedback, route it through the
                        // SAME revise path as the free-form modal, re-render
                        // the card with the menu still attached so presets
                        // stack. Enforce MAX_REDRAFT_ITERATIONS.
                        let selected = match &comp.data.kind {
                            serenity::all::ComponentInteractionDataKind::StringSelect {
                                values,
                            } => values.first().cloned(),
                            _ => None,
                        };
                        let Some(preset_id) = selected else {
                            debug!("quick_refine: no preset selected");
                            return;
                        };
                        let Some(preset) = crate::presets::lookup(&preset_id) else {
                            ack_ephemeral(&ctx, &comp, "Unknown refine preset.").await;
                            return;
                        };
                        if let Err(e) = defer_ephemeral(&ctx, &comp).await {
                            warn!("failed to defer QuickRefine: {e}");
                            return;
                        }
                        let handler = self.state.action_handler.clone();
                        let action_id = cid.action_id.clone();
                        let approval_channel = self.state.approval_channel_id;
                        let store_for_capture = self.state.store.clone();
                        let feedback = preset.feedback.to_string();
                        let preset_id_owned = preset.id.to_string();
                        let ctx_clone = ctx.clone();
                        let comp_clone = comp.clone();

                        tokio::spawn(async move {
                            // Enforce the iteration cap before doing the work.
                            let already = store_for_capture
                                .as_ref()
                                .and_then(|s| s.redraft_count(&action_id).ok())
                                .unwrap_or(0);
                            if already >= crate::presets::MAX_REDRAFT_ITERATIONS {
                                followup(
                                    &ctx_clone,
                                    &comp_clone,
                                    "Refine limit reached for this draft — Approve, Skip, or use Revise for a free-form edit.",
                                )
                                .await;
                                return;
                            }

                            // Snapshot the pre-redraft draft BEFORE revising
                            // (revise mutates draftBody in place) — needed for
                            // the (orig, feedback, revised) eval triple (#37).
                            let original_draft = store_for_capture
                                .as_ref()
                                .and_then(|s| {
                                    s.get_action_with_email(&action_id).ok().flatten()
                                })
                                .map(|a| a.action.draft_body.unwrap_or_default());

                            let outcome = match handler {
                                Some(h) => h.revise(&action_id, &feedback).await,
                                None => ApprovalActionOutcome::Failed {
                                    message: "no action handler configured".into(),
                                },
                            };

                            let repost = if let ApprovalActionOutcome::Revised {
                                email,
                                draft,
                            } = &outcome
                            {
                                if let (Some(store), Some(orig)) = (
                                    store_for_capture.as_ref(),
                                    original_draft.as_ref(),
                                ) {
                                    if let Err(e) = store.record_revision_triple(
                                        &action_id, orig, &feedback, draft,
                                    ) {
                                        warn!(
                                            action_id = %action_id,
                                            "quick_refine: record triple failed: {e}"
                                        );
                                    }
                                }
                                Some((email.clone(), draft.clone()))
                            } else {
                                None
                            };

                            followup(&ctx_clone, &comp_clone, &describe(&outcome)).await;

                            let mut new_card_posted = false;
                            if let Some((email, draft)) = repost {
                                // Persist preset choice + bump the counter, then
                                // re-render with the post-increment count so the
                                // card shows the right version and drops the
                                // menu once the cap is hit.
                                let count = match store_for_capture.as_ref() {
                                    Some(s) => s
                                        .record_redraft(&action_id, Some(&preset_id_owned))
                                        .unwrap_or_else(|e| {
                                            warn!(
                                                action_id = %action_id,
                                                "quick_refine: record_redraft failed: {e}"
                                            );
                                            0
                                        }),
                                    None => 0,
                                };
                                info!(
                                    action_id = %action_id,
                                    preset = %preset_id_owned,
                                    redraft_count = count,
                                    "quick_refine applied"
                                );
                                let msg =
                                    approval_message(&action_id, &email, &draft, count);
                                match approval_channel
                                    .send_message(&ctx_clone.http, msg)
                                    .await
                                {
                                    Ok(_) => new_card_posted = true,
                                    Err(e) => warn!(
                                        action_id = %action_id,
                                        "quick_refine: re-post card failed: {e}"
                                    ),
                                }
                            }

                            let delete_old = match &outcome {
                                ApprovalActionOutcome::Revised { .. } => new_card_posted,
                                ApprovalActionOutcome::AlreadyResolved { .. } => true,
                                _ => false,
                            };
                            if delete_old {
                                delete_source_message(
                                    &ctx_clone,
                                    comp_clone.channel_id,
                                    comp_clone.message.id,
                                    &action_id,
                                )
                                .await;
                            }
                        });
                    }
                    Verb::ReviseModal | Verb::FillAskModal => {
                        debug!("unexpected modal verb on component interaction");
                    }
                }
            }
            Interaction::Modal(modal) => {
                let Some(cid) = CustomId::parse(&modal.data.custom_id) else {
                    return;
                };
                if !matches!(cid.verb, Verb::ReviseModal | Verb::FillAskModal) {
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

                let feedback = match cid.verb {
                    Verb::FillAskModal => {
                        // Re-derive the unresolved asks from the persisted
                        // draft so submitted values pair to the right ask,
                        // then turn them into Revise feedback. Routing
                        // through the SAME revise path below means the draft
                        // is re-written with the concrete values and the
                        // needs-input marker is naturally dropped (the
                        // re-draft never re-emits it).
                        let needs = self
                            .state
                            .store
                            .as_ref()
                            .and_then(|s| {
                                s.get_action_with_email(&cid.action_id).ok().flatten()
                            })
                            .and_then(|a| a.action.draft_body)
                            .map(|d| split_needs_input(&d).1)
                            .unwrap_or_default();
                        let filled =
                            extract_fill_values(&modal.data.components, &needs);
                        if filled.is_empty() {
                            let _ = modal
                                .create_followup(
                                    &ctx.http,
                                    CreateInteractionResponseFollowup::new()
                                        .content(
                                            "No values supplied — draft unchanged.",
                                        )
                                        .ephemeral(true),
                                )
                                .await;
                            return;
                        }
                        fill_feedback(&filled)
                    }
                    _ => extract_feedback(&modal.data.components).unwrap_or_default(),
                };
                let handler = self.state.action_handler.clone();
                let action_id = cid.action_id.clone();
                let approval_channel = self.state.approval_channel_id;
                let store_for_capture = self.state.store.clone();
                let ctx_clone = ctx.clone();
                let modal_clone = modal.clone();

                tokio::spawn(async move {
                    // Snapshot the pre-Revise draft BEFORE calling revise — the
                    // revise call mutates `actions.draftBody` in-place, so a
                    // post-call read would return the new draft. (#37)
                    let original_draft = store_for_capture
                        .as_ref()
                        .and_then(|s| s.get_action_with_email(&action_id).ok().flatten())
                        .map(|a| a.action.draft_body.unwrap_or_default());

                    let outcome = match handler {
                        Some(h) => h.revise(&action_id, &feedback).await,
                        None => ApprovalActionOutcome::Failed {
                            message: "no action handler configured".into(),
                        },
                    };

                    let repost = if let ApprovalActionOutcome::Revised { email, draft } =
                        &outcome
                    {
                        // Capture the (original, feedback, revised) triple for
                        // the draft-quality eval corpus (#37). Best-effort: a
                        // store error here must not break the user-facing
                        // Revise flow, so we just log and move on.
                        if let (Some(store), Some(orig)) =
                            (store_for_capture.as_ref(), original_draft.as_ref())
                        {
                            match store.record_revision_triple(
                                &action_id,
                                orig,
                                &feedback,
                                draft,
                            ) {
                                Ok(rev_id) => debug!(
                                    action_id = %action_id,
                                    revision_id = %rev_id,
                                    "revise: captured triple"
                                ),
                                Err(e) => warn!(
                                    action_id = %action_id,
                                    "revise: failed to record triple: {e}"
                                ),
                            }
                        }
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

                    // Post the new card BEFORE deleting the old one. If the
                    // re-post fails the old card stays as a fallback so the
                    // user isn't left without a card to act on.
                    let mut new_card_posted = false;
                    if let Some((email, draft)) = repost {
                        // Free-form Revise counts toward the visible draft
                        // version (#34) but records no preset. It's the
                        // explicit escape hatch, so we never block it.
                        let count = match store_for_capture.as_ref() {
                            Some(s) => s.record_redraft(&action_id, None).unwrap_or_else(|e| {
                                warn!(action_id = %action_id, "revise: record_redraft failed: {e}");
                                0
                            }),
                            None => 0,
                        };
                        let msg = approval_message(&action_id, &email, &draft, count);
                        match approval_channel.send_message(&ctx_clone.http, msg).await {
                            Ok(_) => new_card_posted = true,
                            Err(e) => warn!(
                                action_id = %action_id,
                                "revise: failed to re-post approval card: {e}"
                            ),
                        }
                    }

                    // Delete the original card the modal was opened from. On
                    // Revised, only delete if the replacement actually posted.
                    // On AlreadyResolved, delete unconditionally — the card is
                    // stale and there's nothing to replace it with.
                    let delete_old = match &outcome {
                        ApprovalActionOutcome::Revised { .. } => new_card_posted,
                        ApprovalActionOutcome::AlreadyResolved { .. } => true,
                        _ => false,
                    };
                    if delete_old {
                        if let Some(src) = modal_clone.message.as_ref() {
                            delete_source_message(
                                &ctx_clone,
                                modal_clone.channel_id,
                                src.id,
                                &action_id,
                            )
                            .await;
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

/// Should the source approval card be deleted after this outcome?
///
/// Approved / Skipped / Revised: yes — the card is now stale or superseded.
/// AlreadyResolved: yes — this is, by definition, a stale card.
/// NotFound / Failed: no — leave the card so the user can investigate / retry.
fn should_delete_source(outcome: &ApprovalActionOutcome) -> bool {
    matches!(
        outcome,
        ApprovalActionOutcome::Approved
            | ApprovalActionOutcome::Skipped
            | ApprovalActionOutcome::Revised { .. }
            | ApprovalActionOutcome::AlreadyResolved { .. }
    )
}

/// Best-effort deletion of an approval card. Failures are cosmetic (the
/// product-side action already succeeded), so we just warn.
async fn delete_source_message(
    ctx: &Context,
    channel_id: ChannelId,
    message_id: MessageId,
    action_id: &str,
) {
    if let Err(e) = channel_id.delete_message(&ctx.http, message_id).await {
        warn!(
            action_id = %action_id,
            "failed to delete resolved approval card: {e}"
        );
    }
}

/// On startup, scan the most recent ~100 messages in the approval channel and
/// delete any approval cards whose underlying action is already resolved
/// (sent / rejected / etc). This handles cards left over from prior runs or
/// from interactions that crashed before the active-path cleanup ran.
async fn sweep_resolved_cards(
    ctx: &Context,
    channel_id: ChannelId,
    bot_user_id: UserId,
    handler: std::sync::Arc<dyn crate::ApprovalActionHandler>,
) {
    let messages = match channel_id
        .messages(&ctx.http, GetMessages::new().limit(100))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            warn!("startup sweep: failed to fetch approval channel history: {e}");
            return;
        }
    };

    let mut deleted = 0usize;
    let mut scanned = 0usize;
    for msg in messages {
        if msg.author.id != bot_user_id {
            continue;
        }
        let Some(action_id) = action_id_from_message(&msg) else {
            continue;
        };
        scanned += 1;
        if !handler.is_resolved(&action_id).await {
            continue;
        }
        if let Err(e) = channel_id.delete_message(&ctx.http, msg.id).await {
            warn!(
                action_id = %action_id,
                "startup sweep: failed to delete stale card: {e}"
            );
        } else {
            deleted += 1;
        }
    }
    info!(
        scanned = scanned,
        deleted = deleted,
        "startup sweep: cleaned up resolved approval cards"
    );
}

/// Walk a message's components looking for the first parseable approval-card
/// button `custom_id`. Returns the `action_id` if found; `None` for any
/// message that isn't one of our cards.
fn action_id_from_message(msg: &Message) -> Option<String> {
    for row in &msg.components {
        for component in &row.components {
            if let ActionRowComponent::Button(button) = component {
                if let ButtonKind::NonLink { custom_id, .. } = &button.data {
                    if let Some(parsed) = crate::custom_id::CustomId::parse(custom_id) {
                        return Some(parsed.action_id);
                    }
                }
            }
        }
    }
    None
}

/// Keep only attachments whose Discord-reported `content_type` looks like an
/// image. Discord populates this from the upload, so it's reliable enough to
/// gate which attachments we hand to the vision model.
/// Handle a `!invoice …` config command. Pure: reads/writes the invoice_config
/// store rows and returns the text to reply with. Recognized:
///   `!invoice status`
///   `!invoice recipient <email>`
///   `!invoice entity <composio_entity_id>`
///   `!invoice autosend on|off`
fn handle_invoice_command(store: Option<&Store>, text: &str) -> String {
    let Some(store) = store else {
        return "invoice config unavailable (store not wired)".to_string();
    };
    let parts: Vec<&str> = text.split_whitespace().collect();
    let g = |k: &str| store.get_invoice_config(k).ok().flatten().unwrap_or_default();
    match parts.get(1).copied() {
        Some("status") => {
            let recipient = {
                let r = g("recipient_email");
                if r.is_empty() { "(unset — set via dashboard or `!invoice recipient`)".to_string() } else { r }
            };
            let entity = g("from_entity");
            let last = g("last_billed_week_end");
            let autosend = g("auto_send_enabled") == "true";
            format!(
                "📄 **Invoice config**\n• auto-send: **{}**\n• recipient: `{}`\n• next #: `{}`\n• sending entity: `{}`\n• last billed week: `{}`",
                if autosend { "ON" } else { "OFF (scheduler will not send)" },
                recipient,
                store.invoice_counter().unwrap_or(35),
                if entity.is_empty() { "(unset)".to_string() } else { entity },
                if last.is_empty() { "(never)".to_string() } else { last },
            )
        }
        Some("recipient") => match parts.get(2) {
            Some(email) if email.contains('@') && email.contains('.') => {
                match store.set_invoice_config("recipient_email", email) {
                    Ok(()) => format!("✅ invoice recipient set to `{email}`"),
                    Err(e) => format!("⚠️ failed to set recipient: {e}"),
                }
            }
            _ => "usage: `!invoice recipient you@example.com`".to_string(),
        },
        Some("entity") => match parts.get(2) {
            Some(ent) if !ent.is_empty() => {
                match store.set_invoice_config("from_entity", ent) {
                    Ok(()) => format!("✅ sending entity set to `{ent}`"),
                    Err(e) => format!("⚠️ failed to set entity: {e}"),
                }
            }
            _ => "usage: `!invoice entity <composio_entity_id>`".to_string(),
        },
        Some("autosend") => match parts.get(2).map(|s| s.to_ascii_lowercase()) {
            Some(v) if v == "on" || v == "true" => {
                match store.set_invoice_config("auto_send_enabled", "true") {
                    Ok(()) => "✅ invoice auto-send **ON** — the scheduler will send on Sundays".to_string(),
                    Err(e) => format!("⚠️ failed to enable auto-send: {e}"),
                }
            }
            Some(v) if v == "off" || v == "false" => {
                match store.set_invoice_config("auto_send_enabled", "false") {
                    Ok(()) => "🛑 invoice auto-send **OFF** — the scheduler will not send".to_string(),
                    Err(e) => format!("⚠️ failed to disable auto-send: {e}"),
                }
            }
            _ => "usage: `!invoice autosend on|off`".to_string(),
        },
        _ => "invoice commands:\n• `!invoice status`\n• `!invoice recipient <email>`\n• `!invoice entity <composio_entity_id>`\n• `!invoice autosend on|off`"
            .to_string(),
    }
}

fn filter_image_attachments(attachments: &[Attachment]) -> Vec<Attachment> {
    attachments
        .iter()
        .filter(|a| {
            a.content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("image/"))
        })
        .cloned()
        .collect()
}

/// Pick a sensible file extension for the tempfile. Prefer the URL filename's
/// extension, otherwise derive from the MIME subtype, otherwise fall back to
/// `bin`.
fn extension_for(att: &Attachment) -> String {
    if let Some(ext) = std::path::Path::new(&att.filename)
        .extension()
        .and_then(|e| e.to_str())
    {
        if !ext.is_empty() {
            return ext.to_lowercase();
        }
    }
    if let Some(ct) = att.content_type.as_deref() {
        if let Some(rest) = ct.strip_prefix("image/") {
            // image/jpeg → jpg; everything else passes through.
            let ext = match rest {
                "jpeg" => "jpg",
                other => other,
            };
            return ext.to_string();
        }
    }
    "bin".into()
}

/// Download each attachment to `/tmp/aa-img-<msg_id>-<idx>.<ext>`. Returns the
/// paths that succeeded; failures are logged and skipped.
async fn download_images(attachments: &[Attachment], msg_id: u64) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(attachments.len());
    for (idx, att) in attachments.iter().enumerate() {
        let ext = extension_for(att);
        let path = PathBuf::from(format!("/tmp/aa-img-{msg_id}-{idx}.{ext}"));
        match reqwest::get(&att.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => match tokio::fs::write(&path, &bytes).await {
                    Ok(()) => out.push(path),
                    Err(e) => warn!("write image tempfile {} failed: {e}", path.display()),
                },
                Err(e) => warn!("read image bytes from {} failed: {e}", att.url),
            },
            Err(e) => warn!("download image {} failed: {e}", att.url),
        }
    }
    out
}

/// Combine the user's text (possibly empty) with a list of locally-downloaded
/// image paths, instructing Claude to use the Read tool to inspect them.
fn build_prompt(user_text: &str, images: &[PathBuf]) -> String {
    if images.is_empty() {
        return user_text.to_string();
    }
    let mut s = String::new();
    if !user_text.is_empty() {
        s.push_str(user_text);
        s.push_str("\n\n");
    }
    s.push_str("[attached images to analyze]\n");
    for path in images {
        s.push_str("- ");
        s.push_str(&path.display().to_string());
        s.push('\n');
    }
    s.push_str("\nUse the Read tool to view each image and answer based on them.");
    s
}

/// Layer a pre-formatted `<conversation_history>` block in front of the
/// current-turn prompt. If `history` is empty, falls through to the bare
/// `build_prompt` so first-turn messages match prior behavior exactly.
fn build_prompt_with_context(history: &str, user_text: &str, images: &[PathBuf]) -> String {
    let current = build_prompt(user_text, images);
    if history.is_empty() {
        return current;
    }
    format!("{history}\n\nuser's current message:\n{current}")
}

/// Conversation-history constants. Tuned by hand for the single-user DM case.
const HISTORY_LIMIT: u8 = 30;
const MAX_AGE_SECS: i64 = 2 * 60 * 60; // 2 hours
const HISTORY_CHAR_CAP: usize = 10_000;

/// Pull recent messages from the channel/DM and format them as a role-tagged
/// transcript. Empty string when there's nothing to include (first turn, or
/// everything exceeded the age cap, or fetch failed).
async fn fetch_conversation_context(
    ctx: &Context,
    channel_id: ChannelId,
    before: MessageId,
    bot_user_id: Option<UserId>,
    allowed_user_id: Option<UserId>,
) -> String {
    let builder = GetMessages::new().before(before).limit(HISTORY_LIMIT);
    let messages = match channel_id.messages(&ctx.http, builder).await {
        Ok(m) => m,
        Err(e) => {
            warn!("fetch conversation history failed: {e}");
            return String::new();
        }
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now_secs - MAX_AGE_SECS;

    // Discord returns newest-first. Process newest → oldest but emit in
    // chronological order at the end.
    let mut turns: Vec<(&'static str, String)> = Vec::with_capacity(messages.len());
    for m in messages.into_iter() {
        if m.timestamp.unix_timestamp() < cutoff {
            continue;
        }
        let role = if bot_user_id == Some(m.author.id) {
            "assistant"
        } else {
            match allowed_user_id {
                Some(allowed) if m.author.id != allowed => continue,
                _ => "user",
            }
        };
        let mut body = m.content.trim().to_string();
        if !m.attachments.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str("[image attachment]");
        }
        if body.is_empty() {
            continue;
        }
        turns.push((role, body));
    }
    // `messages` was newest-first; the Vec we built is therefore newest-first too.
    turns.reverse();
    format_transcript(&turns, HISTORY_CHAR_CAP)
}

/// Pure renderer so unit tests don't need a live Discord connection.
/// Truncates from the FRONT (oldest) when over `char_cap` — recent context
/// is more valuable than ancient context.
fn format_transcript(turns: &[(&str, String)], char_cap: usize) -> String {
    if turns.is_empty() {
        return String::new();
    }
    let mut kept: Vec<String> = Vec::with_capacity(turns.len());
    let mut total = 0usize;
    for (role, body) in turns.iter().rev() {
        let entry = format!("{role}: {body}\n");
        if total + entry.len() > char_cap {
            break;
        }
        total += entry.len();
        kept.push(entry);
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.reverse();
    let mut out = String::with_capacity(total + 64);
    out.push_str("<conversation_history>\n");
    for entry in kept {
        out.push_str(&entry);
    }
    out.push_str("</conversation_history>");
    out
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

    #[test]
    fn transcript_empty_for_empty_input() {
        assert_eq!(format_transcript(&[], 10_000), "");
    }

    #[test]
    fn transcript_preserves_chronological_order() {
        let turns = vec![
            ("user", "first question".to_string()),
            ("assistant", "first answer".to_string()),
            ("user", "follow-up".to_string()),
        ];
        let out = format_transcript(&turns, 10_000);
        assert!(out.starts_with("<conversation_history>\n"));
        assert!(out.ends_with("</conversation_history>"));
        // Earliest turn should appear before later ones.
        let i1 = out.find("first question").unwrap();
        let i2 = out.find("first answer").unwrap();
        let i3 = out.find("follow-up").unwrap();
        assert!(i1 < i2 && i2 < i3);
    }

    #[test]
    fn transcript_drops_oldest_when_over_cap() {
        let turns = vec![
            ("user", "A".repeat(200)),
            ("assistant", "B".repeat(200)),
            ("user", "C".repeat(200)),
        ];
        // Cap small enough that only the last two fit.
        let out = format_transcript(&turns, 500);
        assert!(!out.contains(&"A".repeat(200)));
        assert!(out.contains(&"B".repeat(200)));
        assert!(out.contains(&"C".repeat(200)));
    }

    #[test]
    fn transcript_returns_empty_when_nothing_fits() {
        let turns = vec![("user", "x".repeat(10_000))];
        assert_eq!(format_transcript(&turns, 100), "");
    }

    #[test]
    fn build_prompt_without_history_matches_bare_prompt() {
        let got = build_prompt_with_context("", "hello", &[]);
        assert_eq!(got, "hello");
    }

    #[test]
    fn build_prompt_layers_history_before_current_message() {
        let history = "<conversation_history>\nuser: hi\nassistant: hello\n</conversation_history>";
        let got = build_prompt_with_context(history, "what's next?", &[]);
        assert!(got.starts_with("<conversation_history>"));
        assert!(got.contains("user's current message:\nwhat's next?"));
    }

    fn att(filename: &str, content_type: Option<&str>) -> Attachment {
        // Round-trip a minimal Attachment through serde_json so we don't have
        // to hand-construct every field of the upstream type.
        let json = serde_json::json!({
            "id": "1",
            "filename": filename,
            "content_type": content_type,
            "size": 1u64,
            "url": "https://cdn.discordapp.com/example.png",
            "proxy_url": "https://cdn.discordapp.com/example.png",
            "ephemeral": false,
        });
        serde_json::from_value(json).expect("attachment fixture should deserialize")
    }

    #[test]
    fn filter_keeps_only_image_content_types() {
        let atts = vec![
            att("a.png", Some("image/png")),
            att("b.pdf", Some("application/pdf")),
            att("c.jpg", Some("image/jpeg")),
            att("d.txt", None),
        ];
        let images = filter_image_attachments(&atts);
        let names: Vec<&str> = images.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(names, vec!["a.png", "c.jpg"]);
    }

    #[test]
    fn extension_prefers_filename_then_mime() {
        assert_eq!(extension_for(&att("photo.JPG", Some("image/jpeg"))), "jpg");
        assert_eq!(extension_for(&att("noext", Some("image/png"))), "png");
        assert_eq!(extension_for(&att("noext", Some("image/jpeg"))), "jpg");
        assert_eq!(extension_for(&att("noext", None)), "bin");
    }

    #[test]
    fn build_prompt_with_only_images_does_not_panic_on_empty_text() {
        let images = vec![PathBuf::from("/tmp/aa-img-42-0.png")];
        let prompt = build_prompt("", &images);
        assert!(prompt.contains("[attached images to analyze]"));
        assert!(prompt.contains("/tmp/aa-img-42-0.png"));
        assert!(prompt.contains("Use the Read tool"));
    }

    #[test]
    fn build_prompt_combines_text_and_images() {
        let images = vec![
            PathBuf::from("/tmp/aa-img-7-0.png"),
            PathBuf::from("/tmp/aa-img-7-1.jpg"),
        ];
        let prompt = build_prompt("what's in this?", &images);
        assert!(prompt.starts_with("what's in this?"));
        assert!(prompt.contains("/tmp/aa-img-7-0.png"));
        assert!(prompt.contains("/tmp/aa-img-7-1.jpg"));
    }

    #[test]
    fn build_prompt_without_images_returns_plain_text() {
        let prompt = build_prompt("hello", &[]);
        assert_eq!(prompt, "hello");
    }
}
