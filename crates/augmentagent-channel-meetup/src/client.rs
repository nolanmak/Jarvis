//! Thin async wrapper over `scripts/meetup-events.mjs` (Node). Precedent:
//! `crates/augmentagent-cli/src/invoice.rs` shelling out to Python.
//!
//! The mjs script speaks Meetup's public GraphQL (`meetup.com/gql2`) via
//! persisted-query sha256 hashes — no auth. Those hashes are tied to Meetup's
//! frontend bundle: when Meetup ships a new bundle the script exits 2
//! (`PersistedQueryNotFound`), which we surface as a non-fatal
//! `MeetupError::StalePersistedQuery` so a stale hash degrades one channel
//! rather than crashing the daemon.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum MeetupError {
    #[error("meetup persisted-query hash is stale — refresh via /intercept")]
    StalePersistedQuery,
    #[error("meetup group not found or has no events: {0}")]
    GroupNotFound(String),
    #[error("spawning node: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("decoding meetup output: {0}")]
    Decode(String),
    #[error("meetup script error: {0}")]
    Runtime(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeetupVenue {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetupEvent {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub date_time: Option<String>,
    #[serde(default)]
    pub is_online: bool,
    #[serde(default)]
    pub going: Option<i64>,
    #[serde(default)]
    pub venue: Option<MeetupVenue>,
}

#[derive(Debug, Deserialize)]
struct MeetupCliOut {
    #[serde(default)]
    events: Vec<MeetupEvent>,
}

/// Resolves + invokes `node scripts/meetup-events.mjs`.
pub struct MeetupClient {
    node_bin: String,
    script: PathBuf,
}

impl MeetupClient {
    /// `repo_root` is the daemon's working dir (where `scripts/` lives).
    /// `AUGMENTAGENT_NODE_BIN` (absolute path) overrides the `node` lookup —
    /// the tenant unit's restricted PATH may not include nvm's node.
    pub fn new(repo_root: &Path) -> Self {
        let node_bin = std::env::var("AUGMENTAGENT_NODE_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "node".to_string());
        Self {
            node_bin,
            script: repo_root.join("scripts/meetup-events.mjs"),
        }
    }

    /// Fetch up to `limit` upcoming events for `urlname` (the group slug).
    pub async fn upcoming_events(
        &self,
        urlname: &str,
        limit: usize,
    ) -> Result<Vec<MeetupEvent>, MeetupError> {
        let out = Command::new(&self.node_bin)
            .arg(&self.script)
            .arg(urlname)
            .arg("--json")
            .arg("--limit")
            .arg(limit.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // mjs: exit 2 = PersistedQueryNotFound, exit 1 = other.
            if out.status.code() == Some(2)
                || stderr.contains("PersistedQueryNotFound")
            {
                return Err(MeetupError::StalePersistedQuery);
            }
            if stderr.contains("not found") {
                return Err(MeetupError::GroupNotFound(urlname.to_string()));
            }
            return Err(MeetupError::Runtime(stderr.trim().to_string()));
        }

        let parsed: MeetupCliOut = serde_json::from_slice(&out.stdout)
            .map_err(|e| MeetupError::Decode(e.to_string()))?;
        Ok(parsed.events)
    }
}
