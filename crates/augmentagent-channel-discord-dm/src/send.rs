//! Outbound: send a DM back to the original Discord channel on approval.

use std::sync::Arc;

use serenity::all::{ChannelId, CreateMessage};
use serenity::http::Http;

/// Send `content` to a Discord DM channel. The channel id is whatever was
/// captured at inbound time — for DMs that's a stable per-user-pair id, so the
/// reply lands in the same DM thread the original message came from.
pub async fn send_discord_dm(
    http: &Arc<Http>,
    channel_id: u64,
    content: &str,
) -> anyhow::Result<()> {
    let channel = ChannelId::new(channel_id);
    let message = CreateMessage::new().content(content);
    channel
        .send_message(http.as_ref(), message)
        .await
        .map_err(|e| anyhow::anyhow!("discord send_message: {e}"))?;
    Ok(())
}
