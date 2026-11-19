//! Owner text commands that are neither `/model` nor a legacy `!word`
//! (#2208). The Discord handler recognises nothing itself: it hands every
//! owner message to [`crate::QueryHandler::owner_text_command`], which
//! returns `Some` for a message whose first token is a command it owns,
//! valid or not, so such text can never become model input. The trait method
//! decides and formats; the handler performs every Discord call.

/// Where the command was typed. Filled by the handler from the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerCommandCtx {
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub message_id: u64,
    pub is_dm: bool,
}

/// What the handler must do with a recognised command.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnerCommandOutcome {
    /// Post this text as a Discord reply to the command message. Replying
    /// (not a plain send) is what lets later history drop the exchange.
    Reply(String),
}

/// First tokens that are owner text commands. Their messages, and the bot's
/// direct replies to them, never enter later conversation history.
pub const TRIGGERS: [&str; 1] = ["/search"];

pub fn is_owner_text_command(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|first| TRIGGERS.contains(&first))
}
