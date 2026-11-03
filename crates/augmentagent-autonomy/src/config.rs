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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::handle::{Handle, Platform};
use crate::registry;

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
        match self.tier {
            Tier::Full => registry::is_known(action),
            Tier::Actions => {
                registry::is_listable(action) && self.allowed_actions.iter().any(|a| a == action)
            }
        }
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
        let mut out = Vec::new();
        if let Some(fatal) = &self.fatal {
            out.push(format!(
                "trusted contacts: {} could not be loaded, so nobody is trusted: {fatal}",
                self.path.display()
            ));
        }
        out.extend(
            self.problems
                .iter()
                .map(|p| format!("trusted contacts: {p}")),
        );
        out
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
        let mut report = LoadReport {
            path: path.to_path_buf(),
            file_hash: Some(file_hash(text.as_bytes())),
            ..LoadReport::default()
        };
        let tables = match parse_tables(text) {
            Ok(t) => t,
            Err(e) => {
                report.fatal = Some(e);
                return report;
            }
        };

        // Validate each entry on its own.
        let mut valid: Vec<(usize, TrustedContact)> = Vec::new();
        for (index, table) in tables.into_iter().enumerate() {
            let slug = table
                .get("wiki")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let entry = match ContactEntry::deserialize(toml::Value::Table(table)) {
                Ok(e) => e,
                Err(e) => {
                    report.problems.push(ContactProblem {
                        index,
                        slug,
                        reason: one_line(&e.to_string()),
                    });
                    continue;
                }
            };
            match validate_entry(&entry) {
                Ok(c) => valid.push((index, c)),
                Err(reason) => report.problems.push(ContactProblem {
                    index,
                    slug,
                    reason,
                }),
            }
        }

        // Cross-contact: a handle or slug on more than one contact disables
        // every contact that shares it.
        let mut by_handle: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_slug: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (pos, (_, c)) in valid.iter().enumerate() {
            for h in &c.handles {
                by_handle.entry(h.canonical()).or_default().push(pos);
            }
            by_slug.entry(c.slug.as_str()).or_default().push(pos);
        }
        let mut reasons: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (handle, owners) in &by_handle {
            if owners.len() > 1 {
                for &pos in owners {
                    reasons
                        .entry(pos)
                        .or_default()
                        .push(format!("handle {handle} is also on another contact"));
                }
            }
        }
        for (slug, owners) in &by_slug {
            if owners.len() > 1 {
                for &pos in owners {
                    reasons
                        .entry(pos)
                        .or_default()
                        .push(format!("wiki slug `{slug}` appears more than once"));
                }
            }
        }
        for (pos, (index, contact)) in valid.into_iter().enumerate() {
            match reasons.remove(&pos) {
                Some(r) => report.problems.push(ContactProblem {
                    index,
                    slug: Some(contact.slug),
                    reason: r.join("; "),
                }),
                None => report.contacts.push(contact),
            }
        }
        report.problems.sort_by_key(|p| p.index);
        report
    }
}

/// `AUGMENTAGENT_TRUSTED_CONTACTS` when set and non-empty, else
/// [`DEFAULT_CONFIG_PATH`] relative to the working directory.
pub fn config_path() -> PathBuf {
    match std::env::var(CONFIG_ENV) {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v.trim()),
        _ => PathBuf::from(DEFAULT_CONFIG_PATH),
    }
}

/// Read and validate the file. A missing file is an empty config with no
/// alert (autonomy simply has nobody to act for).
pub fn load_config(path: &Path) -> LoadReport {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => LoadReport::from_text(path, &text),
            Err(e) => LoadReport {
                path: path.to_path_buf(),
                file_hash: Some(file_hash(e.as_bytes())),
                fatal: Some("file is not valid UTF-8".into()),
                ..LoadReport::default()
            },
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LoadReport {
            path: path.to_path_buf(),
            ..LoadReport::default()
        },
        Err(e) => LoadReport {
            path: path.to_path_buf(),
            fatal: Some(format!("cannot read: {e}")),
            ..LoadReport::default()
        },
    }
}

/// The `[[contact]]` tables of a config file. Any TOML error, a top-level
/// key other than `contact`, or a `contact` that is not an array of tables
/// is a whole-file (fatal) error.
pub(crate) fn parse_tables(text: &str) -> Result<Vec<toml::Table>, String> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| one_line(&e.to_string()))?;
    let mut tables = Vec::new();
    for (key, value) in doc {
        if key != "contact" {
            return Err(format!(
                "unknown top-level key `{key}` (expected only [[contact]] tables)"
            ));
        }
        let toml::Value::Array(items) = value else {
            return Err("`contact` must be an array of tables ([[contact]])".into());
        };
        for item in items {
            let toml::Value::Table(t) = item else {
                return Err("`contact` must be an array of tables ([[contact]])".into());
            };
            tables.push(t);
        }
    }
    Ok(tables)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Validate one entry on its own (no cross-contact duplicate check). The
