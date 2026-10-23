//! Deterministic phone controls, dispatched only after the gateway's owner
//! allowlist and conversation checks. Raw setup text never enters model turns.
use crate::QueryHandler;

/// Provider identifiers from the authenticated gateway message, never parsed
/// from command text. The runtime adapter must still check stored authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneCommandContext {
    pub guild_id: Option<u64>,
    pub bot_user_id: u64,
    pub channel_id: u64,
    pub author_id: u64,
    pub message_id: u64,
}

pub use augmentagent_phone::commands::sensitive as is_private_control;

pub async fn dispatch(
    handler: &dyn QueryHandler,
    context: &PhoneCommandContext,
    text: &str,
) -> Option<String> {
    if !augmentagent_phone::commands::recognizes(text) {
        return None;
    }
    Some(
        handler
            .phone_command(context, text)
            .await
            .unwrap_or_else(|| "Phone commands are unavailable in this Discord connection.".into()),
    )
}

impl PhoneCommandContext {
    /// Identity conversion only; callers must authorize the result against the
    /// stored owner and control conversation before installing a turn context.
    pub fn origin(&self) -> anyhow::Result<augmentagent_store::phone::PhoneOrigin> {
        use augmentagent_store::{
            SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
        };
        anyhow::ensure!(
            self.bot_user_id != 0
                && self.author_id != 0
                && self.channel_id != 0
                && self.message_id != 0
                && self.guild_id != Some(0),
            "Discord phone identity is unavailable"
        );
        let account_id = self
            .guild_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| format!("dm:{}", self.bot_user_id));
        let account = SurfaceAccountRef::new(SurfacePlatform::new("discord")?, account_id)?;
        Ok(augmentagent_store::phone::PhoneOrigin {
            owner: SurfaceOwnerRef::new(account.clone(), self.author_id.to_string())?,
            conversation: SurfaceConversationRef::new(account, self.channel_id.to_string(), None)?,
        })
    }
}

pub async fn answer_turn(
    handler: &dyn QueryHandler,
    context: &PhoneCommandContext,
    audit: &crate::AuditCtx,
    history: &str,
    current: &str,
) -> anyhow::Result<String> {
    let answer = handler.answer_turn(audit, history, current);
    match handler.phone_origin(context) {
        Some(origin) if audit.owner_authorized => {
            augmentagent_phone::context::scope(origin, None, answer).await
        }
        _ => answer.await,
    }
}

/// Model controls use the same authenticated origin as the following text turn,
/// so a bound DM cannot silently switch away from its native phone session.
pub async fn model_command(
    handler: &dyn QueryHandler,
    context: &PhoneCommandContext,
    text: &str,
) -> Option<String> {
    let command = handler.model_command_in_guild(context.guild_id, context.channel_id, text);
    match handler.phone_origin(context) {
        Some(origin) => augmentagent_phone::context::scope(origin, None, command).await,
        None => command.await,
    }
}
