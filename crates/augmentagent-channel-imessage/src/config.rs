//! Feature gate + settings. Per the `JournalConfig::load` contract:
//! `None` = config absent → feature off; the daemon must start cleanly.

use std::path::PathBuf;

use tracing::warn;

#[derive(Debug, Clone)]
pub struct ImessageConfig {
    /// Canonicalized root of the bundle repo (holds `conversations/`).
    /// Canonicalize-early per the #337 relative-path lesson.
    pub repo_dir: PathBuf,
    /// Optional S3 bucket/prefix for attachment fetch (stretch, #888).
    pub s3_bucket: Option<String>,
    pub s3_prefix: Option<String>,
}

impl ImessageConfig {
    pub fn load() -> Option<Self> {
        let raw = std::env::var("AUGMENTAGENT_IMESSAGE_REPO_DIR").ok()?;
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let repo_dir = match std::fs::canonicalize(raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(dir = raw, error = %e, "AUGMENTAGENT_IMESSAGE_REPO_DIR set but unusable; imessage ingest disabled");
                return None;
            }
        };
        Some(Self {
            repo_dir,
            s3_bucket: std::env::var("AUGMENTAGENT_IMESSAGE_S3_BUCKET")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            s3_prefix: std::env::var("AUGMENTAGENT_IMESSAGE_S3_PREFIX")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        })
    }
}

/// Opt-in: send each new batch of history messages through the LLM wiki
/// ingest. Off by default — every changed conversation per poll is one
/// Haiku call on the owner's subscription, and the rows are already
/// searchable without it.
pub const ENV_HISTORY_WIKI_CAPTURE: &str = "AUGMENTAGENT_HISTORY_WIKI_CAPTURE";

pub fn history_wiki_capture_enabled() -> bool {
    parse_flag(std::env::var(ENV_HISTORY_WIKI_CAPTURE).ok().as_deref())
}

fn parse_flag(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_capture_is_off_unless_explicitly_enabled() {
        assert!(!parse_flag(None));
        assert!(!parse_flag(Some("")));
        assert!(!parse_flag(Some("0")));
        assert!(!parse_flag(Some("false")));
        assert!(parse_flag(Some("1")));
        assert!(parse_flag(Some(" TRUE ")));
    }
}
