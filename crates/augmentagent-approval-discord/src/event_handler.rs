//! Serenity event handler that routes button/modal interactions into the broker.

use std::sync::Arc;

use serenity::all::{
    Context, CreateInteractionResponse, CreateInteractionResponseMessage, EventHandler,
    Interaction, Ready,
};
use tracing::{debug, info, warn};

use crate::broker::{BrokerState, DeliveryOutcome};
use crate::custom_id::{CustomId, Verb};
use crate::layout::{extract_feedback, revise_modal};
use crate::ApprovalOutcome;

pub struct Handler {
    pub state: Arc<BrokerState>,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("discord broker ready as {}", ready.user.name);
        self.state.mark_ready();
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
                        // Component interactions don't carry modal text; this arm is defensive.
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
