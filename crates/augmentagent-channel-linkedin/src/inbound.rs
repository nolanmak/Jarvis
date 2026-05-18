//! `InboundSource` adapter for LinkedIn DMs (#25, partial).
//!
//! Wraps `LinkedInApi::fetch_recent_dms` into the generic [`WorkItem`] shape
//! so LinkedIn can be driven by [`augmentagent_channel_core::ChannelRunner`].
//! Mirrors the Gmail `GmailInbound` and Telegram `TelegramBotInbound`
//! adapters.
//!
//! Scope note (`Refs #25 — partial`): this is the additive "raw inbox" view.
//! The production path remains [`LinkedInChannel::poll_once`](crate::LinkedInChannel),
//! which owns the rich triage → draft → approve → ingest dispatch plus the
//! 4h-cadence jitter and is covered by the existing channel tests. Fully
//! retiring that bespoke loop in favour of `ChannelRunner` is deferred to keep
//! the regression surface contained, exactly as the Telegram bot keeps its own
//! `poll_once` alongside an `InboundSource` adapter.
//!
//! Outbound (self-sent) messages are filtered here so the adapter's contract
//! matches `poll_once`, which skips `dm.is_outbound(member_urn)` before triage.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use augmentagent_channel_core::{InboundSource, WorkItem};

use crate::api::{LinkedInApi, LinkedInError};
use crate::types::Dm;

/// Platform discriminator for LinkedIn rows / work items.
pub const PLATFORM: &str = "linkedin";

/// Raw-inbox adapter over LinkedIn DMs.
pub struct LinkedInInbound<L: LinkedInApi> {
    pub api: Arc<L>,
    /// The user's own member URN — used to drop self-sent messages, exactly
    /// like `LinkedInChannel::poll_once`.
    pub member_urn: String,
}

impl<L: LinkedInApi> LinkedInInbound<L> {
    pub fn new(api: Arc<L>, member_urn: String) -> Self {
        Self { api, member_urn }
    }
}

/// Build the `WorkItem` for one inbound DM. `payload` carries the serialized
/// `Email` (via `Dm::into_email`) so a handler reconstructs the typed struct
/// without a re-fetch — same approach as the Gmail/Telegram adapters.
pub fn dm_to_work_item(dm: Dm, my_urn: &str) -> WorkItem {
    let external_id = dm.message_urn.clone();
    let email = dm.into_email(my_urn);
    WorkItem {
        platform: PLATFORM.to_string(),
        kind: "dm".to_string(),
        external_id,
        payload: serde_json::to_value(&email).unwrap_or(serde_json::Value::Null),
    }
}

#[async_trait]
impl<L: LinkedInApi + 'static> InboundSource for LinkedInInbound<L> {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        let dms = match self.api.fetch_recent_dms().await {
            Ok(dms) => dms,
            Err(LinkedInError::AuthExpired) => {
                warn!("linkedin inbound: auth expired — run `augmentagent linkedin login`");
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut items = Vec::new();
        for dm in dms {
            // Same guard as poll_once: never triage our own outbound messages.
            if dm.is_outbound(&self.member_urn) {
                continue;
            }
            items.push(dm_to_work_item(dm, &self.member_urn));
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemberUrn;
    use std::sync::Mutex;

    struct FakeApi {
        dms: Mutex<Vec<Dm>>,
        fail: Mutex<Option<LinkedInError>>,
    }

    impl FakeApi {
        fn with(dms: Vec<Dm>) -> Self {
            Self {
                dms: Mutex::new(dms),
                fail: Mutex::new(None),
            }
        }
        fn failing(err: LinkedInError) -> Self {
            Self {
                dms: Mutex::new(Vec::new()),
                fail: Mutex::new(Some(err)),
            }
        }
    }

    #[async_trait]
    impl LinkedInApi for FakeApi {
        async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e);
            }
            Ok(std::mem::take(&mut *self.dms.lock().unwrap()))
        }
        async fn send_message(
            &self,
            _conversation_urn: &str,
            _text: &str,
        ) -> Result<String, LinkedInError> {
            Ok("urn:li:msg:sent".into())
        }
        async fn fetch_feed_posts_by_author(
            &self,
            _author_urn: &str,
        ) -> Result<Vec<crate::types::FeedPost>, LinkedInError> {
            Ok(Vec::new())
        }
        async fn post_comment(
            &self,
            _post_urn: &str,
            _text: &str,
        ) -> Result<String, LinkedInError> {
            Ok("urn:li:comment:fake".into())
        }
        async fn react(
            &self,
            _post_urn: &str,
            _reaction: &str,
        ) -> Result<(), LinkedInError> {
            Ok(())
        }
        async fn create_share(
            &self,
            _draft: crate::posting::PostDraft<'_>,
        ) -> Result<crate::posting::ShareUrn, LinkedInError> {
            Ok(crate::posting::ShareUrn("urn:li:share:fake".into()))
        }
    }

    fn mk_dm(id: &str, sender: &str) -> Dm {
        Dm {
            message_urn: id.to_string(),
            conversation_urn: format!("conv-{id}"),
            peer_name: "Jane Doe".to_string(),
            peer_urn: MemberUrn("urn:li:peer".to_string()),
            sender_urn: MemberUrn(sender.to_string()),
            text: format!("hello {id}"),
            delivered_at_ms: 1_775_000_000_000,
        }
    }

    const ME: &str = "urn:li:me";

    #[tokio::test]
    async fn fetch_new_yields_inbound_dms_only() {
        let api = Arc::new(FakeApi::with(vec![
            mk_dm("m1", "urn:li:peer"),
            mk_dm("self", ME), // outbound — must be filtered
            mk_dm("m2", "urn:li:peer"),
        ]));
        let inbound = LinkedInInbound::new(api, ME.to_string());
        let items = inbound.fetch_new().await.unwrap();
        let ids: Vec<_> = items.iter().map(|w| w.external_id.clone()).collect();
        assert_eq!(ids, vec!["m1", "m2"]);
        assert!(items.iter().all(|w| w.platform == "linkedin" && w.kind == "dm"));
        // Payload round-trips into a typed Email with the linkedin prefix.
        let email: augmentagent_store::Email =
            serde_json::from_value(items[0].payload.clone()).unwrap();
        assert_eq!(email.message_id, "m1");
        assert_eq!(email.platform, "linkedin");
    }

    #[tokio::test]
    async fn fetch_new_swallows_auth_expired_as_empty() {
        let api = Arc::new(FakeApi::failing(LinkedInError::AuthExpired));
        let inbound = LinkedInInbound::new(api, ME.to_string());
        // Mirrors poll_once: AuthExpired logs + yields nothing, not an error.
        assert!(inbound.fetch_new().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_new_propagates_other_errors() {
        let api = Arc::new(FakeApi::failing(LinkedInError::Decode("bad json".into())));
        let inbound = LinkedInInbound::new(api, ME.to_string());
        let err = inbound.fetch_new().await.unwrap_err();
        assert!(err.to_string().contains("decode"));
    }
}
