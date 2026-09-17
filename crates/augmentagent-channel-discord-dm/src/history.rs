//! Discord history → searchable conversation history (the Discord leg of the
//! history knowledge base, #1054).
//!
//! Pages the operator's opted-in Discord conversations through the REST API
//! and writes each message as an `emails` row (`platform = "discord"`), the
//! same shape WhatsApp and iMessage history land in, so
//! `search_conversation_history` and wiki-ask cover Discord.
//!
//! What gets read is per-operator configuration, never hardcoded:
//! - `AUGMENTAGENT_DISCORD_EXPORT_DMS=1` — DMs and group DMs
//! - `AUGMENTAGENT_DISCORD_EXPORT_GUILDS=<guild_id,…>` — every readable text
//!   channel of each listed server
//!
//! Hard rules:
//! - **Zero reasoner calls.** A sync is a knowledge-base update, never an
//!   agent task; nothing here can reach triage, drafts, or wiki ingest.
//! - Rows are inserted already processed (`digest_only`) so no sweep over
//!   unprocessed rows can ever triage history.
//! - `message_id` is the bare snowflake, identical to the live channel's rows,
//!   so a message the live channel already stored is never duplicated. For a
//!   channel with an active `priority` subscription, history stops at that
//!   subscription's `last_seen_message_id` so live triage keeps ownership of
//!   anything it hasn't seen yet.
//! - Pacing: one request at a time with a pause between requests, and the
//!   background cadence matches the live channel (4 h + jitter). Conversations
//!   whose `last_message_id` hasn't moved past the cursor cost no request.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use augmentagent_store::{
    rusqlite::OptionalExtension, Email, Store, SubscriptionMode, TriageResult,
};
use serde::Serialize;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::api::{DiscordClient, DiscordError};
use crate::types::Message;
use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

pub const ENV_EXPORT_DMS: &str = "AUGMENTAGENT_DISCORD_EXPORT_DMS";
pub const ENV_EXPORT_GUILDS: &str = "AUGMENTAGENT_DISCORD_EXPORT_GUILDS";

/// Background cadence, matching the live channel's poll fingerprint.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(crate::channel::DEFAULT_POLL_SECS);
/// Per-channel page cap for one run (100 messages per page). Large backlogs
/// finish over several runs.
pub const DEFAULT_MAX_PAGES_PER_CHANNEL: usize = 50;
const PAGE_SIZE: u32 = 100;
const NEXT_RUN_KEY: &str = "next_run_at_ms";

#[derive(Debug, Clone)]
pub struct HistoryConfig {
    pub dms: bool,
    pub guilds: Vec<String>,
    pub max_pages_per_channel: usize,
    /// Pause before every request after the first.
    pub request_pause: Duration,
}

impl HistoryConfig {
    /// `None` when the operator hasn't opted into any Discord history.
    pub fn from_env() -> Option<Self> {
        Self::parse(
            std::env::var(ENV_EXPORT_DMS).ok().as_deref(),
            std::env::var(ENV_EXPORT_GUILDS).ok().as_deref(),
        )
    }

    pub fn parse(dms: Option<&str>, guilds: Option<&str>) -> Option<Self> {
        let dms = matches!(
            dms.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
            Some("1" | "true" | "yes" | "on")
        );
        let mut guild_ids: Vec<String> = guilds
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        guild_ids.dedup();
        if !dms && guild_ids.is_empty() {
            return None;
        }
        Some(Self {
            dms,
            guilds: guild_ids,
            max_pages_per_channel: DEFAULT_MAX_PAGES_PER_CHANNEL,
            request_pause: Duration::from_millis(1200),
        })
    }
}

#[derive(Debug, Default, Serialize)]
pub struct HistoryReport {
    pub conversations: usize,
    pub unchanged: usize,
    pub fetched: usize,
    pub forbidden: usize,
    pub requests: usize,
    pub inserted: usize,
    pub already_present: usize,
    pub held_for_live_channel: usize,
    /// Channels that stopped at the page cap and continue next run.
    pub incomplete: usize,
    pub errors: usize,
}

#[derive(Debug, Clone)]
struct Target {
    channel_id: String,
    title: String,
    kind: &'static str,
    last_message_id: Option<String>,
}

/// Is a background run due? Missing state means yes.
pub fn is_due(store: &Store, now_ms: i64) -> Result<bool> {
    ensure_tables(store)?;
    let next: Option<String> = store.with_conn(|c| {
        c.query_row(
            "SELECT value FROM discord_history_meta WHERE key = ?1",
            [NEXT_RUN_KEY],
            |r| r.get(0),
        )
        .optional()
    })?;
    Ok(next
        .and_then(|v| v.parse::<i64>().ok())
        .is_none_or(|next| now_ms >= next))
}

