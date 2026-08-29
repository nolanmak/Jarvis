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
    ActionRowComponent, Attachment, ButtonKind, ChannelId, Context, CreateAttachment,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateMessage, EventHandler, GetMessages, Http, Interaction, Message, MessageId,
    MessageReference, Ready, UserId,
};
use tracing::{debug, info, warn};

use crate::broker::BrokerState;
use crate::custom_id::{CustomId, Verb};
use crate::layout::{
    approval_message, extract_feedback, extract_fill_values, fill_ask_modal, fill_feedback,
    revise_modal, schedule_modal, split_needs_input, SCHEDULE_CUSTOM_VALUE,
};
use crate::ApprovalActionOutcome;

const DISCORD_MSG_LIMIT: usize = 1900;

/// Fail-closed owner-allowlist check (#303). Returns `true` only when an
/// allowlist is configured (`DISCORD_ALLOWED_USER_ID`) *and* the actor matches
/// it. When `allowed_user_id` is `None` this returns `false`, so every gated
/// action is refused rather than served to an arbitrary Discord user — the
/// opposite of the previous `if let Some(allowed) = …` guards, which skipped
/// the check entirely (fail-open) when no allowlist was configured.
fn is_authorized(allowed_user_id: Option<UserId>, actor: UserId) -> bool {
    matches!(allowed_user_id, Some(allowed) if allowed == actor)
}

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

        if !is_authorized(self.state.allowed_user_id, msg.author.id) {
            debug!(
                "ignoring message from unauthorized user {} (allowlist configured: {})",
                msg.author.id.get(),
                self.state.allowed_user_id.is_some()
            );
            return;
        }

        let user_text = msg.content.trim().to_string();
        let AttachmentPartition {
            images,
            text_files,
            docs,
            rejected,
        } = partition_attachments(&msg.attachments);
        if user_text.is_empty()
            && images.is_empty()
            && text_files.is_empty()
            && docs.is_empty()
        {
            // Rejected-only path: tell the user what was dropped, don't ping
            // the reasoner. Otherwise the message is truly empty — return.
            if let Some(footer) = format_rejection_footer(&rejected) {
                let builder = CreateMessage::new()
                    .content(footer)
                    .reference_message(MessageReference::from((msg.channel_id, msg.id)));
                if let Err(e) = msg.channel_id.send_message(&ctx.http, builder).await {
                    warn!("failed to post rejection footer: {e}");
                }
            }
            return;
        }

        // `!loops` — list / stop running `claude` CLI processes (#176).
        // Distinct from `/loop` below (which is the in-process scheduler).
        // Matched first because the visual similarity to `/loop` makes
        // ordering load-bearing. Owner gating: the allowlist check at the
        // top of this handler already returned for non-allowed users; the
        // refusal in `process_loops::handle` covers the no-allowlist case
        // (without an allowlist, *anyone* DMing the bot could kill host
        // processes — refuse the command entirely in that mode).
        if user_text == "!loops" || user_text.starts_with("!loops ") {
            let allowlist_active = self.state.allowed_user_id.is_some();
            let text = user_text.clone();
            let http = ctx.http.clone();
            let channel_id = msg.channel_id;
            let msg_id = msg.id;
            tokio::spawn(async move {
                let reply = crate::process_loops::handle(&text, allowlist_active).await;
                for chunk in chunk_for_discord(&reply) {
                    let builder = CreateMessage::new()
                        .content(chunk)
                        .reference_message(MessageReference::from((channel_id, msg_id)));
                    if let Err(e) = channel_id.send_message(&*http, builder).await {
                        warn!("failed to post !loops reply: {e}");
                        break;
                    }
                }
            });
            return;
        }

        // `loop` / `/loop` — user-defined scheduled tasks (#104). Handled
        // inline; never routed to the wiki query handler.
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
            send_chunks_reply_chain(
                &ctx.http,
                msg.channel_id,
                msg.id,
                chunk_for_discord(&reply),
                Vec::new(),
                "loop command reply",
            )
            .await;
            return;
        }

        let handler = Arc::clone(handler);
        let ctx_for_history = ctx.clone();
        let http = ctx.http.clone();
        let channel_id = msg.channel_id;
        let msg_id = msg.id;
        let bot_user_id = self.state.bot_user_id.get().copied();
        let allowed_user_id = self.state.allowed_user_id;
        let wiki_root = self.state.wiki_root.clone();

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

            // Download attachments to /tmp so the reasoner's Read tool can open
            // them. Partial success is fine — we proceed with whatever landed
            // and warn about the rest. PDF/DOCX attachments are converted to
            // text via pdftotext/pandoc and join downloaded_txts so the prompt
            // path treats them uniformly.
            let downloaded_imgs = download_images(&images, msg_id.get()).await;
            let mut downloaded_txts = download_text_files(&text_files, msg_id.get()).await;
            let extracted_docs = extract_doc_attachments(&docs, msg_id.get()).await;
            downloaded_txts.extend(extracted_docs);

            let prompt = build_prompt_with_context(
                &history,
                &user_text,
                &downloaded_imgs,
                &downloaded_txts,
            );

            // #125: Liveness signal. Discord's typing indicator auto-expires
            // after ~10s, so we kick one off immediately and re-broadcast on a
            // ~9s tick until the reasoner returns. Failures here are cosmetic
            // (network blip, missing perm) — log and keep going so a typing
            // glitch never blocks the actual reply.
            //
            // #132 / #201 — Build the per-request audit context so the
            // reasoner can record tool calls into the NDJSON audit log and
            // post side-channel notifications back to THIS channel on
            // high-risk tool calls (Write/Edit/Bash/...).
            let audit_ctx = crate::AuditCtx {
                session_id: format!("{}:{}", channel_id, msg_id),
                http: Some(http.clone()),
                channel_id: Some(channel_id),
            };
            let result =
                run_with_typing(&http, channel_id, handler.answer(&audit_ctx, &prompt)).await;

            // Best-effort cleanup. Tempfiles aren't load-bearing for the reply
            // we're about to post, so we tolerate failures.
            for path in downloaded_imgs
                .iter()
                .chain(downloaded_txts.iter().map(|f| &f.path))
            {
                if let Err(e) = tokio::fs::remove_file(path).await {
                    warn!("failed to remove tempfile {}: {e}", path.display());
                }
            }

            let footer = format_rejection_footer(&rejected);

            match result {
                Ok(answer) => {
                    // #440 — pull `ATTACH:` markers out of the answer and
                    // deliver the referenced wiki files on the first chunk.
                    let (mut answer, attachments) =
                        crate::attachments::prepare_answer_delivery(&answer, wiki_root.as_deref())
                            .await;
                    if let Some(f) = &footer {
                        // Append before chunking so a long answer's footer
                        // still ends up in the final Discord message.
                        answer.push_str("\n\n");
                        answer.push_str(f);
                    }
                    send_chunks_reply_chain(
                        &http,
                        channel_id,
                        msg_id,
                        chunk_for_discord(&answer),
                        attachments,
                        "wiki answer chunk",
                    )
                    .await;
                }
                Err(e) => {
                    let mut err_msg = format!("wiki query failed: {e}");
                    if let Some(f) = &footer {
                        err_msg.push_str("\n\n");
                        err_msg.push_str(f);
                    }
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
                if !is_authorized(self.state.allowed_user_id, comp.user.id) {
                    ack_ephemeral(
                        &ctx,
                        &comp,
                        "You are not authorized to approve replies on this bot.",
                    )
                    .await;
                    return;
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
                    Verb::SchedulePick => {
                        // Schedule select on the card (#501). The symbolic
                        // value is resolved to epoch-ms at CLICK time —
                        // cards sit for hours/days, so render-time
                        // resolution would drift.
                        let selected = match &comp.data.kind {
                            serenity::all::ComponentInteractionDataKind::StringSelect {
                                values,
                            } => values.first().cloned(),
                            _ => None,
                        };
                        let Some(token) = selected else {
                            debug!("schedule_pick: no option selected");
                            return;
                        };
                        if token == SCHEDULE_CUSTOM_VALUE {
                            // "Custom…" opens the free-text modal — a Modal
                            // response to a select interaction is legal in
                            // serenity 0.12 (same shape as Revise). Known
                            // cosmetic artifact: dismissing the modal leaves
                            // the select displaying "Custom…"; re-picking
                            // still works.
                            let modal = schedule_modal(&cid.action_id);
                            if let Err(e) = comp
                                .create_response(
                                    &ctx.http,
                                    CreateInteractionResponse::Modal(modal),
                                )
                                .await
                            {
                                warn!("failed to open schedule modal: {e}");
                            }
                            return;
                        }
                        let at_ms = match crate::timeparse::resolve_token(
                            &token,
                            chrono::Local::now(),
                        ) {
                            Ok(v) => v,
                            Err(msg) => {
                                ack_ephemeral(&ctx, &comp, &msg).await;
                                return;
                            }
                        };
                        // Defer: the handler does store CAS + a Discord
                        // notice post — too slow for the 3s ack budget.
                        if let Err(e) = defer_ephemeral(&ctx, &comp).await {
                            warn!("failed to defer SchedulePick: {e}");
                            return;
                        }
                        let handler = self.state.action_handler.clone();
                        let action_id = cid.action_id.clone();
                        let ctx_clone = ctx.clone();
                        let comp_clone = comp.clone();
                        tokio::spawn(async move {
                            let outcome = match handler {
                                Some(h) => h.schedule(&action_id, at_ms).await,
                                None => ApprovalActionOutcome::Failed {
                                    message: "no action handler configured".into(),
                                },
                            };
                            followup(&ctx_clone, &comp_clone, &describe(&outcome)).await;
                            // The handler posted the scheduled notice; the
                            // actionable card is now retired (Scheduled) or
                            // was already stale (AlreadyResolved). The
                            // carousel advance happens on the handler side.
                            if should_delete_card_after_schedule(&outcome) {
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
                    Verb::SendNow | Verb::CancelSchedule | Verb::BackToQueue => {
                        // Scheduled-notice buttons (#501). Defer first —
                        // Send Now runs a full Composio send; Cancel and
                        // Back-to-queue do Gmail/store round-trips.
                        if let Err(e) = defer_ephemeral(&ctx, &comp).await {
                            warn!("failed to defer {:?}: {e}", cid.verb);
                            return;
                        }
                        let verb = cid.verb;
                        let handler = self.state.action_handler.clone();
                        let action_id = cid.action_id.clone();
                        let ctx_clone = ctx.clone();
                        let comp_clone = comp.clone();
                        tokio::spawn(async move {
                            let outcome = match handler {
                                Some(h) => match verb {
                                    Verb::SendNow => h.send_now(&action_id).await,
                                    Verb::CancelSchedule => {
                                        h.cancel_schedule(&action_id).await
                                    }
                                    _ => h.back_to_queue(&action_id).await,
                                },
                                None => ApprovalActionOutcome::Failed {
                                    message: "no action handler configured".into(),
                                },
                            };
                            followup(&ctx_clone, &comp_clone, &describe(&outcome)).await;
                            // The interaction's own message IS the notice
                            // for these verbs — remove it once the schedule
                            // left the scheduled state. Quiet best-effort:
                            // the handler usually already deleted it via the
                            // stored pointers.
                            if should_delete_notice(&outcome) {
                                delete_notice_message(
                                    &ctx_clone,
                                    comp_clone.channel_id,
                                    comp_clone.message.id,
                                    &action_id,
                                )
                                .await;
                            }
                        });
                    }
                    Verb::ReviseModal | Verb::FillAskModal | Verb::ScheduleModal => {
                        debug!("unexpected modal verb on component interaction");
                    }
                }
            }
            Interaction::Modal(modal) => {
                let Some(cid) = CustomId::parse(&modal.data.custom_id) else {
                    return;
                };
                if !matches!(
                    cid.verb,
                    Verb::ReviseModal | Verb::FillAskModal | Verb::ScheduleModal
                ) {
                    return;
                }
                if !is_authorized(self.state.allowed_user_id, modal.user.id) {
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

                // #501 — custom-time schedule modal: parse the free text,
                // then the same `handler.schedule` path as a select pick.
                // Parse failure is an ephemeral followup and the card stays.
                if matches!(cid.verb, Verb::ScheduleModal) {
                    let input = extract_feedback(&modal.data.components).unwrap_or_default();
                    let handler = self.state.action_handler.clone();
                    let action_id = cid.action_id.clone();
                    let ctx_clone = ctx.clone();
                    let modal_clone = modal.clone();
                    tokio::spawn(async move {
                        let at_ms = match crate::timeparse::parse_send_at(
                            &input,
                            chrono::Local::now(),
                        ) {
                            Ok(v) => v,
                            Err(msg) => {
                                modal_followup(&ctx_clone, &modal_clone, &msg, &action_id)
                                    .await;
                                return;
                            }
                        };
                        let outcome = match handler {
                            Some(h) => h.schedule(&action_id, at_ms).await,
                            None => ApprovalActionOutcome::Failed {
                                message: "no action handler configured".into(),
                            },
                        };
                        modal_followup(
                            &ctx_clone,
                            &modal_clone,
                            &describe(&outcome),
                            &action_id,
                        )
                        .await;
                        if should_delete_card_after_schedule(&outcome) {
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
                        // #473 — the revise flow recreates the Gmail draft
                        // with the card's stored envelope, so the reposted
                        // card must keep SHOWING it: a BCC that is applied
                        // but invisible is exactly the body/envelope mismatch
                        // the issue is about. Display-only (the captured
                        // triple and stored draftBody above stay clean);
                        // no envelope recorded → body passes through as-is.
                        let draft = append_envelope_markers(
                            draft,
                            store_for_capture.as_deref(),
                            &action_id,
                            &email.from,
                        );
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

/// Run the reasoner future while keeping a Discord typing indicator alive in
/// `channel_id`. The indicator auto-expires after ~10s, so we kick one off
/// immediately and re-broadcast every `TYPING_REFRESH_SECS` until the future
/// resolves. Indicator-broadcast failures are logged and otherwise ignored —
/// the reply path must not depend on the typing UX succeeding. (#125)
async fn run_with_typing<F, T>(http: &Http, channel_id: ChannelId, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    // Send the first ping unconditionally so the indicator appears within a
    // round-trip of the user's message, even if the reasoner finishes quickly.
    if let Err(e) = channel_id.broadcast_typing(http).await {
        debug!("initial broadcast_typing failed: {e}");
    }
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(TYPING_REFRESH_SECS));
    // Discord's indicator times out at ~10s; an early extra tick is fine but
    // we don't want to re-fire immediately on the first poll.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick so the first refresh happens after the
    // configured interval rather than at t=0.
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return out,
            _ = ticker.tick() => {
                if let Err(e) = channel_id.broadcast_typing(http).await {
                    debug!("broadcast_typing refresh failed: {e}");
                }
            }
        }
    }
}

const TYPING_REFRESH_SECS: u64 = 9;

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
        ApprovalActionOutcome::Scheduled { local, .. } => {
            format!("Scheduled — sends {local}.")
        }
        ApprovalActionOutcome::Unscheduled => {
            "Back in the queue — approval card reposted.".into()
        }
        ApprovalActionOutcome::CancelledSchedule => {
            "Schedule cancelled — draft discarded.".into()
        }
        ApprovalActionOutcome::Failed { message } => format!("Failed: {message}"),
    }
}

/// Ephemeral followup on a deferred MODAL interaction (#501). Mirror of
/// [`followup`] for the component shape.
async fn modal_followup(
    ctx: &Context,
    modal: &serenity::all::ModalInteraction,
    message: &str,
    action_id: &str,
) {
    if let Err(e) = modal
        .create_followup(
            &ctx.http,
            CreateInteractionResponseFollowup::new()
                .content(message)
                .ephemeral(true),
        )
        .await
    {
        warn!(action_id = %action_id, "schedule modal: followup failed: {e}");
    }
}

/// Should the ACTIONABLE CARD be deleted after a schedule attempt (#501)?
/// Scheduled: the notice replaces it. AlreadyResolved: stale by definition.
/// Failed (guard rejection, CAS error): keep the card so the owner can pick
/// a different time.
fn should_delete_card_after_schedule(outcome: &ApprovalActionOutcome) -> bool {
    matches!(
        outcome,
        ApprovalActionOutcome::Scheduled { .. }
            | ApprovalActionOutcome::AlreadyResolved { .. }
    )
}

/// Should the SCHEDULED NOTICE be deleted after this outcome (#501)?
/// Approved (Send Now fired) / CancelledSchedule / Unscheduled: the schedule
/// is gone either way. AlreadyResolved: only when the fresh status says the
/// schedule is genuinely dead — a Send Now double-click loser sees
/// `AlreadyResolved{"sending"}` while the winner is mid-send, and deleting
/// the notice then would remove the only surface showing a send is in
/// flight (#501 review); `"scheduled"` likewise means still live. Failed /
/// NotFound: keep it so the owner can retry or investigate — same
/// philosophy as [`should_delete_source`].
fn should_delete_notice(outcome: &ApprovalActionOutcome) -> bool {
    match outcome {
        ApprovalActionOutcome::Approved
        | ApprovalActionOutcome::CancelledSchedule
        | ApprovalActionOutcome::Unscheduled => true,
        ApprovalActionOutcome::AlreadyResolved { status } => {
            status != "scheduled" && status != "sending"
        }
        _ => false,
    }
}

/// Best-effort delete of the scheduled-notice message an interaction rode in
/// on (#501). Logged at debug, not warn: the handler usually already deleted
/// it via the stored pointers, so "Unknown Message" here is the expected
/// case, not a problem.
async fn delete_notice_message(
    ctx: &Context,
    channel_id: ChannelId,
    message_id: MessageId,
    action_id: &str,
) {
    if let Err(e) = channel_id.delete_message(&ctx.http, message_id).await {
        debug!(
            action_id = %action_id,
            "scheduled notice already gone or delete failed: {e}"
        );
    }
}

/// #473 — append the stored compose envelope to a reposted card body as the
/// same display markers the original card carried (`[to: …]`/`[cc: …]`/
/// `[bcc: …]`), plus `[subject: …]` when a Revise overrode the header
/// (#652). `[to:]` is shown only when it differs from `card_from` (the
/// card's From line), matching compose-time behavior. No store or no
/// recorded envelope (auto-triage replies, non-gmail platforms) → the body
/// passes through unchanged. Public since #501: the Back-to-queue repost in
/// the CLI reuses it so the reposted card matches the Revise repost exactly.
pub fn append_envelope_markers(
    body: String,
    store: Option<&augmentagent_store::Store>,
    action_id: &str,
    card_from: &str,
) -> String {
    let Some(env) = store.and_then(|s| s.get_action_envelope(action_id).ok().flatten()) else {
        return body;
    };
    let mut markers = String::new();
    if let Some(to) = env.to.as_deref() {
        if !to.eq_ignore_ascii_case(card_from) {
            markers.push_str(&format!("\n[to: {to}]"));
        }
    }
    if let Some(cc) = env.cc.as_deref() {
        markers.push_str(&format!("\n[cc: {cc}]"));
    }
    if let Some(bcc) = env.bcc.as_deref() {
        markers.push_str(&format!("\n[bcc: {bcc}]"));
    }
    // #652 — a Revise that changed the subject is otherwise invisible: the
    // card title still renders the inbound subject.
    if let Some(subject) = env.subject.as_deref() {
        markers.push_str(&format!("\n[subject: {subject}]"));
    }
    if markers.is_empty() {
        return body;
    }
    // #629 — insert the markers into the HUMAN part of the draft, keeping any
    // #35 needs-input marker last: `split_needs_input` (which every card
    // render runs) discards text after the marker close, so markers appended
    // blindly after it would silently vanish from the card.
    let (human, asks) = crate::split_needs_input(&body);
    let with_markers = format!("{human}\n{markers}");
    if asks.is_empty() {
        with_markers
    } else {
        let pairs: Vec<(String, String)> = asks
            .into_iter()
            .map(|a| (a.kind, a.text))
            .collect();
        crate::append_needs_input_marker(&with_markers, &pairs)
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
            // #502 — Approve on a --send-at proposal armed the schedule; the
            // actionable card is replaced by the scheduled notice.
            | ApprovalActionOutcome::Scheduled { .. }
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
/// delete our stale messages. Verb-aware since #501, because two message
/// kinds now live in the channel:
///
/// - **Actionable cards** (Approve verb family): stale once the action is
///   resolved (status ≠ pending) — unchanged behavior.
/// - **Scheduled notices** (SendNow/CancelSchedule/BackToQueue): stale once
///   the action is NOT scheduled/sending. Without this split the old sweep
///   would delete every live notice at restart, while a blanket
///   scheduled-exemption would instead immortalize leftover actionable cards.
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
        let Some((action_id, kind)) = action_id_from_message(&msg) else {
            continue;
        };
        scanned += 1;
        let stale = sweep_should_delete(
            kind,
            handler.is_resolved(&action_id).await,
            handler.is_schedule_live(&action_id).await,
        );
        if !stale {
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

/// Which of our two message kinds a bot message is (#501). Determines the
/// staleness rule the startup sweep applies to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweptKind {
    /// Actionable approval card — carries the Approve verb family.
    Card,
    /// Scheduled notice — carries SendNow/CancelSchedule/BackToQueue.
    Notice,
}

/// The sweep's delete decision, pure so the kind × status matrix is unit
/// testable: cards die when resolved; notices die when the schedule is no
/// longer live (not `scheduled`/`sending`).
fn sweep_should_delete(kind: SweptKind, is_resolved: bool, schedule_live: bool) -> bool {
    match kind {
        SweptKind::Card => is_resolved,
        SweptKind::Notice => !schedule_live,
    }
}

/// Walk a message's components looking for the first parseable `aa:`
/// button `custom_id` that identifies a message kind. Returns the
/// `(action_id, kind)` pair if found; `None` for any message that isn't one
/// of ours.
fn action_id_from_message(msg: &Message) -> Option<(String, SweptKind)> {
    for row in &msg.components {
        for component in &row.components {
            if let ActionRowComponent::Button(button) = component {
                if let ButtonKind::NonLink { custom_id, .. } = &button.data {
                    if let Some(classified) = classify_custom_id(custom_id) {
                        return Some(classified);
                    }
                }
            }
        }
    }
    None
}

/// Classify one `aa:` custom_id into the message kind it implies (#501).
/// `None` for unparseable ids and for verbs that never ride on a message
/// button (select/modal verbs), so a stray one can't misclassify a message.
fn classify_custom_id(raw: &str) -> Option<(String, SweptKind)> {
    let parsed = crate::custom_id::CustomId::parse(raw)?;
    let kind = match parsed.verb {
        Verb::Approve | Verb::Revise | Verb::Skip | Verb::FillAsk => SweptKind::Card,
        Verb::SendNow | Verb::CancelSchedule | Verb::BackToQueue => SweptKind::Notice,
        Verb::ReviseModal
        | Verb::FillAskModal
        | Verb::QuickRefine
        | Verb::SchedulePick
        | Verb::ScheduleModal => return None,
    };
    Some((parsed.action_id, kind))
}

/// Soft cap on how much of a text file we feed into the prompt. Files larger
/// than this are downloaded up to `MAX_DOWNLOAD_BYTES` and truncated to the
/// first `MAX_TEXT_BYTES` bytes — the prompt annotation marks the file as
/// `TRUNCATED — first X of Y` so the reasoner knows it has the head only.
const MAX_TEXT_BYTES: u32 = 1_048_576; // 1 MB (matches serenity's Attachment.size u32)

/// Hard cap on text-file attachment size. Files larger than this are dropped
/// at filter time so we never spend bandwidth downloading them. Sized to leave
/// headroom above `MAX_TEXT_BYTES` — files in `MAX_TEXT_BYTES..MAX_DOWNLOAD_BYTES`
/// are accepted and truncated at write time.
const MAX_DOWNLOAD_BYTES: u32 = 8 * 1_048_576; // 8 MB

/// Extensions we accept as text even when Discord omits `content_type`.
/// Discord populates `content_type` from the upload, which is unreliable for
/// code/config files, so we fall back to extension here.
const TEXT_EXT_ALLOWLIST: &[&str] = &[
    // plain text & docs
    "txt", "md", "markdown", "rst", "log",
    // structured data
    "json", "yaml", "yml", "toml", "csv", "tsv", "xml",
    // source code
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs",
    "py", "go", "java", "c", "cc", "cpp", "h", "hpp",
    "cs", "rb", "php", "swift", "kt", "scala",
    "sh", "bash", "zsh", "sql",
    "html", "css", "scss", "less",
    // config
    "ini", "conf", "cfg", "properties",
];

/// Extensions we refuse to ingest as text even if the MIME type matches.
/// These are formats that commonly contain credentials.
const TEXT_EXT_DENYLIST: &[&str] = &["env", "pem", "key", "p12", "pfx"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocKind {
    Pdf,
    Docx,
    Doc,
}

/// Identify the doc kind from an attachment's content_type and/or filename
/// extension. Returns `None` for everything else — non-docs flow through the
/// regular text-file / image / rejected branches.
fn doc_kind_for(att: &Attachment) -> Option<DocKind> {
    let ext = std::path::Path::new(&att.filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase());
    let ct = att.content_type.as_deref();
    match (ct, ext.as_deref()) {
        (Some("application/pdf"), _) | (_, Some("pdf")) => Some(DocKind::Pdf),
        (
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            _,
        )
        | (_, Some("docx")) => Some(DocKind::Docx),
        (Some("application/msword"), _) | (_, Some("doc")) => Some(DocKind::Doc),
        _ => None,
    }
}

/// Pure helper that picks the converter binary + args for a doc kind.
/// Extracted so the dispatch is unit-testable without the binaries installed.
fn doc_command_for(kind: DocKind, in_path: &std::path::Path) -> (&'static str, Vec<String>) {
    let in_arg = in_path.to_string_lossy().into_owned();
    match kind {
        // `-` writes to stdout; `-layout` preserves columns/whitespace better
        // for log-like dumps.
        DocKind::Pdf => ("pdftotext", vec!["-layout".into(), in_arg, "-".into()]),
        // pandoc handles both .docx and legacy .doc.
        DocKind::Docx | DocKind::Doc => ("pandoc", vec!["--to=plain".into(), in_arg]),
    }
}

/// Shell out to the appropriate converter and return the extracted text.
/// Errors include: binary missing, non-zero exit, invalid UTF-8 in stdout.
async fn convert_doc_to_text(kind: DocKind, in_path: &std::path::Path) -> anyhow::Result<String> {
    let (program, args) = doc_command_for(kind, in_path);
    let output = tokio::process::Command::new(program)
        .args(&args)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Mirror of the image filter for text files. Accepts attachments whose
/// `content_type` starts with `text/`, or whose extension is in
/// `TEXT_EXT_ALLOWLIST`. Drops anything in `TEXT_EXT_DENYLIST` or larger than
/// `MAX_DOWNLOAD_BYTES`, regardless of MIME. Files between `MAX_TEXT_BYTES`
/// and `MAX_DOWNLOAD_BYTES` are accepted here and truncated at download time.
fn filter_text_attachments(attachments: &[Attachment]) -> Vec<Attachment> {
    attachments
        .iter()
        .filter(|a| {
            if a.size > MAX_DOWNLOAD_BYTES {
                return false;
            }
            let ext = std::path::Path::new(&a.filename)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase());
            if let Some(e) = ext.as_deref() {
                if TEXT_EXT_DENYLIST.contains(&e) {
                    return false;
                }
            }
            if a.content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("text/"))
            {
                return true;
            }
            if let Some(e) = ext.as_deref() {
                if TEXT_EXT_ALLOWLIST.contains(&e) {
                    return true;
                }
            }
            false
        })
        .cloned()
        .collect()
}

/// Why an attachment was dropped. Rendered into a footer on the bot's reply
/// so the user knows we saw it but couldn't ingest it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RejectReason {
    /// Above `MAX_DOWNLOAD_BYTES`. Carries the actual size in bytes for the
    /// footer (e.g. "9.0 MB > 8.0 MB"). Files between `MAX_TEXT_BYTES` and
    /// `MAX_DOWNLOAD_BYTES` are truncated rather than rejected.
    Oversize { size: u32 },
    /// Extension is in `TEXT_EXT_DENYLIST` — formats that commonly hold
    /// credentials (.env, .pem, etc.). Refused even with matching MIME.
    SecurityDenylist,
    /// Not an image, not text by MIME, not in the text extension allowlist.
    UnsupportedType {
        content_type: Option<String>,
        ext: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct RejectedAttachment {
    filename: String,
    reason: RejectReason,
}

#[derive(Debug, Default)]
struct AttachmentPartition {
    images: Vec<Attachment>,
    text_files: Vec<Attachment>,
    /// PDFs / DOCXs / DOCs routed through `extract_doc_attachments` instead
    /// of the regular text-file download path. Their extracted text is fed
    /// to the prompt as if it were a regular text attachment.
    docs: Vec<Attachment>,
    rejected: Vec<RejectedAttachment>,
}

/// Classify each attachment into images / text-files / docs / rejected in one
/// pass. Replaces the per-category filter calls so a `.zip` (which no filter
/// accepts) shows up exactly once in `rejected` rather than being silently
/// dropped by each filter.
///
/// Images are unconditionally accepted (no size/extension gate; matches the
/// pre-existing image-attachment behavior). The text-file gating logic
/// mirrors `filter_text_attachments` — kept in sync deliberately rather than
/// delegated, because the rejection reasons need to be produced inline.
fn partition_attachments(attachments: &[Attachment]) -> AttachmentPartition {
    let mut out = AttachmentPartition::default();
    for a in attachments {
        if a
            .content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("image/"))
        {
            out.images.push(a.clone());
            continue;
        }
        let ext = std::path::Path::new(&a.filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase());
        if let Some(e) = ext.as_deref() {
            if TEXT_EXT_DENYLIST.contains(&e) {
                out.rejected.push(RejectedAttachment {
                    filename: a.filename.clone(),
                    reason: RejectReason::SecurityDenylist,
                });
                continue;
            }
        }
        if a.size > MAX_DOWNLOAD_BYTES {
            out.rejected.push(RejectedAttachment {
                filename: a.filename.clone(),
                reason: RejectReason::Oversize { size: a.size },
            });
            continue;
        }
        // Doc formats (PDF / DOCX / DOC) get routed through the converter
        // pipeline before joining text_files for the prompt.
        if doc_kind_for(a).is_some() {
            out.docs.push(a.clone());
            continue;
        }
        let is_text_mime = a
            .content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/"));
        let is_allowlisted_ext = ext
            .as_deref()
            .is_some_and(|e| TEXT_EXT_ALLOWLIST.contains(&e));
        if is_text_mime || is_allowlisted_ext {
            out.text_files.push(a.clone());
        } else {
            out.rejected.push(RejectedAttachment {
                filename: a.filename.clone(),
                reason: RejectReason::UnsupportedType {
                    content_type: a.content_type.as_deref().map(|s| s.to_string()),
                    ext,
                },
            });
        }
    }
    out
}

fn format_size(bytes: u32) -> String {
    const MB: f64 = 1_048_576.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Render the rejection footer. Returns `None` if there's nothing to surface,
/// so callers can use `if let Some(footer) = format_rejection_footer(...)`.
fn format_rejection_footer(rejected: &[RejectedAttachment]) -> Option<String> {
    if rejected.is_empty() {
        return None;
    }
    let parts: Vec<String> = rejected
        .iter()
        .map(|r| match &r.reason {
            RejectReason::Oversize { size } => format!(
                "{} ({} > {})",
                r.filename,
                format_size(*size),
                format_size(MAX_DOWNLOAD_BYTES),
            ),
            RejectReason::SecurityDenylist => format!("{} (security)", r.filename),
            RejectReason::UnsupportedType { content_type, ext } => {
                let detail = content_type
                    .clone()
                    .or_else(|| ext.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                format!("{} (unsupported: {})", r.filename, detail)
            }
        })
        .collect();
    Some(format!("\u{26A0}\u{FE0F} skipped: {}", parts.join(", ")))
}

/// Label used to annotate prior turns in conversation history when the user
/// previously sent attachments. Chosen from the present attachment kinds so the
/// model has a coarse hint about what was in the earlier turn.
fn attachment_kind_label(attachments: &[Attachment]) -> &'static str {
    let has_img = attachments.iter().any(|a| {
        a.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("image/"))
    });
    let has_txt = !filter_text_attachments(attachments).is_empty();
    match (has_img, has_txt) {
        (true, true) => "[image + text attachment]",
        (true, false) => "[image attachment]",
        (false, true) => "[text attachment]",
        _ => "[attachment]",
    }
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
        // text/* fallback for the common cases. Only kicks in when the
        // filename has no extension — otherwise the early-return above wins.
        match ct {
            "text/plain" => return "txt".into(),
            "text/markdown" => return "md".into(),
            "text/csv" => return "csv".into(),
            _ => {}
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

/// A text-file attachment that was downloaded to disk for the reasoner.
/// `truncated` is true when the file was larger than `MAX_TEXT_BYTES` and we
/// wrote only the first MB; `original_size` is what Discord reported on the
/// uploaded attachment.
#[derive(Debug, Clone)]
struct DownloadedTextFile {
    path: PathBuf,
    truncated: bool,
    original_size: u32,
}

/// Pure slicing helper for the truncation logic. Extracted so the cap behavior
/// is unit-testable without standing up a fake HTTP server.
fn truncate_text_bytes(bytes: &[u8]) -> (&[u8], bool) {
    let cap = MAX_TEXT_BYTES as usize;
    if bytes.len() > cap {
        (&bytes[..cap], true)
    } else {
        (bytes, false)
    }
}

/// Mirror of `download_images` for text-file attachments. Writes to
/// `/tmp/aa-txt-<msg_id>-<idx>.<ext>`. Partial success is fine — failures are
/// logged and skipped, same as the image path.
///
/// Files larger than `MAX_TEXT_BYTES` are *truncated* (we still write the
/// first MB) so the reasoner can read the head of a large log/config rather
/// than the user getting nothing back. The hard cap is `MAX_DOWNLOAD_BYTES`,
/// enforced at filter time.
async fn download_text_files(
    attachments: &[Attachment],
    msg_id: u64,
) -> Vec<DownloadedTextFile> {
    let mut out = Vec::with_capacity(attachments.len());
    for (idx, att) in attachments.iter().enumerate() {
        let ext = extension_for(att);
        let path = PathBuf::from(format!("/tmp/aa-txt-{msg_id}-{idx}.{ext}"));
        match reqwest::get(&att.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => {
                    let (to_write, truncated) = truncate_text_bytes(&bytes);
                    match tokio::fs::write(&path, to_write).await {
                        Ok(()) => out.push(DownloadedTextFile {
                            path,
                            truncated,
                            original_size: att.size,
                        }),
                        Err(e) => warn!("write text tempfile {} failed: {e}", path.display()),
                    }
                }
                Err(e) => warn!("read text bytes from {} failed: {e}", att.url),
            },
            Err(e) => warn!("download text file {} failed: {e}", att.url),
        }
    }
    out
}

/// Download PDF / DOCX / DOC attachments, shell out to the appropriate
/// converter (pdftotext / pandoc), truncate the extracted text to
/// `MAX_TEXT_BYTES`, and write the result to `/tmp/aa-doc-<msg_id>-<idx>.txt`.
///
/// Returns `DownloadedTextFile`s so the converted output flows through the
/// same prompt-builder annotation as a regular text attachment. The original
/// binary tempfile is removed after conversion regardless of outcome.
///
/// Partial success is fine — failures (binary missing, conversion error,
/// non-UTF-8 stdout) are logged and the attachment is skipped, matching the
/// `download_*` pattern elsewhere in this file.
async fn extract_doc_attachments(
    attachments: &[Attachment],
    msg_id: u64,
) -> Vec<DownloadedTextFile> {
    let mut out = Vec::with_capacity(attachments.len());
    for (idx, att) in attachments.iter().enumerate() {
        let Some(kind) = doc_kind_for(att) else {
            continue;
        };
        let in_ext = extension_for(att);
        let in_path = PathBuf::from(format!("/tmp/aa-doc-{msg_id}-{idx}.{in_ext}"));
        let out_path = PathBuf::from(format!("/tmp/aa-doc-{msg_id}-{idx}.txt"));

        // Download binary.
        let download_ok = match reqwest::get(&att.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => match tokio::fs::write(&in_path, &bytes).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("write doc tempfile {} failed: {e}", in_path.display());
                        false
                    }
                },
                Err(e) => {
                    warn!("read doc bytes from {} failed: {e}", att.url);
                    false
                }
            },
            Err(e) => {
                warn!("download doc {} failed: {e}", att.url);
                false
            }
        };

        if download_ok {
            match convert_doc_to_text(kind, &in_path).await {
                Ok(text) => {
                    let (to_write, truncated) = truncate_text_bytes(text.as_bytes());
                    let extracted_len = text.len().min(u32::MAX as usize) as u32;
                    match tokio::fs::write(&out_path, to_write).await {
                        Ok(()) => out.push(DownloadedTextFile {
                            path: out_path,
                            truncated,
                            original_size: extracted_len,
                        }),
                        Err(e) => {
                            warn!("write doc text tempfile {} failed: {e}", out_path.display())
                        }
                    }
                }
                Err(e) => warn!(
                    "convert {} ({:?}) failed: {e:#}",
                    att.filename, kind
                ),
            }
        }

        // Best-effort: clean up the binary tempfile regardless of conversion
        // outcome. The .txt tempfile is cleaned up by the handler.
        if let Err(e) = tokio::fs::remove_file(&in_path).await {
            // Missing-file is benign (download may have failed before write).
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("remove doc tempfile {} failed: {e}", in_path.display());
            }
        }
    }
    out
}

/// Combine the user's text (possibly empty) with lists of locally-downloaded
/// image and text-file paths, instructing Claude to use the Read tool to
/// inspect them. Truncated text files get a marker so the reasoner knows it
/// is reading the head of the file only.
fn build_prompt(
    user_text: &str,
    images: &[PathBuf],
    text_files: &[DownloadedTextFile],
) -> String {
    if images.is_empty() && text_files.is_empty() {
        return user_text.to_string();
    }
    let mut s = String::new();
    if !user_text.is_empty() {
        s.push_str(user_text);
        s.push_str("\n\n");
    }
    if !images.is_empty() {
        // `IMAGE:` marker lines are the cross-provider convention defined in
        // `augmentagent_channel_core::images`: claude Reads the path directly
        // (scope-guard carve-out for /tmp/aa-img-*), a codex failover turns
        // each marker into a native `-i` attachment, and text-only providers
        // replace them with an honest note. The prefix is MIRRORED here as a
        // literal — this crate must stay free of a channel-core dependency
        // (channel-core depends on us), same pattern as SOCIALAPI_API_KEY_ENV.
        s.push_str("[attached images to analyze — open each IMAGE path]\n");
        for path in images {
            s.push_str(&format!("IMAGE: {}\n", path.display()));
        }
    }
    if !text_files.is_empty() {
        s.push_str("[attached text files to read]\n");
        for f in text_files {
            s.push_str("- ");
            s.push_str(&f.path.display().to_string());
            if f.truncated {
                s.push_str(&format!(
                    "  (TRUNCATED — first {} of {})",
                    format_size(MAX_TEXT_BYTES),
                    format_size(f.original_size),
                ));
            }
            s.push('\n');
        }
    }
    s.push_str("\nUse the Read tool to view each attachment and answer based on them.");
    s
}

/// Layer a pre-formatted `<conversation_history>` block in front of the
/// current-turn prompt. If `history` is empty, falls through to the bare
/// `build_prompt` so first-turn messages match prior behavior exactly.
fn build_prompt_with_context(
    history: &str,
    user_text: &str,
    images: &[PathBuf],
    text_files: &[DownloadedTextFile],
) -> String {
    let current = build_prompt(user_text, images, text_files);
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
        } else if is_authorized(allowed_user_id, m.author.id) {
            "user"
        } else {
            // Fail-closed (#303): with no configured allowlist, or for any
            // non-owner author, don't fold their messages into the owner's
            // conversation context.
            continue;
        };
        let mut body = m.content.trim().to_string();
        if !m.attachments.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(attachment_kind_label(&m.attachments));
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
/// Post a multi-chunk reply to `channel_id` with reply-chaining and retry
/// (issue #126).
///
/// - Chunk 1 references the original user message (`root_msg`).
/// - Chunks 2..N reference the *previous chunk's* message id, not the root.
///   Discord sometimes rejects rapid-fire reply-to-same-parent sends, and
///   chaining sidesteps that failure mode while still keeping the visual
///   thread together.
/// - Each chunk send retries once on failure with 250ms then 750ms backoff
///   before giving up. The second retry drops the reply reference entirely,
///   in case the reference itself (deleted parent / Discord validation) is
///   what failed.
/// - If a chunk still can't be delivered after both retries, we post a
///   fallback `couldn't deliver remaining N message(s)` notice so the user
///   isn't left thinking the answer ended early. We keep trying subsequent
///   chunks anyway — a single bad chunk shouldn't truncate the whole reply.
/// - 200ms `tokio::time::sleep` between chunks to stay under Discord's
///   per-channel send rate.
async fn send_chunks_reply_chain(
    http: &Http,
    channel_id: ChannelId,
    root_msg: MessageId,
    chunks: Vec<String>,
    attachments: Vec<CreateAttachment>,
    label: &str,
) {
    let total = chunks.len();
    let mut prev_msg_id: Option<MessageId> = None;
    let mut failed_count: usize = 0;

    for (idx, chunk) in chunks.into_iter().enumerate() {
        // 200ms inter-chunk delay (skip before the very first chunk).
        if idx > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let reply_target = if idx == 0 {
            Some(root_msg)
        } else {
            prev_msg_id
        };

        // #440 — outbound files ride on the first chunk only.
        let chunk_files: &[CreateAttachment] = if idx == 0 { &attachments } else { &[] };

        match send_one_with_retry(http, channel_id, reply_target, &chunk, chunk_files, label).await
        {
            Some(sent_id) => {
                prev_msg_id = Some(sent_id);
            }
            None => {
                failed_count += 1;
                // Don't stop the loop — keep trying remaining chunks so a
                // single transient failure doesn't truncate everything.
            }
        }
    }

    if failed_count > 0 {
        let notice = format!(
            "\u{26a0}\u{fe0f} couldn't deliver {failed_count} of {total} message(s) — check logs"
        );
        let builder = CreateMessage::new()
            .content(notice)
            .reference_message(MessageReference::from((channel_id, root_msg)));
        if let Err(e) = channel_id.send_message(http, builder).await {
            warn!("failed to post {label} truncation notice: {e}");
        }
    }
}

/// Send a single chunk with one retry. Returns the sent message id on
/// success, or `None` if both attempts failed.
///
/// Backoff schedule: initial send → 250ms → retry → 750ms → final retry
/// without the reply reference (defensive: in case the parent message
/// reference itself is what Discord rejected).
async fn send_one_with_retry(
    http: &Http,
    channel_id: ChannelId,
    reply_target: Option<MessageId>,
    chunk: &str,
    attachments: &[CreateAttachment],
    label: &str,
) -> Option<MessageId> {
    let attempts: [(u64, bool); 3] = [(0, true), (250, true), (750, false)];
    let mut last_err: Option<String> = None;
    for (delay_ms, use_reference) in attempts {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let mut builder = CreateMessage::new().content(chunk);
        if !attachments.is_empty() {
            // Rebuilt per attempt — CreateAttachment is Clone and the builder
            // consumes its files on send.
            builder = builder.add_files(attachments.iter().cloned());
        }
        if use_reference {
            if let Some(target) = reply_target {
                builder = builder.reference_message(MessageReference::from((channel_id, target)));
            }
        }
        match channel_id.send_message(http, builder).await {
            Ok(sent) => return Some(sent.id),
            Err(e) => {
                last_err = Some(e.to_string());
                warn!("failed to post {label} (delay={delay_ms}ms, ref={use_reference}): {e}");
            }
        }
    }
    if let Some(err) = last_err {
        warn!("giving up on {label} after retries: {err}");
    }
    None
}

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

    // ---- #303: fail-closed owner-allowlist check ----

    #[test]
    fn is_authorized_fails_closed_without_allowlist() {
        let owner = UserId::new(111);
        let other = UserId::new(222);
        // No allowlist configured -> everyone is refused (fail-closed).
        assert!(!is_authorized(None, owner));
        assert!(!is_authorized(None, other));
        // Allowlist configured -> only the configured owner passes.
        assert!(is_authorized(Some(owner), owner));
        assert!(!is_authorized(Some(owner), other));
    }

    // ---- #473: envelope markers on the reposted (post-Revise) card ----

    fn store_with_envelope(
        to: Option<&str>,
        cc: Option<&str>,
        bcc: Option<&str>,
    ) -> (augmentagent_store::Store, String, tempfile::NamedTempFile) {
        let f = tempfile::NamedTempFile::new().unwrap();
        let s = augmentagent_store::Store::open(f.path()).unwrap();
        let id = s
            .log_action(
                "m-473",
                None,
                "josh@x.com",
                "intro",
                None,
                Some("draft"),
                augmentagent_store::ActionStatus::Pending,
            )
            .unwrap();
        s.set_action_envelope(&id, to, cc, bcc).unwrap();
        (s, id, f)
    }

    #[test]
    fn envelope_markers_show_overridden_to_and_bcc() {
        let (s, id, _f) =
            store_with_envelope(Some("omer@y.com"), None, Some("josh@x.com"));
        let out = append_envelope_markers("body".into(), Some(&s), &id, "josh@x.com");
        assert!(out.contains("[to: omer@y.com]"), "missing to marker: {out}");
        assert!(out.contains("[bcc: josh@x.com]"), "missing bcc marker: {out}");
        assert!(out.starts_with("body"), "body must lead: {out}");
    }

    #[test]
    fn envelope_markers_show_a_revised_subject() {
        // #652 — the reposted card is the only place the operator can see
        // that Revise actually applied the subject they asked for.
        let (s, id, _f) = store_with_envelope(None, None, None);
        s.set_action_subject(&id, Some("Invoice for July")).unwrap();
        let out = append_envelope_markers("body".into(), Some(&s), &id, "alice@example.com");
        assert!(
            out.contains("[subject: Invoice for July]"),
            "missing subject marker: {out}"
        );

        // Untouched subject → no marker, even with an envelope recorded.
        let (s, id, _f) = store_with_envelope(None, Some("cc@example.com"), None);
        let out = append_envelope_markers("body".into(), Some(&s), &id, "alice@example.com");
        assert!(!out.contains("[subject:"), "spurious subject marker: {out}");
    }

    #[test]
    fn envelope_markers_omit_to_when_it_matches_card_from() {
        // New-email cards: the card's From line already IS the recipient
        // list, so a [to:] marker would be redundant noise.
        let (s, id, _f) =
            store_with_envelope(Some("a@b.com"), Some("cc@d.com"), None);
        let out = append_envelope_markers("body".into(), Some(&s), &id, "a@b.com");
        assert!(!out.contains("[to:"), "redundant to marker: {out}");
        assert!(out.contains("[cc: cc@d.com]"), "missing cc marker: {out}");
    }

    #[test]
    fn envelope_markers_stay_visible_on_needs_input_drafts() {
        // #629 — a needs-input draft carries a trailing #35 marker, and
        // split_needs_input (run by every card render) discards text after
        // it. The cc marker must land in the human part or it never renders.
        let (s, id, _f) =
            store_with_envelope(Some("a@example.com"), Some("cc@example.com"), None);
        let body = crate::append_needs_input_marker(
            "body",
            &[("scheduling".to_string(), "what time works?".to_string())],
        );
        let out = append_envelope_markers(body, Some(&s), &id, "a@example.com");
        let (human, asks) = crate::split_needs_input(&out);
        assert!(
            human.contains("[cc: cc@example.com]"),
            "cc marker lost from rendered card: {out}"
        );
        assert_eq!(asks.len(), 1, "needs-input ask lost: {out}");
        assert_eq!(asks[0].text, "what time works?");
    }

    #[test]
    fn assumes_fence_survives_a_card_re_render_with_every_other_marker() {
        // #785 — a re-rendered card can carry all three carriers at once. The
        // assumes fence is spliced (not truncated) on render, so the envelope
        // markers appended after it must still reach the card.
        let (s, id, _f) =
            store_with_envelope(Some("a@example.com"), Some("cc@example.com"), None);
        let body = crate::append_needs_input_marker(
            &crate::append_assumes_marker("body", &["you're free on the 14th".to_string()]),
            &[("scheduling".to_string(), "what time works?".to_string())],
        );
        let out = append_envelope_markers(body, Some(&s), &id, "a@example.com");
        let (human, asks) = crate::split_needs_input(&out);
        let (human, facts) = crate::split_assumes(&human);
        assert_eq!(asks.len(), 1, "needs-input ask lost: {out}");
        assert_eq!(facts, vec!["you're free on the 14th".to_string()]);
        assert!(
            human.contains("[cc: cc@example.com]"),
            "cc marker lost from rendered card: {out}"
        );
        assert!(human.starts_with("body"), "draft text mangled: {human}");
    }

    #[test]
    fn no_envelope_or_no_store_passes_body_through() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let s = augmentagent_store::Store::open(f.path()).unwrap();
        let id = s
            .log_action(
                "m-none",
                None,
                "a@b.com",
                "s",
                None,
                Some("d"),
                augmentagent_store::ActionStatus::Pending,
            )
            .unwrap();
        // Row exists but no envelope was ever recorded (auto-triage shape).
        assert_eq!(
            append_envelope_markers("body".into(), Some(&s), &id, "a@b.com"),
            "body"
        );
        // No store wired at all.
        assert_eq!(append_envelope_markers("body".into(), None, &id, "a@b.com"), "body");
    }

    // ---- #501: verb-aware startup sweep ----

    #[test]
    fn classify_custom_id_maps_verb_families() {
        // Approve verb family → actionable card.
        for v in ["approve", "revise", "skip", "fill_ask"] {
            let (id, kind) = classify_custom_id(&format!("aa:a1:{v}")).unwrap();
            assert_eq!(id, "a1");
            assert_eq!(kind, SweptKind::Card, "{v} must classify as Card");
        }
        // Notice verb family → scheduled notice.
        for v in ["send_now", "cancel_schedule", "back_to_queue"] {
            let (id, kind) = classify_custom_id(&format!("aa:a2:{v}")).unwrap();
            assert_eq!(id, "a2");
            assert_eq!(kind, SweptKind::Notice, "{v} must classify as Notice");
        }
        // Select/modal verbs never identify a message kind.
        for v in [
            "quick_refine",
            "schedule_pick",
            "schedule_modal",
            "revise_modal",
            "fill_ask_modal",
        ] {
            assert!(classify_custom_id(&format!("aa:a3:{v}")).is_none());
        }
        // Foreign ids don't classify.
        assert!(classify_custom_id("other:a4:approve").is_none());
    }

    #[test]
    fn sweep_matrix_cards_die_when_resolved_notices_when_schedule_dead() {
        // Actionable card: only the resolved flag matters.
        assert!(!sweep_should_delete(SweptKind::Card, false, false));
        assert!(!sweep_should_delete(SweptKind::Card, false, true));
        assert!(sweep_should_delete(SweptKind::Card, true, false));
        assert!(sweep_should_delete(SweptKind::Card, true, true));
        // Scheduled notice: only schedule liveness matters — a notice for a
        // live scheduled/sending row survives restart, everything else dies
        // (including pending: a back-to-queue'd row's notice is stale).
        assert!(sweep_should_delete(SweptKind::Notice, false, false));
        assert!(sweep_should_delete(SweptKind::Notice, true, false));
        assert!(!sweep_should_delete(SweptKind::Notice, false, true));
        assert!(!sweep_should_delete(SweptKind::Notice, true, true));
    }

    // ---- #501: outcome → message-cleanup decisions ----

    #[test]
    fn notice_deletion_covers_terminal_outcomes_only() {
        assert!(should_delete_notice(&ApprovalActionOutcome::Approved));
        assert!(should_delete_notice(&ApprovalActionOutcome::CancelledSchedule));
        assert!(should_delete_notice(&ApprovalActionOutcome::Unscheduled));
        assert!(should_delete_notice(&ApprovalActionOutcome::AlreadyResolved {
            status: "sent".into()
        }));
        // A double-click loser while the schedule is still live (armed, or
        // the winner mid-send) must NOT take the notice down (#501 review).
        assert!(!should_delete_notice(&ApprovalActionOutcome::AlreadyResolved {
            status: "sending".into()
        }));
        assert!(!should_delete_notice(&ApprovalActionOutcome::AlreadyResolved {
            status: "scheduled".into()
        }));
        assert!(!should_delete_notice(&ApprovalActionOutcome::Failed {
            message: "x".into()
        }));
        assert!(!should_delete_notice(&ApprovalActionOutcome::NotFound));
    }

    #[test]
    fn card_deletion_after_schedule_keeps_failed_cards() {
        assert!(should_delete_card_after_schedule(
            &ApprovalActionOutcome::Scheduled {
                at_ms: 1,
                local: "t".into()
            }
        ));
        assert!(should_delete_card_after_schedule(
            &ApprovalActionOutcome::AlreadyResolved {
                status: "scheduled".into()
            }
        ));
        // A guard rejection must leave the card so the owner picks again.
        assert!(!should_delete_card_after_schedule(
            &ApprovalActionOutcome::Failed {
                message: "too soon".into()
            }
        ));
    }

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
        let got = build_prompt_with_context("", "hello", &[], &[]);
        assert_eq!(got, "hello");
    }

    #[test]
    fn build_prompt_layers_history_before_current_message() {
        let history = "<conversation_history>\nuser: hi\nassistant: hello\n</conversation_history>";
        let got = build_prompt_with_context(history, "what's next?", &[], &[]);
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
    fn extension_prefers_filename_then_mime() {
        assert_eq!(extension_for(&att("photo.JPG", Some("image/jpeg"))), "jpg");
        assert_eq!(extension_for(&att("noext", Some("image/png"))), "png");
        assert_eq!(extension_for(&att("noext", Some("image/jpeg"))), "jpg");
        assert_eq!(extension_for(&att("noext", None)), "bin");
    }

    #[test]
    fn extension_for_text_mime_falls_back_to_subtype_mapping() {
        // No filename extension → derive from text/* MIME.
        assert_eq!(extension_for(&att("noext", Some("text/plain"))), "txt");
        assert_eq!(extension_for(&att("noext", Some("text/markdown"))), "md");
        assert_eq!(extension_for(&att("noext", Some("text/csv"))), "csv");
        // Unmapped text MIME falls through to bin (the reasoner reads the
        // file regardless of extension, so this only affects tempfile naming).
        assert_eq!(extension_for(&att("noext", Some("text/x-rust"))), "bin");
    }

    #[test]
    fn extension_for_prefers_filename_extension_over_text_mime() {
        // Filename extension always wins, even when content_type says text/*.
        assert_eq!(extension_for(&att("foo.rs", Some("text/plain"))), "rs");
        assert_eq!(extension_for(&att("README.md", Some("text/plain"))), "md");
    }

    #[test]
    fn build_prompt_with_only_images_does_not_panic_on_empty_text() {
        let images = vec![PathBuf::from("/tmp/aa-img-42-0.png")];
        let prompt = build_prompt("", &images, &[]);
        assert!(prompt.contains("[attached images to analyze"));
        // Images are referenced as cross-provider `IMAGE:` marker lines
        // (mirrors augmentagent_channel_core::images) so a codex failover
        // can translate them into native `-i` attachments.
        assert!(prompt.contains("IMAGE: /tmp/aa-img-42-0.png"));
        assert!(prompt.contains("Use the Read tool"));
    }

    #[test]
    fn build_prompt_combines_text_and_images() {
        let images = vec![
            PathBuf::from("/tmp/aa-img-7-0.png"),
            PathBuf::from("/tmp/aa-img-7-1.jpg"),
        ];
        let prompt = build_prompt("what's in this?", &images, &[]);
        assert!(prompt.starts_with("what's in this?"));
        assert!(prompt.contains("/tmp/aa-img-7-0.png"));
        assert!(prompt.contains("/tmp/aa-img-7-1.jpg"));
    }

    #[test]
    fn build_prompt_without_images_returns_plain_text() {
        let prompt = build_prompt("hello", &[], &[]);
        assert_eq!(prompt, "hello");
    }

    fn fresh_txt(path: &str) -> DownloadedTextFile {
        DownloadedTextFile {
            path: PathBuf::from(path),
            truncated: false,
            original_size: 1,
        }
    }

    #[test]
    fn build_prompt_includes_text_files() {
        let txts = vec![fresh_txt("/tmp/aa-txt-9-0.md"), fresh_txt("/tmp/aa-txt-9-1.json")];
        let prompt = build_prompt("summarize", &[], &txts);
        assert!(prompt.starts_with("summarize"));
        assert!(prompt.contains("[attached text files to read]"));
        assert!(prompt.contains("/tmp/aa-txt-9-0.md"));
        assert!(prompt.contains("/tmp/aa-txt-9-1.json"));
        assert!(prompt.contains("Use the Read tool"));
        // Non-truncated files do NOT get the marker.
        assert!(!prompt.contains("TRUNCATED"));
    }

    #[test]
    fn build_prompt_combines_images_and_text_files() {
        let images = vec![PathBuf::from("/tmp/aa-img-1-0.png")];
        let txts = vec![fresh_txt("/tmp/aa-txt-1-0.md")];
        let prompt = build_prompt("what do these say?", &images, &txts);
        let img_idx = prompt.find("[attached images to analyze").expect("image block");
        let txt_idx = prompt.find("[attached text files to read]").expect("text block");
        assert!(img_idx < txt_idx);
        assert!(prompt.contains("/tmp/aa-img-1-0.png"));
        assert!(prompt.contains("/tmp/aa-txt-1-0.md"));
    }

    #[test]
    fn build_prompt_annotates_truncated_files() {
        let txts = vec![
            DownloadedTextFile {
                path: PathBuf::from("/tmp/aa-txt-3-0.log"),
                truncated: true,
                original_size: 4 * 1_048_576 + 700_000, // ~4.7 MB
            },
            fresh_txt("/tmp/aa-txt-3-1.md"),
        ];
        let prompt = build_prompt("anything weird in the log?", &[], &txts);
        // Truncated file gets the marker.
        assert!(prompt.contains("/tmp/aa-txt-3-0.log"));
        assert!(prompt.contains("TRUNCATED — first 1.0 MB of 4.7 MB"));
        // Non-truncated sibling does not.
        let md_line = prompt
            .lines()
            .find(|l| l.contains("aa-txt-3-1.md"))
            .expect("md line");
        assert!(!md_line.contains("TRUNCATED"));
    }

    #[test]
    fn truncate_returns_full_buffer_when_under_cap() {
        let bytes = vec![0u8; 500_000];
        let (slice, truncated) = truncate_text_bytes(&bytes);
        assert_eq!(slice.len(), 500_000);
        assert!(!truncated);
    }

    #[test]
    fn truncate_clips_to_cap_when_over() {
        let bytes = vec![0u8; (MAX_TEXT_BYTES + 1) as usize];
        let (slice, truncated) = truncate_text_bytes(&bytes);
        assert_eq!(slice.len(), MAX_TEXT_BYTES as usize);
        assert!(truncated);
    }

    #[test]
    fn partition_accepts_up_to_download_cap_and_rejects_beyond() {
        let just_under = att_sized("a.log", Some("text/plain"), MAX_DOWNLOAD_BYTES);
        let just_over = att_sized("b.log", Some("text/plain"), MAX_DOWNLOAD_BYTES + 1);
        let between = att_sized("c.log", Some("text/plain"), MAX_TEXT_BYTES + 1);
        let p = partition_attachments(&[just_under, just_over, between]);
        // Files in MAX_TEXT_BYTES..=MAX_DOWNLOAD_BYTES are accepted (will be truncated at download).
        let accepted_names: Vec<&str> =
            p.text_files.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(accepted_names, vec!["a.log", "c.log"]);
        // Only files > MAX_DOWNLOAD_BYTES are rejected.
        assert_eq!(p.rejected.len(), 1);
        assert_eq!(p.rejected[0].filename, "b.log");
        assert!(matches!(p.rejected[0].reason, RejectReason::Oversize { .. }));
    }

    #[test]
    fn attachment_kind_label_picks_label_per_kind() {
        assert_eq!(
            attachment_kind_label(&[att("a.png", Some("image/png"))]),
            "[image attachment]"
        );
        assert_eq!(
            attachment_kind_label(&[att("b.md", Some("text/markdown"))]),
            "[text attachment]"
        );
        assert_eq!(
            attachment_kind_label(&[
                att("a.png", Some("image/png")),
                att("b.md", Some("text/markdown")),
            ]),
            "[image + text attachment]"
        );
        assert_eq!(
            attachment_kind_label(&[att("c.zip", Some("application/zip"))]),
            "[attachment]"
        );
    }

    fn att_sized(filename: &str, content_type: Option<&str>, size: u32) -> Attachment {
        let json = serde_json::json!({
            "id": "1",
            "filename": filename,
            "content_type": content_type,
            "size": size,
            "url": "https://cdn.discordapp.com/example.png",
            "proxy_url": "https://cdn.discordapp.com/example.png",
            "ephemeral": false,
        });
        serde_json::from_value(json).expect("attachment fixture should deserialize")
    }

    #[test]
    fn filter_text_by_content_type() {
        let atts = vec![
            att("a.txt", Some("text/plain")),
            att("b.md", Some("text/markdown")),
            att("c.png", Some("image/png")),
            att("d.pdf", Some("application/pdf")),
        ];
        let texts = filter_text_attachments(&atts);
        let names: Vec<&str> = texts.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.md"]);
    }

    #[test]
    fn filter_text_by_extension_when_content_type_missing() {
        let atts = vec![
            att("a.rs", None),
            att("b.md", None),
            att("c.json", None),
            att("d.bin", None),
            att("e.exe", None),
        ];
        let texts = filter_text_attachments(&atts);
        let names: Vec<&str> = texts.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(names, vec!["a.rs", "b.md", "c.json"]);
    }

    #[test]
    fn filter_text_rejects_oversized() {
        // Files in MAX_TEXT_BYTES..=MAX_DOWNLOAD_BYTES are accepted (and
        // truncated at download time); only files > MAX_DOWNLOAD_BYTES drop.
        let atts = vec![
            att_sized("small.txt", Some("text/plain"), 500_000),
            att_sized("big.txt", Some("text/plain"), MAX_DOWNLOAD_BYTES + 1),
        ];
        let texts = filter_text_attachments(&atts);
        let names: Vec<&str> = texts.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(names, vec!["small.txt"]);
    }

    #[test]
    fn filter_text_rejects_denylisted_extensions() {
        let atts = vec![
            att("ok.json", Some("text/plain")),
            att("creds.env", Some("text/plain")),
            att("secret.pem", Some("text/plain")),
            att("cert.p12", None),
            att("normal.txt", Some("text/plain")),
        ];
        let texts = filter_text_attachments(&atts);
        let names: Vec<&str> = texts.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(names, vec!["ok.json", "normal.txt"]);
    }

    #[test]
    fn partition_classifies_image_text_and_rejected() {
        let atts = vec![
            att("photo.png", Some("image/png")),
            att("notes.md", Some("text/markdown")),
            att("archive.zip", Some("application/zip")),
        ];
        let p = partition_attachments(&atts);
        assert_eq!(
            p.images.iter().map(|a| a.filename.as_str()).collect::<Vec<_>>(),
            vec!["photo.png"]
        );
        assert_eq!(
            p.text_files
                .iter()
                .map(|a| a.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["notes.md"]
        );
        assert_eq!(p.rejected.len(), 1);
        assert_eq!(p.rejected[0].filename, "archive.zip");
        assert!(matches!(
            p.rejected[0].reason,
            RejectReason::UnsupportedType { .. }
        ));
    }

    #[test]
    fn partition_rejects_oversize_with_size_in_reason() {
        let big = MAX_DOWNLOAD_BYTES + 1;
        let atts = vec![att_sized("huge.log", Some("text/plain"), big)];
        let p = partition_attachments(&atts);
        assert!(p.text_files.is_empty());
        assert_eq!(p.rejected.len(), 1);
        match &p.rejected[0].reason {
            RejectReason::Oversize { size } => assert_eq!(*size, big),
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn partition_rejects_denylist_even_with_text_mime() {
        let atts = vec![
            att("ok.json", Some("text/plain")),
            att("creds.env", Some("text/plain")),
            att("secret.pem", Some("text/plain")),
        ];
        let p = partition_attachments(&atts);
        assert_eq!(
            p.text_files
                .iter()
                .map(|a| a.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["ok.json"]
        );
        let rejected_names: Vec<&str> =
            p.rejected.iter().map(|r| r.filename.as_str()).collect();
        assert_eq!(rejected_names, vec!["creds.env", "secret.pem"]);
        assert!(p
            .rejected
            .iter()
            .all(|r| matches!(r.reason, RejectReason::SecurityDenylist)));
    }

    #[test]
    fn format_rejection_footer_returns_none_when_empty() {
        assert_eq!(format_rejection_footer(&[]), None);
    }

    #[test]
    fn format_rejection_footer_renders_each_reason_kind() {
        let rejected = vec![
            RejectedAttachment {
                filename: "creds.env".into(),
                reason: RejectReason::SecurityDenylist,
            },
            RejectedAttachment {
                filename: "huge.log".into(),
                reason: RejectReason::Oversize {
                    size: MAX_DOWNLOAD_BYTES + MAX_TEXT_BYTES / 2,
                },
            },
            RejectedAttachment {
                filename: "weird.iso".into(),
                reason: RejectReason::UnsupportedType {
                    content_type: Some("application/octet-stream".into()),
                    ext: Some("iso".into()),
                },
            },
        ];
        let footer = format_rejection_footer(&rejected).expect("footer");
        assert!(footer.starts_with("\u{26A0}\u{FE0F} skipped: "));
        assert!(footer.contains("creds.env (security)"));
        assert!(footer.contains("huge.log (8.5 MB > 8.0 MB)"));
        assert!(footer.contains("weird.iso (unsupported: application/octet-stream)"));
    }

    #[test]
    fn format_rejection_footer_falls_back_to_ext_when_no_content_type() {
        let rejected = vec![RejectedAttachment {
            filename: "thing.dat".into(),
            reason: RejectReason::UnsupportedType {
                content_type: None,
                ext: Some("dat".into()),
            },
        }];
        let footer = format_rejection_footer(&rejected).expect("footer");
        assert!(footer.contains("thing.dat (unsupported: dat)"));
    }

    #[test]
    fn doc_kind_for_detects_each_format() {
        assert_eq!(
            doc_kind_for(&att("report.pdf", Some("application/pdf"))),
            Some(DocKind::Pdf)
        );
        // Discord sometimes omits content_type — extension still detects.
        assert_eq!(doc_kind_for(&att("report.pdf", None)), Some(DocKind::Pdf));
        assert_eq!(
            doc_kind_for(&att(
                "notes.docx",
                Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            )),
            Some(DocKind::Docx)
        );
        assert_eq!(doc_kind_for(&att("notes.docx", None)), Some(DocKind::Docx));
        assert_eq!(
            doc_kind_for(&att("legacy.doc", Some("application/msword"))),
            Some(DocKind::Doc)
        );
        assert_eq!(doc_kind_for(&att("legacy.doc", None)), Some(DocKind::Doc));
    }

    #[test]
    fn doc_kind_for_returns_none_for_non_docs() {
        assert_eq!(doc_kind_for(&att("a.png", Some("image/png"))), None);
        assert_eq!(doc_kind_for(&att("b.txt", Some("text/plain"))), None);
        assert_eq!(doc_kind_for(&att("c.zip", Some("application/zip"))), None);
        assert_eq!(doc_kind_for(&att("noext", None)), None);
    }

    #[test]
    fn doc_command_for_pdf_invokes_pdftotext() {
        let (program, args) = doc_command_for(DocKind::Pdf, std::path::Path::new("/tmp/x.pdf"));
        assert_eq!(program, "pdftotext");
        assert!(args.iter().any(|a| a == "-layout"));
        assert!(args.iter().any(|a| a == "/tmp/x.pdf"));
        // Trailing "-" tells pdftotext to write to stdout.
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn doc_command_for_docx_and_doc_invoke_pandoc() {
        for kind in [DocKind::Docx, DocKind::Doc] {
            let (program, args) = doc_command_for(kind, std::path::Path::new("/tmp/x.docx"));
            assert_eq!(program, "pandoc", "kind={kind:?}");
            assert!(args.iter().any(|a| a == "--to=plain"), "kind={kind:?}");
            assert!(args.iter().any(|a| a == "/tmp/x.docx"), "kind={kind:?}");
        }
    }

    #[test]
    fn partition_routes_docs_into_docs_bucket() {
        let atts = vec![
            att("report.pdf", Some("application/pdf")),
            att("notes.docx", None),
            att("readme.md", Some("text/markdown")),
            att("photo.png", Some("image/png")),
            att("archive.zip", Some("application/zip")),
        ];
        let p = partition_attachments(&atts);
        let doc_names: Vec<&str> = p.docs.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(doc_names, vec!["report.pdf", "notes.docx"]);
        assert_eq!(
            p.text_files
                .iter()
                .map(|a| a.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["readme.md"]
        );
        assert_eq!(
            p.images.iter().map(|a| a.filename.as_str()).collect::<Vec<_>>(),
            vec!["photo.png"]
        );
        assert_eq!(p.rejected.len(), 1);
        assert_eq!(p.rejected[0].filename, "archive.zip");
    }
}
