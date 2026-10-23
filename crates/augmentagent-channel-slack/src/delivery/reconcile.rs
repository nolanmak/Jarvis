//! Finding a send whose outcome was lost (#1285/#1294).
//!
//! Every post the dispatcher makes carries its outbox idempotency key in
//! Slack message metadata ([`super::METADATA_EVENT_TYPE`]). When a post timed
//! out, or the process died with it in flight, [`SlackSendReconciler`] reads
//! the conversation back — `conversations.replies` for a thread,
//! `conversations.history` otherwise — with `include_all_metadata`, starting
//! at the send's claim time minus [`ReconcilePolicy::skew_ms`], for at most
//! [`ReconcilePolicy::max_pages`] pages of [`ReconcilePolicy::page_limit`].
//!
//! - Key found → delivered, with that message's `ts`.
//! - Every page read, key absent, and the claim is older than
//!   [`ReconcilePolicy::settle_ms`] → not delivered; resending is safe.
//! - Key absent but the claim is younger than the settle window → too early
//!   to tell; look again when the window has passed (not a failure).
//! - The lookup fails, or the page budget runs out first → a failed lookup;
//!   the dispatcher retries with backoff and dead-letters after
//!   [`ReconcilePolicy::max_lookups`].
//!
//! Uploads: the result of `files.completeUploadExternal` is not recorded
//! when its reply is lost, and a shared file's message carries no metadata
//! we set, so an uncertain upload is treated as not delivered once the
//! settle window has passed. The worst case is one duplicate file, never a
//! missing one. Edits (`chat.update`) are idempotent and are simply redone.

use std::future::Future;

use augmentagent_store::delivery::{
    OutboundOperation, OutboundSend, ReconcileOutcome, RetryPolicy, SendReconciler,
};
use serde_json::Value;

use super::METADATA_EVENT_TYPE;
use crate::surface::SlackMessageId;
use crate::transport::web::{HistoryQuery, SlackWebApi};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilePolicy {
    /// A missing key only proves "not delivered" this long after the claim.
    pub settle_ms: i64,
    /// Look this far before the claim time (clock skew between hosts).
    pub skew_ms: i64,
    /// Messages per history page.
    pub page_limit: u32,
    /// Pages per lookup; more than this is a failed lookup.
    pub max_pages: u32,
    /// Failed lookups before the send is dead-lettered.
    pub max_lookups: u32,
    /// Backoff between failed lookups.
    pub retry: RetryPolicy,
}

impl Default for ReconcilePolicy {
    fn default() -> Self {
        Self {
            settle_ms: 30_000,
            skew_ms: 60_000,
            page_limit: 200,
            max_pages: 5,
            max_lookups: 5,
            retry: RetryPolicy {
                base_delay_ms: 5_000,
                max_delay_ms: 5 * 60_000,
            },
        }
    }
}

/// Result of one lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    Found {
        provider_message_id: String,
    },
    NotFound,
    /// Not visible yet; ask again at `recheck_at_ms`.
    TooEarly {
        recheck_at_ms: i64,
    },
    Failed(String),
}

/// `ms` since the epoch as a Slack `ts` (`<seconds>.<micros>`).
pub fn slack_ts_from_ms(ms: i64) -> String {
    let ms = ms.max(0);
    format!("{}.{:06}", ms / 1000, (ms % 1000) * 1000)
}

fn carries_key(metadata: Option<&Value>, key: &str) -> bool {
    metadata.is_some_and(|m| {
        m.get("event_type").and_then(Value::as_str) == Some(METADATA_EVENT_TYPE)
            && m.pointer("/event_payload/idempotency_key")
                .and_then(Value::as_str)
                == Some(key)
    })
}

/// Looks sends up in Slack history by the idempotency key in their metadata.
pub struct SlackSendReconciler<'a> {
    api: &'a dyn SlackWebApi,
    policy: ReconcilePolicy,
    now_ms: Option<i64>,
}

impl<'a> SlackSendReconciler<'a> {
    pub fn new(api: &'a dyn SlackWebApi, policy: ReconcilePolicy) -> Self {
        Self {
            api,
            policy,
            now_ms: None,
        }
    }

    /// Fix "now" for the [`SendReconciler`] impl (default: the wall clock).
    pub fn at(mut self, now_ms: i64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }

    pub fn policy(&self) -> &ReconcilePolicy {
        &self.policy
    }

    fn settled_or_too_early(&self, send: &OutboundSend, now_ms: i64) -> LookupResult {
        let settles_at = send
            .last_claimed_at_ms
            .map_or(i64::MIN, |c| c.saturating_add(self.policy.settle_ms));
        if now_ms < settles_at {
            LookupResult::TooEarly {
                recheck_at_ms: settles_at,
            }
        } else {
            LookupResult::NotFound
        }
    }

    pub async fn lookup_at(&self, send: &OutboundSend, now_ms: i64) -> LookupResult {
        match send.operation {
            OutboundOperation::Post | OutboundOperation::InteractionResponse => {}
            OutboundOperation::Upload => return self.settled_or_too_early(send, now_ms),
            OutboundOperation::Update => return LookupResult::NotFound,
        }
        let channel = send.conversation.conversation_id().to_string();
        let thread_ts = send.conversation.thread_id().map(str::to_string);
        let oldest = send
            .last_claimed_at_ms
            .map(|c| slack_ts_from_ms(c.saturating_sub(self.policy.skew_ms)));
        let mut cursor: Option<String> = None;
        for _ in 0..self.policy.max_pages.max(1) {
            let query = HistoryQuery {
                channel: channel.clone(),
                thread_ts: thread_ts.clone(),
                oldest: oldest.clone(),
                limit: self.policy.page_limit.max(1),
                cursor: cursor.take(),
                include_all_metadata: true,
            };
            let page = match &thread_ts {
                Some(_) => self.api.conversations_replies(query).await,
                None => self.api.conversations_history(query).await,
            };
            let page = match page {
                Ok(p) => p,
                Err(e) => return LookupResult::Failed(e.to_string()),
            };
            if let Some(hit) = page
                .messages
                .iter()
                .find(|m| carries_key(m.metadata.as_ref(), &send.idempotency_key))
            {
                let id = SlackMessageId::new(&channel, &hit.ts)
                    .map(|id| id.encode())
                    .unwrap_or_else(|_| format!("{channel}:{}", hit.ts));
                return LookupResult::Found {
                    provider_message_id: id,
                };
            }
            match page.next_cursor {
                Some(next) if page.has_more => cursor = Some(next),
                _ => return self.settled_or_too_early(send, now_ms),
            }
        }
        LookupResult::Failed(format!(
            "history page budget exhausted ({} pages of {}) before the send was found",
            self.policy.max_pages, self.policy.page_limit
        ))
    }
}

impl SendReconciler for SlackSendReconciler<'_> {
    fn lookup(&self, send: &OutboundSend) -> impl Future<Output = ReconcileOutcome> + Send {
        let now_ms = self.now_ms.unwrap_or_else(wall_clock_ms);
        async move {
            match self.lookup_at(send, now_ms).await {
                LookupResult::Found {
                    provider_message_id,
                } => ReconcileOutcome::Delivered {
                    provider_message_id,
                },
                LookupResult::NotFound => ReconcileOutcome::NotDelivered,
                LookupResult::TooEarly { .. } | LookupResult::Failed(_) => {
                    ReconcileOutcome::Unknown
                }
            }
        }
    }
}

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
