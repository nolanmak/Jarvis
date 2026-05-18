//! Slack workspace digest (#8).
//!
//! Slack's channel poll stores `SubscriptionMode::Digest` items into the
//! `emails` table (see `channel.rs::handle_message` →
//! `outcome.digest_stored`), but until now nothing composed those into a
//! daily summary — Discord had a digest scheduler, Slack did not.
//!
//! Rather than fork a second copy of the scheduler, this reuses the now
//! platform-parameterized [`DigestScheduler`] from
//! `augmentagent-channel-discord-dm`, bound to `platform = "slack"`. All the
//! store helpers it relies on (`list_active_subscriptions`,
//! `recent_emails_for_thread`, `mark_digest_posted`) are already
//! `channel_id`-keyed and platform-filtered, so no store change was needed.
//!
//! Output groups by Slack channel (one digest per Digest-mode subscription,
//! which maps 1:1 to a Slack channel id) and uses the same Haiku
//! summarization path — cheap, a few hundred one-liners per call.

use std::path::PathBuf;
use std::sync::Arc;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::Reasoner;
use augmentagent_store::Store;

pub use augmentagent_channel_discord_dm::digest::{
    DIGEST_WINDOW_HOURS, DIGEST_WINDOW_MS, MIN_BETWEEN_DIGESTS_MS,
};

use crate::PLATFORM;

/// Type alias: the Slack digest scheduler is the shared
/// [`DigestScheduler`](augmentagent_channel_discord_dm::digest::DigestScheduler)
/// pinned to the Slack platform discriminator.
pub type SlackDigestScheduler<R> =
    augmentagent_channel_discord_dm::digest::DigestScheduler<R>;

/// Construct a digest scheduler bound to `platform = "slack"`. Mirrors the
/// Discord spawn in `main.rs`; the only differences are the platform filter
/// and the prompt's source noun.
pub fn slack_digest_scheduler<R: Reasoner + 'static>(
    store: Arc<Store>,
    reasoner: Arc<R>,
    approvals: Arc<dyn ApprovalBroker>,
    wiki_root: Option<PathBuf>,
) -> SlackDigestScheduler<R> {
    augmentagent_channel_discord_dm::digest::DigestScheduler::for_platform(
        store,
        reasoner,
        approvals,
        wiki_root,
        PLATFORM,
        "Slack channel",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_scheduler_filters_on_slack_platform() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
        let reasoner = Arc::new(augmentagent_channel_core::ClaudeCliReasoner::new());
        let broker: Arc<dyn ApprovalBroker> =
            Arc::new(augmentagent_approval_discord::NoopBroker);
        let sched = slack_digest_scheduler(store, reasoner, broker, None);
        assert_eq!(sched.platform(), PLATFORM);
        assert_eq!(sched.platform(), "slack");
    }
}
