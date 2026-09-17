//! Structured cross-channel message search (#1095).
//!
//! Every stored message (`emails` row, any platform) gets a normalized
//! `message_index` row: conversation, kind, sender, direction, time. The
//! index is derived data — rebuildable from `emails` at any time — and is
//! maintained without touching the triage write path: SQLite triggers on
//! `emails` enqueue changed ids into `message_index_queue`, and
//! [`index::drain`] turns queued ids into index rows.
//!
//! Nothing in this crate calls a model. Extraction is deterministic code.

pub mod extract;
pub mod handles;
pub mod index;
pub mod people;

pub use extract::{extract, EmailRowView, IndexFields, OwnerHandles, EXTRACTOR_VERSION};
pub use index::{check, drain, enqueue_stale, DrainReport, IndexHealth, YIELD_PAUSE};
