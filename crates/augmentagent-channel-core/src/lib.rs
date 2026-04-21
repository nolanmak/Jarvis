//! Shared channel primitives: reasoner trait, prompt builders, decision parsing, wiki ingest.
//!
//! Platform-specific channels (email, linkedin, slack, …) depend on this crate for the
//! triage → draft → ingest pipeline. Each channel implements its own poll loop and transport,
//! but consumes the same `Reasoner` abstraction and prompt/decision/ingest logic.

pub mod decision;
pub mod ingest;
pub mod prompt;
pub mod reasoner;

pub use decision::{Decision, DecisionKind};
pub use reasoner::{ClaudeCliReasoner, Reasoner, ReasonerOpts};
