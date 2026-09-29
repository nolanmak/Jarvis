//! An answer → ordered entries on the durable outbox (#1285) → Slack.
//!
//! [`plan_answer`] converts the Markdown once, splits it
//! ([`super::split_message`]) and adds one upload per generated file. Every
//! entry targets the same conversation (thread included) and carries a key
//! derived only from the turn ID, the part kind and the part index
//! ([`part_idempotency_key`]), so re-planning the same turn after a restart
//! enqueues nothing new and the outbox's per-conversation ordering sends
//! whatever is left, in order.
//!
//! [`SlackOutboxDispatcher`] claims this workspace's sends and performs them:
//! `chat.postMessage` (with `link_names: false` and the idempotency key in
//! message metadata), `chat.update`, or the external file upload. Failures
//! are classified so that nothing is resent blindly:
//!
//! | failure | outbox result |
//! | --- | --- |
//! | rate limited, Slack transient error, HTTP 5xx | `failed`, retried after `max(backoff, Retry-After)` |
//! | upload failed before completion (`UploadIncomplete`) | retried as a whole (nothing was shared) |
//! | timeout / connection lost / unreadable reply on a post or completion | `reconcile`: may have landed; resolved by a `SendReconciler`, never resent blindly |
//! | same on `chat.update` | retried (an edit is idempotent) |
//! | any other Slack error, HTTP 4xx, bad file, bad payload | `dead_letter` |

use std::path::PathBuf;

use augmentagent_store::delivery::{
    EnqueueOutcome, NewOutboundSend, OutboundOperation, OutboundSend, RetryPolicy, SendStatus,
};
use augmentagent_store::{
    Store, StoreError, StoreResult, SurfaceAccountRef, SurfaceConversationRef,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tracing::{debug, warn};

use super::{markdown_to_mrkdwn, split_message, DEFAULT_PART_CHARS};
use crate::surface::{SlackMessageId, SlackWorkspace};
use crate::transport::web::{
    PostMessage, SlackWebApi, UpdateMessage, UploadFile, UploadSource, WebApiError,
};

/// `event_type` of the metadata attached to every delivered post.
pub const METADATA_EVENT_TYPE: &str = "augmentagent_delivery";

/// Slack error codes worth another attempt; every other code is permanent.
const TRANSIENT_SLACK_ERRORS: &[&str] = &[
    "internal_error",
    "fatal_error",
    "service_unavailable",
    "request_timeout",
    "ratelimited",
    "rate_limited",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Text,
    File,
}

/// `turn:<turn_id>:text:<index>` / `turn:<turn_id>:file:<index>`.
pub fn part_idempotency_key(turn_id: &str, kind: PartKind, index: usize) -> String {
    let kind = match kind {
        PartKind::Text => "text",
        PartKind::File => "file",
    };
    format!("turn:{turn_id}:{kind}:{index}")
}

/// A generated file to share after the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerFile {
    /// Read at send time, so it must still exist then. Spaces and Unicode
    /// are fine; the path must be valid UTF-8 to be stored.
    pub path: PathBuf,
    /// Defaults to the path's file name.
    pub filename: Option<String>,
    pub title: Option<String>,
    pub alt_text: Option<String>,
}

/// One turn's answer.
#[derive(Debug, Clone, Copy)]
pub struct Answer<'a> {
    /// Stable across restarts of the same turn; keys are derived from it.
    pub turn_id: &'a str,
    pub markdown: &'a str,
    pub files: &'a [AnswerFile],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOptions {
    /// Characters per posted part (see [`super::split`]).
    pub part_chars: usize,
    /// Outbox attempt budget per part.
    pub max_attempts: u32,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            part_chars: DEFAULT_PART_CHARS,
            max_attempts: 5,
        }
    }
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("nothing to deliver: the answer is empty and has no files")]
    Empty,
    #[error("invalid answer: {0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Serialize, Deserialize)]