/// Record when the next background run may start. Persisted, so daemon
/// restarts don't trigger extra syncs.
pub fn schedule_next(store: &Store, next_run_ms: i64) -> Result<()> {
    ensure_tables(store)?;
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO discord_history_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            augmentagent_store::rusqlite::params![NEXT_RUN_KEY, next_run_ms.to_string()],
        )
    })?;
    Ok(())
}

fn ensure_tables(store: &Store) -> Result<()> {
    store.with_conn(|c| {
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS discord_history_sync_state (
                channel_id TEXT PRIMARY KEY, last_message_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS discord_history_meta (
                key TEXT PRIMARY KEY, value TEXT NOT NULL
             );",
        )
    })?;
    Ok(())
}

fn cursor(store: &Store, channel_id: &str) -> Result<Option<String>> {
    Ok(store.with_conn(|c| {
        c.query_row(
            "SELECT last_message_id FROM discord_history_sync_state WHERE channel_id = ?1",
            [channel_id],
            |r| r.get(0),
        )
        .optional()
    })?)
}

fn set_cursor(store: &Store, channel_id: &str, last: &str) -> Result<()> {
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO discord_history_sync_state (channel_id, last_message_id) VALUES (?1, ?2) \
             ON CONFLICT(channel_id) DO UPDATE SET last_message_id = excluded.last_message_id",
            [channel_id, last],
        )
    })?;
    Ok(())
}

