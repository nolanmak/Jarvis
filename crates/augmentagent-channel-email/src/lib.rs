//! Gmail channel adapter for AugmentAgent.
//!
//! Polls Gmail via Composio, spawns Claude per new email (triage → draft → ingest),
//! writes to sqlite, hands drafts to the Discord approval broker. Shared pipeline
//! primitives (`Reasoner`, prompt builders, decision parsing, ingest) live in
//! `augmentagent-channel-core`; this crate owns only Gmail-specific transport.

pub mod gmail;
pub mod sigextract;
pub mod tone;
mod channel;

pub use channel::{GmailChannel, GmailChannelConfig, PollOutcome};
pub use sigextract::{
    detect_signature_block, signature_patch, ExtractedFields, SignatureBlock,
    SignatureExtractor, SigError,
};

// Back-compat re-exports: old callers can keep using `augmentagent_channel_email::X`
// for items that moved to `augmentagent-channel-core`. Remove once all callers migrate.
pub use augmentagent_channel_core::{
    decision, decision::Decision, decision::DecisionKind, ingest, prompt, reasoner,
    ClaudeCliReasoner, Reasoner, ReasonerOpts,
};