struct TextPayload {
    text: String,
    part: usize,
    parts: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct FilePayload {
    path: String,
    filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alt_text: Option<String>,
}

/// The outbox entries for an answer, in delivery order: text parts, then
/// files. Pure and deterministic.
pub fn plan_answer(
    conversation: &SurfaceConversationRef,
    answer: &Answer<'_>,
    opts: &PlanOptions,
) -> Result<Vec<NewOutboundSend>, PlanError> {
    if answer.turn_id.trim().is_empty() {
        return Err(PlanError::Invalid("turn ID is blank".into()));
    }
    SlackWorkspace::from_account(conversation.account())
        .map_err(|_| PlanError::Invalid("conversation is not a Slack conversation".into()))?;
    let max_attempts = opts.max_attempts.max(1);
    let mut sends = Vec::new();

    if !answer.markdown.trim().is_empty() {
        let text = markdown_to_mrkdwn(answer.markdown);
        let parts = split_message(&text, opts.part_chars);
        let total = parts.len();
        for (index, part) in parts.into_iter().enumerate() {
            let payload = TextPayload {
                text: part.text,
                part: index + 1,
                parts: total,
            };
            sends.push(NewOutboundSend {
                conversation: conversation.clone(),
                idempotency_key: part_idempotency_key(answer.turn_id, PartKind::Text, index),
                operation: OutboundOperation::Post,
                target_message_id: None,
                payload: serde_json::to_string(&payload)
                    .map_err(|e| PlanError::Invalid(e.to_string()))?,
                max_attempts,
                interaction_expires_at_ms: None,
            });
        }
    }

    for (index, file) in answer.files.iter().enumerate() {
        let path = file.path.to_str().ok_or_else(|| {
            PlanError::Invalid(format!("file path is not UTF-8: {}", file.path.display()))
        })?;
        let filename = match &file.filename {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => file
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .ok_or_else(|| PlanError::Invalid(format!("no file name in {path}")))?,
        };
        let payload = FilePayload {
            path: path.to_string(),
            filename,
            title: file.title.clone(),
            alt_text: file.alt_text.clone(),
        };
        sends.push(NewOutboundSend {
            conversation: conversation.clone(),
            idempotency_key: part_idempotency_key(answer.turn_id, PartKind::File, index),
            operation: OutboundOperation::Upload,
            target_message_id: None,
            payload: serde_json::to_string(&payload)
                .map_err(|e| PlanError::Invalid(e.to_string()))?,
            max_attempts,
            interaction_expires_at_ms: None,
        });
    }

    if sends.is_empty() {
        return Err(PlanError::Empty);
    }
    Ok(sends)
}

/// One planned entry after enqueueing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSend {
    pub id: i64,
    pub idempotency_key: String,
    pub operation: OutboundOperation,
    /// `None` when newly queued; the existing status for a duplicate.
    pub status: Option<SendStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerEnqueued {
    pub sends: Vec<PlannedSend>,
    pub queued: usize,
    pub duplicates: usize,
}

/// Plan and enqueue an answer. Safe to call again for the same turn: known
/// keys come back as duplicates with their current status.
pub fn enqueue_answer(
    store: &Store,
    conversation: &SurfaceConversationRef,
    answer: &Answer<'_>,
    opts: &PlanOptions,
    now_ms: i64,
) -> Result<AnswerEnqueued, PlanError> {
    let plan = plan_answer(conversation, answer, opts)?;
    let mut out = AnswerEnqueued {
        sends: Vec::with_capacity(plan.len()),
        queued: 0,
        duplicates: 0,
    };
    for send in plan {
        let (id, status) = match store.enqueue_outbound_send(&send, now_ms)? {
            EnqueueOutcome::Queued { id } => {
                out.queued += 1;
                (id, None)
            }
            EnqueueOutcome::Duplicate { id, status } => {
                out.duplicates += 1;
                (id, Some(status))
            }
        };
        out.sends.push(PlannedSend {
            id,
            idempotency_key: send.idempotency_key,
            operation: send.operation,
            status,
        });
    }
    Ok(out)
}

/// What happened to one claimed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    Sent {
        provider_message_id: String,
    },
    Retrying {
        error: String,
        next_attempt_at_ms: i64,
    },
    DeadLettered {
        error: String,
    },
    /// May have reached Slack; parked in `reconcile`.
    Uncertain {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    pub id: i64,
    pub idempotency_key: String,
    pub operation: OutboundOperation,
    pub outcome: DispatchOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Retry { retry_after_ms: i64 },
    Permanent,
    Uncertain,
}