fn snowflake(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

/// One pass over every opted-in conversation.
pub async fn sync_once(
    client: &DiscordClient,
    store: &Store,
    my_user_id: &str,
    config: &HistoryConfig,
) -> Result<HistoryReport> {
    ensure_tables(store)?;
    let migrated = migrate_dm_subjects(store)?;
    if migrated > 0 {
        info!(migrated, "discord history: prefixed DM subjects");
    }
    let mut report = HistoryReport::default();
    let mut pacer = Pacer::new(config.request_pause);

    // Live priority subscriptions keep ownership of messages they haven't seen.
    let live_caps: HashMap<String, u64> = store
        .list_active_subscriptions(PLATFORM)?
        .into_iter()
        .filter(|s| s.mode == SubscriptionMode::Priority)
        .map(|s| {
            let cap = s.last_seen_message_id.as_deref().map_or(0, snowflake);
            (s.channel_id, cap)
        })
        .collect();

    let targets = match collect_targets(client, config, &mut pacer, &mut report).await {
        Ok(t) => t,
        Err(DiscordError::AuthExpired) => {
            bail!("discord auth expired — run `augmentagent discord login` to re-harvest")
        }
        Err(e) => return Err(e.into()),
    };
    report.conversations = targets.len();

    for target in targets {
        let cap = live_caps.get(&target.channel_id).copied();
        match sync_channel(
            client,
            store,
            my_user_id,
            config,
            &target,
            cap,
            &mut pacer,
            &mut report,
        )
        .await
        {
            Ok(()) => {}
            Err(DiscordError::AuthExpired) => {
                bail!("discord auth expired — run `augmentagent discord login` to re-harvest")
            }
            Err(DiscordError::RateLimited { attempts }) => {
                // Stop the whole run; cursors already saved resume next time.
                warn!(
                    attempts,
                    "discord history sync rate limited; stopping this run"
                );
                report.errors += 1;
                break;
            }
            Err(DiscordError::Server {
                status: 403 | 404, ..
            }) => {
                report.forbidden += 1;
            }
            Err(e) => {
                report.errors += 1;
                warn!(channel_id = %target.channel_id, "discord history channel failed: {e}");
            }
        }
    }
    info!(?report, "discord history sync complete");
    Ok(report)
}

async fn collect_targets(
    client: &DiscordClient,
    config: &HistoryConfig,
    pacer: &mut Pacer,
    report: &mut HistoryReport,
) -> Result<Vec<Target>, DiscordError> {
    let mut targets = Vec::new();
    if config.dms {
        pacer.wait().await;
        report.requests += 1;
        for dm in client.list_dm_channels().await? {
            targets.push(Target {
                title: dm.display_name(),
                kind: if dm.is_one_to_one() { "dm" } else { "group" },
                channel_id: dm.id,
                last_message_id: dm.last_message_id,
            });
        }
    }
    if !config.guilds.is_empty() {
        pacer.wait().await;
        report.requests += 1;
        let names: HashMap<String, String> = client
            .list_guilds()
            .await?
            .into_iter()
            .map(|g| (g.id, g.name))
            .collect();
        for guild_id in &config.guilds {
            let Some(guild_name) = names.get(guild_id) else {
                warn!(
                    guild_id,
                    "discord history: not a member of this server; skipping"
                );
                continue;
            };
            pacer.wait().await;
            report.requests += 1;
            let channels = match client.list_guild_channels(guild_id).await {
                Ok(c) => c,
                Err(DiscordError::Server {
                    status: 403 | 404, ..
                }) => {
                    report.forbidden += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            for ch in channels
                .into_iter()
                .filter(|c| matches!(c.channel_type, 0 | 5))
            {
                targets.push(Target {
                    title: format!("{guild_name} #{}", ch.name),
                    kind: "guild_channel",
                    channel_id: ch.id,
                    last_message_id: ch.last_message_id,
                });
            }
        }
    }
    Ok(targets)
}

#[allow(clippy::too_many_arguments)]
async fn sync_channel(
    client: &DiscordClient,
    store: &Store,
    my_user_id: &str,
    config: &HistoryConfig,
    target: &Target,
    live_cap: Option<u64>,
    pacer: &mut Pacer,
    report: &mut HistoryReport,
) -> Result<(), DiscordError> {
    let store_err = |e: anyhow::Error| DiscordError::Server {
        status: 0,
        body: format!("store: {e:#}"),
    };
    let saved = cursor(store, &target.channel_id).map_err(store_err)?;
    let saved_n = saved.as_deref().map_or(0, snowflake);
    // Empty conversation, or nothing newer than what we already have.
    match target.last_message_id.as_deref().map(snowflake) {
        None => {
            report.unchanged += 1;
            return Ok(());
        }
        Some(last) if last <= saved_n => {
            report.unchanged += 1;
            return Ok(());
        }
        _ => {}
    }
    if live_cap == Some(0) {
        // Live priority channel hasn't polled yet; leave it all to live triage.
        report.held_for_live_channel += 1;
        return Ok(());
    }
    report.fetched += 1;

    let mut after = saved.unwrap_or_else(|| "0".to_string());
    for page in 0.. {
        if page == config.max_pages_per_channel {
            report.incomplete += 1;
            break;
        }
        pacer.wait().await;
        report.requests += 1;
        let mut messages = client
            .fetch_messages(&target.channel_id, Some(&after), PAGE_SIZE)
            .await?;
        let full_page = messages.len() == PAGE_SIZE as usize;
        messages.sort_by_key(|m| snowflake(&m.id));

        let mut hit_cap = false;
        let mut newest: Option<String> = None;
        for msg in &messages {
            if live_cap.is_some_and(|cap| snowflake(&msg.id) > cap) {
                hit_cap = true;
                report.held_for_live_channel += 1;
                break;
            }
            newest = Some(msg.id.clone());
            if !matches!(msg.message_type, 0 | 19) {
                continue;
            }
            let email = history_email(msg, target, my_user_id);
            let first_seen = parse_ts_ms(&msg.timestamp);
            let inserted = store
                .upsert_email_backfill(&email, first_seen)
                .map_err(|e| store_err(e.into()))?;
            if inserted {
                store
                    .mark_email_processed(&email.message_id, TriageResult::DigestOnly)
                    .map_err(|e| store_err(e.into()))?;
                report.inserted += 1;
            } else {
                report.already_present += 1;
            }
        }
        if let Some(n) = &newest {
            set_cursor(store, &target.channel_id, n).map_err(store_err)?;
            after = n.clone();
        }
        if hit_cap || !full_page || newest.is_none() {
            if !hit_cap {
                // Caught up. When the conversation's reported last message
                // wasn't returned (deleted), advance to it anyway so later
                // runs see the conversation as unchanged instead of
                // re-fetching an empty page every time.
                let reported = target.last_message_id.as_deref().map_or(0, snowflake);
                let reported = live_cap.map_or(reported, |cap| reported.min(cap));
                if reported > snowflake(&after) {
                    set_cursor(store, &target.channel_id, &reported.to_string())
                        .map_err(store_err)?;
                }
            }
            break;
        }
    }
    Ok(())
}

/// The history row for one message. `message_id`/`thread_id`/`from` match the
/// live channel's shape; `subject` carries conversation + speaker so keyword
/// search finds them, as the WhatsApp rows do.
fn history_email(msg: &Message, target: &Target, my_user_id: &str) -> Email {
    let speaker = if msg.author.id == my_user_id {
        "me".to_string()
    } else {
        msg.author.display_label()
    };
    let mut body = msg.content.clone();
    for a in &msg.attachments {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!(
            "[attachment: {} {} {}]",
            a.content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
            a.filename,
            a.url
        ));
    }
    Email {
        message_id: msg.id.clone(),
        thread_id: Some(msg.channel_id.clone()),
        from: format!("{speaker} <discord:{}>", msg.author.id),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: format!(
            "{} {} [{speaker}]",
            subject_prefix(target.kind),
            target.title
        ),
        body,
        date: msg.timestamp.clone(),
        account_entity_id: Some(format!("{ACCOUNT_ENTITY_ID_PREFIX}:{my_user_id}")),
        platform: PLATFORM.to_string(),
        kind: target.kind.to_string(),
    }
}

/// Search matches subject text, so the prefix is how the agent tells DMs from
/// server channels (`keyword: "Discord DM"`).
fn subject_prefix(kind: &str) -> &'static str {
    match kind {
        "dm" => "Discord DM:",
        "group" => "Discord group DM:",
        _ => "Discord:",
    }
}

/// Rows written before DM subjects carried a prefix. Idempotent: rewritten
/// subjects no longer match `Discord: %`.
fn migrate_dm_subjects(store: &Store) -> Result<usize> {
    Ok(store.with_conn(|c| {
        c.execute(
            "UPDATE emails SET subject = CASE kind \
                WHEN 'dm' THEN 'Discord DM: ' || substr(subject, 10) \
                ELSE 'Discord group DM: ' || substr(subject, 10) END \
             WHERE platform = 'discord' AND kind IN ('dm', 'group') \
               AND subject LIKE 'Discord: %'",
            [],
        )
    })?)
}

fn parse_ts_ms(ts: &str) -> i64 {
    time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        .unwrap_or(0)
}

struct Pacer {
    pause: Duration,
    first: bool,
}

impl Pacer {
    fn new(pause: Duration) -> Self {
        Self { pause, first: true }
    }

    async fn wait(&mut self) {
        if std::mem::take(&mut self.first) {
            return;
        }
        if !self.pause.is_zero() {
            sleep(self.pause).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::DiscordAuth;
    use serde_json::json;

    fn auth() -> DiscordAuth {
        DiscordAuth {
            user_id: "900".into(),
            token: "test-token-value-long-enough".into(), // pii-ok: synthetic fixture
            super_properties_b64: "<SUPER_PROPERTIES_BASE64>".into(),
            user_agent: "Mozilla/5.0 test".into(),
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    fn cfg_dms() -> HistoryConfig {
        let mut c = HistoryConfig::parse(Some("1"), None).unwrap();
        c.request_pause = Duration::ZERO;
        c
    }

    fn msg(id: u64, author: &str, content: &str) -> serde_json::Value {
        json!({
            "id": id.to_string(), "channel_id": "10", "content": content,
            "timestamp": "2026-09-01T12:00:00.000000+00:00", "type": 0,
            "author": {"id": author, "username": format!("user{author}")},
            "attachments": []
        })
    }

    fn dm_list(last: &str) -> String {
        json!([{"id": "10", "type": 1, "last_message_id": last,
                "recipients": [{"id": "500", "username": "alice", "global_name": "Alice"}]}])
        .to_string()
    }

    fn row_count(store: &Store) -> i64 {
        store
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0)))
            .unwrap()
    }

    #[test]
    fn config_requires_explicit_opt_in() {
        assert!(HistoryConfig::parse(None, None).is_none());
        assert!(HistoryConfig::parse(Some("0"), Some(" , ")).is_none());
        let dms = HistoryConfig::parse(Some("true"), None).unwrap();
        assert!(dms.dms && dms.guilds.is_empty());
        let g = HistoryConfig::parse(None, Some("111, 222")).unwrap();
        assert!(!g.dms);
        assert_eq!(g.guilds, vec!["111", "222"]);
    }

    #[tokio::test]
    async fn backfills_pages_marks_processed_and_is_idempotent() {
        let mut server = mockito::Server::new_async().await;
        let _dms = server
            .mock("GET", "/users/@me/channels")
            .with_body(dm_list("250"))
            .expect(2)
            .create_async()
            .await;
        let page1: Vec<_> = (101..=200).rev().map(|i| msg(i, "500", "hi")).collect();
        let _p1 = server
            .mock("GET", "/channels/10/messages?limit=100&after=0")
            .with_body(json!(page1).to_string())
            .expect(1)
            .create_async()
            .await;
        let mut page2: Vec<_> = (201..=250).map(|i| msg(i, "500", "later")).collect();
        page2.push(msg(260, "900", "my own reply"));
        let _p2 = server
            .mock("GET", "/channels/10/messages?limit=100&after=200")
            .with_body(json!(page2).to_string())
            .expect(1)
            .create_async()
            .await;

        let client = DiscordClient::with_base_url(auth(), server.url());
        let (_d, store) = store();
        let report = sync_once(&client, &store, "900", &cfg_dms()).await.unwrap();
        assert_eq!(report.inserted, 151);
        assert_eq!(report.requests, 3);
        assert_eq!(row_count(&store), 151);

        let (subject, from, processed, kind): (String, String, Option<String>, String) = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT subject, fromEmail, triageResult, kind FROM emails WHERE messageId = '260'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
            })
            .unwrap();
        assert_eq!(subject, "Discord DM: Alice [me]");
        assert_eq!(from, "me <discord:900>");
        assert_eq!(processed.as_deref(), Some("digest_only"));
        assert_eq!(kind, "dm");

        // Second run: the DM's last_message_id (250) is not past the cursor
        // (260) → no message fetch, only the DM list.
        let report = sync_once(&client, &store, "900", &cfg_dms()).await.unwrap();
        assert_eq!(report.inserted, 0);
        assert_eq!(report.requests, 1);
        assert_eq!(row_count(&store), 151);
    }

    #[tokio::test]
    async fn deleted_last_message_does_not_refetch_every_run() {
        let mut server = mockito::Server::new_async().await;
        // Discord still reports 99 as last_message_id, but 99 was deleted.
        let _dms = server
            .mock("GET", "/users/@me/channels")
            .with_body(dm_list("99"))
            .expect(2)
            .create_async()
            .await;
        let _p = server
            .mock("GET", "/channels/10/messages?limit=100&after=0")
            .with_body(json!([msg(50, "500", "still here")]).to_string())
            .expect(1)
            .create_async()
            .await;
        let _stuck = server
            .mock("GET", "/channels/10/messages?limit=100&after=50")
            .with_body("[]")
            .expect(0)
            .create_async()
            .await;
        let client = DiscordClient::with_base_url(auth(), server.url());
        let (_d, store) = store();
        let first = sync_once(&client, &store, "900", &cfg_dms()).await.unwrap();
        assert_eq!((first.inserted, first.requests), (1, 2));
        assert_eq!(cursor(&store, "10").unwrap().as_deref(), Some("99"));
        let second = sync_once(&client, &store, "900", &cfg_dms()).await.unwrap();
        assert_eq!(
            (second.unchanged, second.fetched, second.requests),
            (1, 0, 1)
        );
    }

    #[tokio::test]
    async fn live_row_is_not_duplicated_or_reprocessed() {
        let mut server = mockito::Server::new_async().await;
        let _dms = server
            .mock("GET", "/users/@me/channels")
            .with_body(dm_list("2"))
            .create_async()
            .await;
        let _p = server
            .mock("GET", "/channels/10/messages?limit=100&after=0")
            .with_body(json!([msg(2, "500", "b"), msg(1, "500", "a")]).to_string())
            .create_async()
            .await;
        let (_d, store) = store();
        // The live channel stored message 1 already, untriaged.
        let live = Email {
            message_id: "1".into(),
            thread_id: Some("10".into()),
            from: "Alice <discord:500>".into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: String::new(),
            body: "a".into(),
            date: "2026-09-01T12:00:00+00:00".into(),
            account_entity_id: None,
            platform: "discord".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&live).unwrap();
        let client = DiscordClient::with_base_url(auth(), server.url());
        let report = sync_once(&client, &store, "900", &cfg_dms()).await.unwrap();
        assert_eq!((report.inserted, report.already_present), (1, 1));
        assert_eq!(row_count(&store), 2);
        let triage: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT triageResult FROM emails WHERE messageId='1'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(triage, None, "history must not mark a live row processed");
    }

    #[tokio::test]
    async fn priority_subscription_keeps_unseen_messages_for_live_triage() {
        let mut server = mockito::Server::new_async().await;
        let _dms = server
            .mock("GET", "/users/@me/channels")
            .with_body(dm_list("30"))
            .create_async()
            .await;
        let _p = server
            .mock("GET", "/channels/10/messages?limit=100&after=0")
            .with_body(
                json!([
                    msg(30, "500", "new"),
                    msg(20, "500", "seen"),
                    msg(10, "500", "old")
                ])
                .to_string(),
            )
            .create_async()
            .await;
        let (_d, store) = store();
        let sub = store
            .upsert_subscription("discord", "10", "alice", SubscriptionMode::Priority, None)
            .unwrap();
        store.update_last_seen_message(&sub.id, "20").unwrap();
        let client = DiscordClient::with_base_url(auth(), server.url());
        let report = sync_once(&client, &store, "900", &cfg_dms()).await.unwrap();
        assert_eq!(report.inserted, 2);
        assert_eq!(report.held_for_live_channel, 1);
        assert_eq!(cursor(&store, "10").unwrap().as_deref(), Some("20"));
    }

    #[tokio::test]
    async fn guild_allowlist_reads_only_listed_servers_and_skips_forbidden_channels() {
        let mut server = mockito::Server::new_async().await;
        let _g = server
            .mock("GET", "/users/@me/guilds")
            .with_body(
                json!([{"id": "1", "name": "Allowed"}, {"id": "2", "name": "Other"}]).to_string(),
            )
            .create_async()
            .await;
        let _c = server
            .mock("GET", "/guilds/1/channels")
            .with_body(
                json!([
                    {"id": "40", "name": "general", "type": 0, "last_message_id": "5"},
                    {"id": "41", "name": "private", "type": 0, "last_message_id": "9"},
                    {"id": "42", "name": "voice", "type": 2}
                ])
                .to_string(),
            )
            .create_async()
            .await;
        let _other = server
            .mock("GET", "/guilds/2/channels")
            .expect(0)
            .create_async()
            .await;
        let _m40 = server
            .mock("GET", "/channels/40/messages?limit=100&after=0")
            .with_body(json!([msg(5, "500", "ship it")]).to_string())
            .create_async()
            .await;
        let _m41 = server
            .mock("GET", "/channels/41/messages?limit=100&after=0")
            .with_status(403)
            .with_body("{\"message\":\"Missing Access\"}")
            .create_async()
            .await;
        let (_d, store) = store();
        let mut cfg = HistoryConfig::parse(None, Some("1")).unwrap();
        cfg.request_pause = Duration::ZERO;
        let client = DiscordClient::with_base_url(auth(), server.url());
        let report = sync_once(&client, &store, "900", &cfg).await.unwrap();
        assert_eq!(report.conversations, 2);
        assert_eq!(report.inserted, 1);
        assert_eq!(report.forbidden, 1);
        let (subject, kind): (String, String) = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT subject, kind FROM emails WHERE messageId='5'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(subject, "Discord: Allowed #general [user500]");
        assert_eq!(kind, "guild_channel");
    }

    #[test]
    fn legacy_dm_subjects_are_migrated_once() {
        let (_d, store) = store();
        for (id, kind, subject) in [
            ("1", "dm", "Discord: Alice [me]"),
            ("2", "group", "Discord: Alice, Bob [Bob]"),
            ("3", "guild_channel", "Discord: Server #general [Bob]"),
        ] {
            store
                .with_conn(|c| {
                    c.execute(
                        "INSERT INTO emails (messageId, fromEmail, subject, firstSeenAt, platform, kind) \
                         VALUES (?1, 'x', ?2, 1, 'discord', ?3)",
                        [id, subject, kind],
                    )
                })
                .unwrap();
        }
        assert_eq!(migrate_dm_subjects(&store).unwrap(), 2);
        assert_eq!(migrate_dm_subjects(&store).unwrap(), 0);
        let subjects: Vec<String> = store
            .with_conn(|c| {
                let mut st = c.prepare("SELECT subject FROM emails ORDER BY messageId")?;
                let rows = st.query_map([], |r| r.get(0))?;
                rows.collect()
            })
            .unwrap();
        assert_eq!(
            subjects,
            [
                "Discord DM: Alice [me]",
                "Discord group DM: Alice, Bob [Bob]",
                "Discord: Server #general [Bob]"
            ]
        );
    }

    #[test]
    fn schedule_survives_and_gates_runs() {
        let (_d, store) = store();
        assert!(is_due(&store, 1_000).unwrap());
        schedule_next(&store, 5_000).unwrap();
        assert!(!is_due(&store, 4_999).unwrap());
        assert!(is_due(&store, 5_000).unwrap());
    }
}
