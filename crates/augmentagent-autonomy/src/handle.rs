//! D4 — one canonical handle form, one normaliser.
//!
//! A handle is `<platform>:<value>` with `platform ∈ {email, imessage}`.
//! - `email:` — the whole address trimmed and lowercased; no dot or plus
//!   folding.
//! - `imessage:` — an Apple-ID email (contains `@`) lowercased, otherwise a
//!   phone number in E.164 via the workspace's existing phone normaliser
//!   (`augmentagent_messages::handles::e164`).
//!
//! The config loader rejects any handle that is not already canonical, and
//! inbound resolution builds handles through the same [`normalize_handle`],
//! so the stored and the compared forms cannot disagree.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Platform {
    Email,
    Imessage,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Email => "email",
            Platform::Imessage => "imessage",
        }
    }

    /// The handle prefix (`email`, `imessage`) or an inbound `Email.platform`
    /// value: Gmail-direct mail arrives with platform `gmail`
    /// (`channel-email/src/inbound.rs`), which is the `email` platform here.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "email" | "gmail" => Some(Platform::Email),
            "imessage" => Some(Platform::Imessage),
            _ => None,
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandleError {
    #[error("unknown handle platform `{0}` (expected email or imessage)")]
    UnknownPlatform(String),
    #[error("`{0}` is not an email address")]
    InvalidEmail(String),
    #[error("`{0}` is not a valid phone number")]
    InvalidPhone(String),
    #[error("`{0}` is not of the form <platform>:<value>")]
    Malformed(String),
    #[error("handle `{given}` is not canonical (expected `{canonical}`)")]
    NotCanonical { given: String, canonical: String },
}

/// A canonical handle. Construct with [`normalize_handle`] or
/// [`Handle::parse_canonical`]; there is no other constructor, so every
/// `Handle` value is canonical.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle {
    platform: Platform,
    value: String,
}

impl Handle {
    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// `<platform>:<value>`.
    pub fn canonical(&self) -> String {
        format!("{}:{}", self.platform, self.value)
    }

    /// Parse a stored handle string and require it to be canonical already:
    /// `normalize_handle(platform, value)` must reproduce it byte for byte.
    pub fn parse_canonical(s: &str) -> Result<Handle, HandleError> {
        let _ = s;
        todo!()
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.platform, self.value)
    }
}

/// D4 — normalise a raw value seen on `platform` (`email`, `gmail` or
/// `imessage`) into its canonical handle. An email sender in
/// `Name <addr>` form is reduced to `addr` first.
pub fn normalize_handle(platform: &str, raw: &str) -> Result<Handle, HandleError> {
    let _ = (platform, raw);
    todo!()
}