fn classify(op: OutboundOperation, err: &WebApiError) -> Class {
    let retry = Class::Retry { retry_after_ms: 0 };
    match err {
        WebApiError::RateLimited { retry_after } => Class::Retry {
            retry_after_ms: i64::try_from(retry_after.as_millis()).unwrap_or(i64::MAX),
        },
        WebApiError::Slack { error, .. } if TRANSIENT_SLACK_ERRORS.contains(&error.as_str()) => {
            retry
        }
        WebApiError::Slack { .. } => Class::Permanent,
        WebApiError::Http { status, .. } if *status >= 500 => retry,
        WebApiError::Http { .. } => Class::Permanent,
        WebApiError::Timeout
        | WebApiError::Transport(_)
        | WebApiError::Json(_)
        | WebApiError::Cancelled => {
            if op == OutboundOperation::Update {
                retry
            } else {
                Class::Uncertain
            }
        }
        WebApiError::UploadIncomplete { source, .. } => match classify(op, source) {
            Class::Uncertain => retry,
            other => other,
        },
        WebApiError::FileTooLarge { .. }
        | WebApiError::InvalidUpload(_)
        | WebApiError::Unsupported(_) => Class::Permanent,
    }
}

/// Performs this workspace's outbox sends through a [`SlackWebApi`].
pub struct SlackOutboxDispatcher<'a> {
    store: &'a Store,
    api: &'a dyn SlackWebApi,
    account: SurfaceAccountRef,
    retry: RetryPolicy,
}

