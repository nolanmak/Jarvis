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
    /// The message is in the query channel itself, not in a thread under it.
    pub is_query_channel: bool,
    /// The configured query channel, when there is one.
    pub query_channel_id: Option<u64>,
}

/// What the handler must do with a recognised command.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnerCommandOutcome {
    /// Post this text as a Discord reply to the command message. Replying
    /// (not a plain send) is what lets later history drop the exchange.
    Reply(String),
    /// #2219 — create a yolo thread from the command message, record it in
    /// the yolo thread registry, post `intro` in the thread, and reply to the
    /// command with a link to it.
    OpenYoloThread { intro: String },
}

/// First tokens that are owner text commands. Their messages, and the bot's
/// direct replies to them, never enter later conversation history.
pub const TRIGGERS: [&str; 2] = ["/search", "/yolo"];

/// Reply to anything that looks like a command in a yolo thread but is not
/// `/yolo` or `/search`. Other owner commands do not run there.
pub const NOT_IN_YOLO_THREAD: &str = "Not available in a yolo thread.";

/// Prefix `tag` as its own first line on every chunk, splitting any chunk
/// that would then exceed `limit` characters, so no message loses the tag or
/// goes over Discord's size limit.
pub fn tag_chunks(tag: &str, chunks: Vec<String>, limit: usize) -> Vec<String> {
    let room = limit.saturating_sub(tag.chars().count() + 1).max(1);
    let mut tagged = Vec::new();
    for chunk in chunks {
        let characters: Vec<char> = chunk.chars().collect();
        for part in characters.chunks(room) {
            tagged.push(format!("{tag}\n{}", part.iter().collect::<String>()));
        }
        if characters.is_empty() {
            tagged.push(tag.to_string());
        }
    }
    tagged
}

pub fn is_owner_text_command(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|first| TRIGGERS.contains(&first))
}
