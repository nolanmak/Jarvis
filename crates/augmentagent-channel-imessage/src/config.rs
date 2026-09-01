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
