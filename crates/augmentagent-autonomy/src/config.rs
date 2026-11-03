//! D3 — the owner-only `trusted-contacts.toml` loader.
//!
//! ```toml
//! [[contact]]
//! wiki = "jane-doe"                  # people/<slug>.md (context + display only)
//! tier = "actions"                   # required: "actions" | "full"
//! allowed_actions = ["calendar.add_guest", "calendar.send_details"]  # [] / omitted for full
//! handles = ["imessage:+15555550100", "email:jane@work.example"]    # canonical (D4)
//! calendar_email = "jane@work.example"   # "email:"+this must be in handles
//! preferred_channel = "imessage:+15555550100"  # optional; must be one of handles
//! daily_cap = 10                     # optional, default 10 plans / rolling 24 h
//! ```
//!
//! Validation is fail-closed. A whole-file parse error loads as empty with
//! an alert ([`LoadReport::fatal`]). A per-contact error disables that
//! contact only and is reported in [`LoadReport::problems`]:
//! - missing or unknown `tier`;
//! - an action name the registry does not list (`reply` is not listable);
//! - `full` tier with a non-empty `allowed_actions`;
//! - a non-canonical handle, or no handles at all;
//! - `calendar_email` not among the handles (or missing while
//!   `calendar.add_guest` is allowed);
//! - `preferred_channel` not among the handles;
//! - an unknown key in the entry;
//! - a handle (or wiki slug) that also appears on another contact — every
//!   contact sharing it is disabled.
//!
//! A loaded contact is only *eligible*: whether it is trusted (active) or
//! pending is decided by [`crate::trust::reconcile`] against the store.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::handle::Handle;

/// Override for the config path.
pub const CONFIG_ENV: &str = "AUGMENTAGENT_TRUSTED_CONTACTS";
/// Default path, relative to the daemon's working directory (the repo root).
pub const DEFAULT_CONFIG_PATH: &str = "config/trusted-contacts.toml";
pub const DEFAULT_DAILY_CAP: u32 = 10;

/// D2 — per-person tier. No default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    Actions,
    Full,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Actions => "actions",
            Tier::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "actions" => Some(Tier::Actions),
            "full" => Some(Tier::Full),
            _ => None,
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `[[contact]]` table exactly as written in the file. The CLI writer
/// round-trips this type; the loader validates it into [`TrustedContact`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactEntry {
    pub wiki: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_actions: Vec<String>,
    #[serde(default)]
    pub handles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_cap: Option<u32>,
}

/// A contact that passed validation. Eligible, not yet trusted: see
/// [`crate::trust::TrustSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedContact {
    /// Wiki slug — the key for trust state, pauses and plans.
    pub slug: String,
    pub tier: Tier,
    pub allowed_actions: Vec<String>,
    pub handles: Vec<Handle>,
    /// Invite address for `calendar.add_guest` (bare, lowercased).
    pub calendar_email: Option<String>,
    /// Where proactive outreach goes (D16): `preferred_channel`, else the
    /// first `imessage:` handle, else the first `email:` handle.
    pub preferred_handle: Handle,
    pub daily_cap: u32,
    /// `sha256` of the entry's canonical form; trust state is keyed by
    /// `(slug, entry_hash)`, so any change to the entry re-enters pending.
    pub entry_hash: String,
}

impl TrustedContact {
    pub fn has_handle(&self, handle: &Handle) -> bool {
        self.handles.iter().any(|h| h == handle)
    }

    /// Whether a registry action may run for this contact without a click
    /// (D2): `full` allows every registry action and `reply`; `actions`
    /// allows exactly its `allowed_actions`.
    pub fn allows(&self, action: &str) -> bool {
        let _ = action;
        todo!()
    }
}

/// A per-contact validation failure. The contact is disabled; the rest
/// load normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactProblem {
    /// 0-based position of the `[[contact]]` table in the file.
    pub index: usize,
    /// The entry's `wiki` slug, when it had a readable one.
    pub slug: Option<String>,
    pub reason: String,
}

impl std::fmt::Display for ContactProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.slug {
            Some(slug) => write!(f, "contact `{slug}` disabled: {}", self.reason),
            None => write!(f, "contact #{} disabled: {}", self.index + 1, self.reason),
        }
    }
}

/// Result of one config load. Callers post [`LoadReport::alerts`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadReport {
    pub path: PathBuf,
    /// `sha256` of the file bytes; `None` when the file does not exist.
    pub file_hash: Option<String>,
    /// Contacts that passed validation, in file order.
    pub contacts: Vec<TrustedContact>,
    pub problems: Vec<ContactProblem>,
    /// Whole-file failure (unreadable, TOML parse error, wrong shape). When
    /// set, `contacts` is empty.
    pub fatal: Option<String>,
}

impl LoadReport {
    /// Owner-facing alert lines: the fatal error, then one line per
    /// disabled contact.
    pub fn alerts(&self) -> Vec<String> {
        todo!()
    }

    pub fn contact(&self, slug: &str) -> Option<&TrustedContact> {
        self.contacts.iter().find(|c| c.slug == slug)
    }

    /// The loaded contact owning `handle` (eligible; not necessarily active).
    pub fn contact_for(&self, handle: &Handle) -> Option<&TrustedContact> {
        self.contacts.iter().find(|c| c.has_handle(handle))
    }

    /// Parse and validate `text` as if it had been read from `path`.
    pub fn from_text(path: &Path, text: &str) -> LoadReport {
        let _ = (path, text);
        todo!()
    }
}

/// `AUGMENTAGENT_TRUSTED_CONTACTS` when set and non-empty, else
/// [`DEFAULT_CONFIG_PATH`] relative to the working directory.
pub fn config_path() -> PathBuf {
    todo!()
}

/// Read and validate the file. A missing file is an empty config with no
/// alert (autonomy simply has nobody to act for).
pub fn load_config(path: &Path) -> LoadReport {
    let _ = path;
    todo!()
}

/// Validate one entry on its own (no cross-contact duplicate check). The
/// CLI runs this before writing so it never writes an entry the loader
/// would disable.
pub fn validate_entry(entry: &ContactEntry) -> Result<TrustedContact, String> {
    let _ = entry;
    todo!()
}

/// `sha256` hex of the file bytes.
pub fn file_hash(bytes: &[u8]) -> String {
    let _ = bytes;
    todo!()
}
