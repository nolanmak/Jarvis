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
    Attachment, Context, CreateInteractionResponse, CreateInteractionResponseFollowup,
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

        let user_text = msg.content.trim().to_string();
        let images = filter_image_attachments(&msg.attachments);
        if user_text.is_empty() && images.is_empty() {
            return;
        }

        let handler = Arc::clone(handler);
        let http = ctx.http.clone();
        let channel_id = msg.channel_id;
        let msg_id = msg.id;

        tokio::spawn(async move {
            // Download images to /tmp so the reasoner's Read tool can open them.
            // A partial download (some succeed, some fail) is fine — we proceed
            // with whatever landed and warn about the rest.
            let downloaded = download_images(&images, msg_id.get()).await;

            let prompt = build_prompt(&user_text, &downloaded);

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

/// Keep only attachments whose Discord-reported `content_type` looks like an
/// image. Discord populates this from the upload, so it's reliable enough to
/// gate which attachments we hand to the vision model.
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