/// CLI runs this before writing so it never writes an entry the loader
/// would disable.
pub fn validate_entry(entry: &ContactEntry) -> Result<TrustedContact, String> {
    let slug = entry.wiki.trim();
    if slug.is_empty() || slug != entry.wiki || slug.contains(['/', '\\']) || slug.starts_with('.')
    {
        return Err(format!(
            "`wiki` must be a bare people/ slug, got `{}`",
            entry.wiki
        ));
    }
    let tier = match entry.tier.as_deref() {
        None => return Err("missing `tier` (required: \"actions\" or \"full\")".into()),
        Some(t) => Tier::parse(t)
            .ok_or_else(|| format!("unknown tier `{t}` (expected \"actions\" or \"full\")"))?,
    };
    for action in &entry.allowed_actions {
        if !registry::is_listable(action) {
            return Err(if action == registry::REPLY {
                "`reply` cannot be listed in allowed_actions; it comes only with tier = \"full\""
                    .into()
            } else {
                format!("unknown action `{action}` in allowed_actions")
            });
        }
    }
    if tier == Tier::Full && !entry.allowed_actions.is_empty() {
        return Err("tier = \"full\" must have an empty or omitted allowed_actions".into());
    }
    if entry.handles.is_empty() {
        return Err("no handles".into());
    }
    let mut handles: Vec<Handle> = Vec::with_capacity(entry.handles.len());
    for raw in &entry.handles {
        let h = Handle::parse_canonical(raw).map_err(|e| format!("bad handle `{raw}`: {e}"))?;
        if handles.contains(&h) {
            return Err(format!("handle {h} is listed twice"));
        }
        handles.push(h);
    }
    let calendar_email = match &entry.calendar_email {
        None => None,
        Some(addr) => {
            let want = format!("email:{addr}");
            if !handles.iter().any(|h| h.canonical() == want) {
                return Err(format!(
                    "calendar_email `{addr}` is not among the handles (need `{want}`)"
                ));
            }
            Some(addr.clone())
        }
    };
    if calendar_email.is_none()
        && entry
            .allowed_actions
            .iter()
            .any(|a| a == registry::ADD_GUEST)
    {
        return Err("calendar.add_guest is allowed but calendar_email is missing".into());
    }
    let preferred_handle = match &entry.preferred_channel {
        Some(p) => handles
            .iter()
            .find(|h| h.canonical() == *p)
            .cloned()
            .ok_or_else(|| format!("preferred_channel `{p}` is not among the handles"))?,
        None => handles
            .iter()
            .find(|h| h.platform() == Platform::Imessage)
            .or_else(|| handles.iter().find(|h| h.platform() == Platform::Email))
            .cloned()
            .expect("handles is non-empty"),
    };
    let daily_cap = entry.daily_cap.unwrap_or(DEFAULT_DAILY_CAP);
    let entry_hash = entry_hash(
        slug,
        tier,
        &entry.allowed_actions,
        &handles,
        calendar_email.as_deref(),
        &preferred_handle,
        daily_cap,
    );
    Ok(TrustedContact {
        slug: slug.to_string(),
        tier,
        allowed_actions: entry.allowed_actions.clone(),
        handles,
        calendar_email,
        preferred_handle,
        daily_cap,
        entry_hash,
    })
}

/// Hash of the entry's normalised content. Order of handles and actions
/// does not matter; every field that affects what may happen does.
fn entry_hash(
    slug: &str,
    tier: Tier,
    allowed_actions: &[String],
    handles: &[Handle],
    calendar_email: Option<&str>,
    preferred: &Handle,
    daily_cap: u32,
) -> String {
    let mut actions: Vec<&str> = allowed_actions.iter().map(String::as_str).collect();
    actions.sort_unstable();
    actions.dedup();
    let mut hs: Vec<String> = handles.iter().map(Handle::canonical).collect();
    hs.sort_unstable();
    let canonical = serde_json::json!({
        "v": 1,
        "wiki": slug,
        "tier": tier.as_str(),
        "allowed_actions": actions,
        "handles": hs,
        "calendar_email": calendar_email,
        "preferred_channel": preferred.canonical(),
        "daily_cap": daily_cap,
    });
    hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
}

/// `sha256` hex of the file bytes.
pub fn file_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
