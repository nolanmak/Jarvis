//! Gmail channel adapter for AugmentAgent.
//!
//! Polls Gmail via Composio, spawns Claude per new email (triage → draft → ingest),
//! writes to sqlite, hands drafts to the Discord approval broker. Shared pipeline
//! primitives (`Reasoner`, prompt builders, decision parsing, ingest) live in
//! `augmentagent-channel-core`; this crate owns only Gmail-specific transport.

pub mod gmail;
pub mod inbound;
pub mod outbound;
pub mod scheduled;
pub mod sigextract;
pub mod tone;
mod channel;

pub use channel::{
    DispatchOutcome, GmailChannel, GmailChannelConfig, GmailWorkHandler, PollOutcome,
};
pub use inbound::{email_to_work_item, GmailInbound};
pub use outbound::{
    classify_outbound, Classification, OutboundEvent, OutboundObserver, ACTION_ID_HEADER,
};
pub use scheduled::{
    record_self_send, ScheduledSendEngine, TickSummary, RETRY_EXEMPT_RETRY_COUNT,
    SEND_DRAFT_TIMEOUT,
};
pub use sigextract::{
    detect_signature_block, is_human_sender, is_meeting_invite, signature_patch, ExtractedFields,
    SignatureBlock, SignatureExtractor, SigError,
};

// Back-compat re-exports: old callers can keep using `augmentagent_channel_email::X`
// for items that moved to `augmentagent-channel-core`. Remove once all callers migrate.
pub use augmentagent_channel_core::{
    decision, decision::Decision, decision::DecisionKind, ingest, prompt, reasoner,
    ClaudeCliReasoner, Reasoner, ReasonerOpts,
};
