//! ShadowNote journal channel — read/write the user's journal via the app's
//! existing AppSync GraphQL API.
//!
//! ShadowNote (private repo `nolanmak/ShadowNoteReborn`) is an Amplify app:
//! AppSync + DynamoDB with DataStore sync, Cognito owner auth, and
//! client-side envelope encryption of entry content (KMS data key +
//! CryptoJS passphrase mode). This crate is the low-level integration
//! layer the read (wiki ingest) and write (Discord journaling) phases
//! build on:
//!
//! - [`client`] — SigV4-signed GraphQL calls (`syncEntries`, `listEntries`,
//!   `getEntry`, `createEntry`) as the `augmentagent-shadownote` IAM
//!   principal. IAM was chosen over Cognito user auth so the daemon never
//!   needs an interactive sign-in and nothing expires.
//! - [`crypto`] — the entry-content envelope: KMS-decrypt the per-entry
//!   data key, then CryptoJS/OpenSSL-EVP AES-256-CBC for the body.
//! - [`config`] — opt-in configuration via keyring/env (`SHADOWNOTE_*`).
//!   Absent config must degrade to "feature off", never to a crash.
//!
//! Two invariants callers must not break:
//!
//! 1. **Owner scoping.** The IAM auth rule on `Entry` is not owner-scoped —
//!    it can see every user's rows. Everything in this crate filters by
//!    `SHADOWNOTE_OWNER_ID` server-side *and* asserts it client-side; the
//!    config loader fails closed when the owner id is missing.
//! 2. **Writes go through GraphQL only.** DataStore conflict detection is
//!    enabled on the API; raw DynamoDB writes would corrupt `_version`
//!    sync metadata for the app.

pub mod channel;
pub mod client;
pub mod config;
pub mod crypto;
pub mod html;

pub use channel::{
    JournalChannel, JournalChannelConfig, JournalRuntime, PollOutcome, DEFAULT_BASE_SYNC_THRESHOLD,
    DEFAULT_MAX_ENTRIES_PER_POLL, DEFAULT_POLL_INTERVAL,
};
pub use client::{Entry, EntryPage, JournalApi, JournalError, NewEntry, ShadowNoteClient};
pub use config::JournalConfig;
pub use crypto::{
    decrypt_entry_content, encrypt_entry_content, CryptoError, DekProvider, EnvelopeCiphertext,
    GeneratedDek, KmsDekProvider,
};
pub use html::html_to_text;
