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
