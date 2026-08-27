//! Thin async wrapper over `scripts/meetup-events.mjs` (Node). Precedent:
//! `crates/augmentagent-cli/src/invoice.rs` shelling out to Python.
//!
//! The mjs script speaks Meetup's public GraphQL (`meetup.com/gql2`) via
//! persisted-query sha256 hashes — no auth. Those hashes are tied to Meetup's
//! frontend bundle: when Meetup ships a new bundle the script exits 2
//! (`PersistedQueryNotFound`). We then retry through
//! `scripts/meetup-events-ssr.mjs`, which reads the same events out of the
//! server-rendered page and emits the same JSON — the fallback
//! `scripts/wix-events-sync.mjs` already uses. Only when that also fails do we
//! surface the non-fatal `MeetupError::StalePersistedQuery`, so a rotated hash
//! costs a degraded event list rather than zero events.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::warn;

#[derive(Debug, Error)]
pub enum MeetupError {
    #[error("meetup persisted-query hash is stale — refresh via /intercept ({0})")]
    StalePersistedQuery(String),
    #[error("meetup group not found or has no events: {0}")]
    GroupNotFound(String),
    #[error("spawning node: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("decoding meetup output: {0}")]
    Decode(String),
    #[error("meetup script error: {0}")]
    Runtime(String),
}

/// The SSR reader hands through Apollo's cache verbatim, which spells an
/// absent scalar as an explicit `null`. `serde(default)` only fires on a
/// *missing* key, so null-bearing fields need this too.
fn null_to_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeetupVenue {
    #[serde(default, deserialize_with = "null_to_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub city: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub state: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetupEvent {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub status: String,
    #[serde(default)]
    pub date_time: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
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

/// Resolves + invokes `node scripts/meetup-events.mjs`, with
/// `scripts/meetup-events-ssr.mjs` as the stale-hash fallback.
pub struct MeetupClient {
    node_bin: String,
    script: PathBuf,
    ssr_script: PathBuf,
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
            ssr_script: repo_root.join("scripts/meetup-events-ssr.mjs"),
        }
    }

    /// Fetch up to `limit` upcoming events for `urlname` (the group slug).
    /// A rotated persisted-query hash falls back to the SSR reader, which
    /// returns only the ~10-12 events the page renders — degraded, not empty.
    pub async fn upcoming_events(
        &self,
        urlname: &str,
        limit: usize,
    ) -> Result<Vec<MeetupEvent>, MeetupError> {
        match self.run_script(&self.script, urlname, limit).await {
            Err(MeetupError::StalePersistedQuery(_)) => {
                warn!(
                    group = %urlname,
                    "meetup persisted-query hash is stale — falling back to the SSR reader; \
                     refresh the hash in scripts/meetup-events.mjs via /intercept"
                );
                self.run_script(&self.ssr_script, urlname, limit)
                    .await
                    .map_err(|e| match e {
                        // The group is simply gone; the hash is beside the point.
                        MeetupError::GroupNotFound(_) => e,
                        e => MeetupError::StalePersistedQuery(format!(
                            "SSR fallback also failed: {e}"
                        )),
                    })
            }
            other => other,
        }
    }

    async fn run_script(
        &self,
        script: &Path,
        urlname: &str,
        limit: usize,
    ) -> Result<Vec<MeetupEvent>, MeetupError> {
        let out = Command::new(&self.node_bin)
            .arg(script)
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
            // mjs: exit 2 = PersistedQueryNotFound, exit 3 = SSR page shape
            // changed, exit 1 = other.
            if out.status.code() == Some(2)
                || stderr.contains("PersistedQueryNotFound")
            {
                return Err(MeetupError::StalePersistedQuery(stderr.trim().to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// One event in the shape `meetup-events-ssr.mjs --json` emits.
    const SSR_PAYLOAD: &str = r#"{
        "urlname": "ai-philly",
        "kind": "upcoming",
        "totalCount": 1,
        "count": 1,
        "events": [{
            "id": "315721664",
            "title": "AI Philly Skill Share",
            "url": "https://www.meetup.com/ai-philly/events/315721664/",
            "status": "ACTIVE",
            "dateTime": "2026-08-25T17:30:00-04:00",
            "isOnline": false,
            "going": 42,
            "venue": {"name": "Blockspace Philly", "address": "215 S Broad St", "city": "Philadelphia", "state": "PA"}
        }]
    }"#;

    const STALE_HASH_STUB: &str = "console.error('PersistedQueryNotFound'); process.exit(2);";
    const PAGE_SHAPE_STUB: &str =
        "console.error('no __NEXT_DATA__ on the events page'); process.exit(3);";
    /// Writes a sentinel next to itself so a test can prove it never ran.
    const SENTINEL_STUB: &str = "import { writeFileSync } from 'node:fs';\n\
        writeFileSync(new URL('./ssr-invoked', import.meta.url), '1');\n\
        process.exit(1);";

    fn print_stub(payload: &str) -> String {
        format!("console.log(JSON.stringify({payload}));")
    }

    fn repo_with_scripts(primary: &str, ssr: &str) -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let scripts = dir.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(scripts.join("meetup-events.mjs"), primary).expect("primary stub");
        fs::write(scripts.join("meetup-events-ssr.mjs"), ssr).expect("ssr stub");
        dir
    }

    #[tokio::test]
    async fn stale_hash_falls_back_to_ssr() {
        let repo = repo_with_scripts(STALE_HASH_STUB, &print_stub(SSR_PAYLOAD));
        let events = MeetupClient::new(repo.path())
            .upcoming_events("ai-philly", 8)
            .await
            .expect("stale hash should degrade to the SSR reader, not to zero events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].url,
            "https://www.meetup.com/ai-philly/events/315721664/"
        );
    }

    #[tokio::test]
    async fn both_paths_failing_keeps_intercept_guidance() {
        let repo = repo_with_scripts(STALE_HASH_STUB, PAGE_SHAPE_STUB);
        let err = MeetupClient::new(repo.path())
            .upcoming_events("ai-philly", 8)
            .await
            .expect_err("no source left");
        let msg = err.to_string();
        assert!(msg.contains("/intercept"), "lost the refresh hint: {msg}");
        assert!(msg.contains("__NEXT_DATA__"), "lost the SSR cause: {msg}");
    }

    #[tokio::test]
    async fn primary_success_never_spawns_the_fallback() {
        let repo = repo_with_scripts(&print_stub(SSR_PAYLOAD), SENTINEL_STUB);
        let events = MeetupClient::new(repo.path())
            .upcoming_events("ai-philly", 8)
            .await
            .expect("primary path");
        assert_eq!(events.len(), 1);
        assert!(!repo.path().join("scripts/ssr-invoked").exists());
    }

    #[test]
    fn ssr_null_fields_deserialize() {
        // Apollo's cache emits explicit nulls, which `serde(default)` alone
        // rejects — the SSR path must still parse.
        let raw = r#"{"events":[{
            "id": "1",
            "title": "Skill Share",
            "url": "https://www.meetup.com/ai-philly/events/1/",
            "status": "ACTIVE",
            "dateTime": null,
            "isOnline": null,
            "going": null,
            "venue": {"name": null, "address": null, "city": null, "state": null}
        }]}"#;
        let out: MeetupCliOut = serde_json::from_str(raw).expect("null-tolerant decode");
        let venue = out.events[0].venue.as_ref().expect("venue");
        assert!(venue.name.is_empty());
        assert!(venue.city.is_empty());
        assert!(!out.events[0].is_online);
    }
}
