//! Shared channel primitives: reasoner trait, prompt builders, decision parsing, wiki ingest,
//! and the [`Trigger`] abstraction for work sources.
//!
//! Platform-specific channels (email, linkedin, slack, …) depend on this crate for the
//! triage → draft → ingest pipeline. Each channel implements its own poll loop and transport,
//! but consumes the same `Reasoner` abstraction and prompt/decision/ingest logic. New platforms
//! additionally implement [`Trigger`] so Phase 2 digests and Phase 3 feed engagement can share
//! one work-source contract instead of inventing their own per platform.

pub mod decision;
pub mod ingest;
pub mod prompt;
pub mod reasoner;
pub mod trigger;

pub use decision::{Decision, DecisionKind};
pub use reasoner::{ClaudeCliReasoner, Reasoner, ReasonerOpts};
pub use trigger::{
    DigestSource, FriendFeedSource, InboundMessageTrigger, InboundSource, Trigger, WorkItem,
};
