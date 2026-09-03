//! Wiki layer for AugmentAgent.
//!
//! Owns filesystem layout + deterministic path helpers for the LLM-maintained
//! markdown knowledge base. No LLM logic lives here — callers in
//! `augmentagent-channel-email` drive the Claude spawns; this crate only
//! knows *where* files go and how to name them.

pub mod crm;
pub mod freshness;
pub mod identity;
pub mod index;
pub mod layout;
pub mod locks;
pub mod migrate;
pub mod reader;
pub mod slug;

pub use crm::{merge_person_page, MergeResult, PersonPatch};
pub use identity::{Identities, IdentityIndex, PersonPage};
pub use index::{rebuild_index, IndexStats};
pub use layout::WikiLayout;
pub use locks::{with_page_lock, LOCK_TIMEOUT};
pub use reader::{transcripts_meetings_dir_from_env, WikiReader};
pub use slug::slug_from_email;
