//! Wiki layer for AugmentAgent.
//!
//! Owns filesystem layout + deterministic path helpers for the LLM-maintained
//! markdown knowledge base. No LLM logic lives here — callers in
//! `augmentagent-channel-email` drive the Claude spawns; this crate only
//! knows *where* files go and how to name them.

pub mod layout;
pub mod reader;
pub mod slug;

pub use layout::WikiLayout;
pub use reader::WikiReader;
pub use slug::slug_from_email;