impl<'a> SlackOutboxDispatcher<'a> {
    pub fn new(store: &'a Store, api: &'a dyn SlackWebApi, workspace: &SlackWorkspace) -> Self {
        Self {
            store,
            api,
            account: workspace.account(),
            retry: RetryPolicy {
                base_delay_ms: 2_000,
                max_delay_ms: 5 * 60_000,
            },
        }
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Claim and perform the next due send of this workspace, if any.
    pub async fn dispatch_next(&self, now_ms: i64) -> StoreResult<Option<Dispatched>> {
        let Some(send) = self
            .store
            .claim_next_outbound_send_for(&self.account, now_ms)?
        else {
            return Ok(None);
        };
        debug!(
            id = send.id,
            key = %send.idempotency_key,
            op = send.operation.as_str(),
            attempt = send.attempts,
            "slack outbox: sending"
        );
        let outcome = match self.perform(&send).await {
            Ok(provider_message_id) => {
                self.store
                    .mark_outbound_sent(send.id, &provider_message_id, now_ms)?;
                DispatchOutcome::Sent {
                    provider_message_id,
                }
            }
            Err((class, error)) => self.record_failure(&send, class, error, now_ms)?,
        };
        Ok(Some(Dispatched {
            id: send.id,
            idempotency_key: send.idempotency_key,
            operation: send.operation,
            outcome,
        }))
    }

    /// Dispatch until nothing of this workspace is due at `now_ms`. Stops by
    /// construction: failed sends are due later and uncertain ones are
    /// parked.
    pub async fn drain(&self, now_ms: i64) -> StoreResult<Vec<Dispatched>> {
        let mut out = Vec::new();
        while let Some(done) = self.dispatch_next(now_ms).await? {
            out.push(done);
        }
        Ok(out)
    }

    fn record_failure(
        &self,
        send: &OutboundSend,
        class: Class,
        error: String,
        now_ms: i64,
    ) -> StoreResult<DispatchOutcome> {
        warn!(id = send.id, key = %send.idempotency_key, %error, ?class, "slack outbox: send failed");
        Ok(match class {
            Class::Uncertain => {
                self.store
                    .mark_outbound_uncertain(send.id, &error, now_ms)?;
                DispatchOutcome::Uncertain { error }
            }
            Class::Permanent => {
                self.store
                    .mark_outbound_failed(send.id, &error, false, &self.retry, now_ms)?;
                DispatchOutcome::DeadLettered { error }
            }
            Class::Retry { retry_after_ms } => {
                let policy = RetryPolicy {
                    base_delay_ms: self.retry.base_delay_ms.max(retry_after_ms),
                    max_delay_ms: self.retry.max_delay_ms.max(retry_after_ms),
                };
                match self
                    .store
                    .mark_outbound_failed(send.id, &error, true, &policy, now_ms)?
                {
                    SendStatus::DeadLetter => DispatchOutcome::DeadLettered { error },
                    _ => {
                        let next_attempt_at_ms = self
                            .store
                            .outbound_send(send.id)?
                            .map_or(now_ms, |s| s.next_attempt_at_ms);
                        DispatchOutcome::Retrying {
                            error,
                            next_attempt_at_ms,
                        }
                    }
                }
            }
        })
    }

    async fn perform(&self, send: &OutboundSend) -> Result<String, (Class, String)> {
        let bad_payload = |e: serde_json::Error| (Class::Permanent, format!("bad payload: {e}"));
        let api_err = |e: WebApiError| (classify(send.operation, &e), e.to_string());
        let channel = send.conversation.conversation_id().to_string();
        let thread_ts = send.conversation.thread_id().map(str::to_string);
        match send.operation {
            OutboundOperation::Post => {
                let p: TextPayload = serde_json::from_str(&send.payload).map_err(bad_payload)?;
                let posted = self
                    .api
                    .post_message(PostMessage {
                        channel,
                        text: p.text,
                        thread_ts,
                        link_names: Some(false),
                        metadata: Some(json!({
                            "event_type": METADATA_EVENT_TYPE,
                            "event_payload": { "idempotency_key": send.idempotency_key },
                        })),
                        ..PostMessage::default()
                    })
                    .await
                    .map_err(api_err)?;
                Ok(message_id(&posted.channel, &posted.ts))
            }
            OutboundOperation::Update => {
                let p: TextPayload = serde_json::from_str(&send.payload).map_err(bad_payload)?;
                let target = send.target_message_id.as_deref().unwrap_or_default();
                let id = SlackMessageId::parse(target).map_err(|_| {
                    (
                        Class::Permanent,
                        "update target is not <channel>:<ts>".to_string(),
                    )
                })?;
                let updated = self
                    .api
                    .update_message(UpdateMessage {
                        channel: id.channel_id().to_string(),
                        ts: id.ts().to_string(),
                        text: p.text,
                        blocks: None,
                    })
                    .await
                    .map_err(api_err)?;
                Ok(message_id(&updated.channel, &updated.ts))
            }
            OutboundOperation::Upload => {
                let p: FilePayload = serde_json::from_str(&send.payload).map_err(bad_payload)?;
                let uploaded = self
                    .api
                    .upload_file(UploadFile {
                        channel: Some(channel),
                        filename: p.filename,
                        source: UploadSource::Path(PathBuf::from(p.path)),
                        title: p.title,
                        thread_ts,
                        initial_comment: None,
                        alt_text: p.alt_text,
                    })
                    .await
                    .map_err(api_err)?;
                Ok(uploaded.id)
            }
            OutboundOperation::InteractionResponse => Err((
                Class::Permanent,
                "interaction responses are not delivered by the Slack dispatcher yet".into(),
            )),
        }
    }
}

fn message_id(channel: &str, ts: &str) -> String {
    SlackMessageId::new(channel, ts)
        .map(|id| id.encode())
        .unwrap_or_else(|_| format!("{channel}:{ts}"))
}
