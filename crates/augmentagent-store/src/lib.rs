//! Product-state store backed by the shared `data.db` sqlite file.
//!
//! Schema is owned by the Node tree (`src/db.ts`); this crate never runs migrations.
//! Opens the database in WAL journal mode so the Express dashboard can read
//! concurrently.

pub mod models;
mod store;

pub use models::{Account, ActionRecord, ActionStatus, Email, LearnedPattern, TriageResult};
pub use store::{Store, StoreError, StoreResult};
