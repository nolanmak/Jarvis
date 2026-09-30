//! #1294 — outbound delivery on Slack: Markdown → mrkdwn, splitting long
//! answers, planning them onto the durable outbox (#1285), dispatching them
//! through the Web API and throttled progress edits. Limits and what is
//! unverified: `docs/SLACK-TRANSPORT.md`.
//!
//! Entry points for the serve wiring (#1287) and the turn harness (#1288):
//! [`enqueue_answer`] + [`SlackOutboxDispatcher::drain`] to deliver an
//! answer (text and generated files), and [`ProgressMessage`] for the
//! status line during a turn.

pub mod mrkdwn;
pub mod plan;
pub mod progress;
pub mod reconcile;
pub mod split;

pub use mrkdwn::markdown_to_mrkdwn;
pub use plan::{
    enqueue_answer, notice_idempotency_key, part_idempotency_key, plan_answer, Answer,
    AnswerEnqueued, AnswerFile, DispatchOutcome, Dispatched, PartKind, PlanError, PlanOptions,
    PlannedSend, SlackOutboxDispatcher, METADATA_EVENT_TYPE,
};
pub use progress::{ProgressConfig, ProgressMessage, ProgressReport};
pub use reconcile::{slack_ts_from_ms, LookupResult, ReconcilePolicy, SlackSendReconciler};
pub use split::{split_message, MessagePart, DEFAULT_PART_CHARS, MIN_PART_CHARS, SLACK_TEXT_LIMIT};
