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
        let (platform, value) = s
            .split_once(':')
            .ok_or_else(|| HandleError::Malformed(s.to_string()))?;
        // Only the handle prefixes, not inbound aliases like `gmail`.
        if !matches!(platform, "email" | "imessage") {
            return Err(HandleError::UnknownPlatform(platform.to_string()));
        }
        let handle = normalize_handle(platform, value)?;
        if handle.canonical() != s {
            return Err(HandleError::NotCanonical {
                given: s.to_string(),
                canonical: handle.canonical(),
            });
        }
        Ok(handle)
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
    let platform = Platform::parse(platform)
        .ok_or_else(|| HandleError::UnknownPlatform(platform.to_string()))?;
    let value = match platform {
        Platform::Email => email_value(raw)?,
        Platform::Imessage if raw.contains('@') => email_value(raw)?,
        Platform::Imessage => augmentagent_messages::handles::e164(
            raw,
            augmentagent_messages::handles::default_region(),
        )
        .ok_or_else(|| HandleError::InvalidPhone(raw.trim().to_string()))?,
    };
    Ok(Handle { platform, value })
}

/// Trim + lowercase the whole address; `Name <addr>` reduces to `addr`.
/// No dot or plus folding (D4).
fn email_value(raw: &str) -> Result<String, HandleError> {
    let trimmed = raw.trim();
    let addr = match (trimmed.rfind('<'), trimmed.ends_with('>')) {
        (Some(open), true) => trimmed[open + 1..trimmed.len() - 1].trim(),
        _ => trimmed,
    };
    let addr = addr.to_lowercase();
    let valid = match addr.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !addr.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';' | '"')
                })
        }
        None => false,
    };
    if valid {
        Ok(addr)
    } else {
        Err(HandleError::InvalidEmail(raw.trim().to_string()))
    }
}
