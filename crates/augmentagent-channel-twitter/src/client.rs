//! #79 — X / Twitter posting client.
//!
//! `CreateTweetClient::create(text, reply_to, media)` posts an original tweet
//! or a reply through the `CreateTweet` GraphQL op. Three guards before any
//! network call:
//!
//! 1. **Hard 15/day quota preflight.** X's free tier publishes ~2400
//!    posts/day, but we keep the agent's own automated posting to a hard
//!    ceiling of 15/day so the user owns the rest of the budget — separate
//!    from (and stricter than) the #83 RateGovernor soft caps. Counted from
//!    the `twitter_post_log` store table over a rolling 24h window.
//! 2. **Static queryId fallback chain.** Cached store id → env override →
//!    a static known-good default. X rotates the hashed queryId every few
//!    weeks; the chain recovers without a redeploy.
//! 3. **Dry-run gate.** When `dry_run` is set, nothing hits the wire; the
//!    intent is logged with status `dry_run` (excluded from the quota count).
//!
//! Phases 2-5 (media upload, native threads, browser-fallback posting) are
//! explicitly deferred — `media` is accepted but a non-empty list is
//! rejected with `Unsupported` rather than silently dropped.

use std::sync::Arc;

use thiserror::Error;
use uuid::Uuid;

use augmentagent_store::{rusqlite, Store};

use crate::api::{
    QueryIdResolver, TwitterApi, TwitterError, DEFAULT_CREATE_TWEET_QUERY_ID,
    DEFAULT_USER_TWEETS_QUERY_ID,
};

/// sqlite `config` key the CLI's `augmentagent channel twitter arm/disarm`
/// verb writes (issue #7). Same key the dashboard's `getConfigStatus` and the
/// status aggregator (`augmentagent-cli::status`) read, so all three agree on
/// "armed".
pub const ARMED_CONFIG_KEY: &str = "twitter_real_enabled";

/// Env-var fallback when the sqlite row is absent. Matches the
/// `_REAL_ENABLED` naming Instagram (`INSTAGRAM_REAL_ACCOUNT_ENABLED`) and
/// WhatsApp (`AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED`) use.
pub const ARMED_ENV_KEY: &str = "AUGMENTAGENT_TWITTER_REAL_ENABLED";

/// Boolean parse mirroring `channel_router::is_truthy` in the CLI: empty /
/// `0` / `false` / `off` / `no` (case-insensitive) ⇒ false; anything else
/// non-empty ⇒ true. Centralized here so the gate's accept set never drifts
/// from what `arm` writes (`"true"`) or what status surfaces.
fn is_truthy(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    !matches!(
        s.to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Is the live-send code path armed? Sqlite `config` row wins on conflict
/// (matches `getConfigStatus()` precedence in `src/dashboard.ts:78` and the
/// CLI's `armed_from` in `augmentagent-cli/src/status.rs`); env var is the
/// fallback when no row exists, so a one-shot operator run with the env set
/// still works. A sqlite read error (e.g. table absent on a fresh box) falls
/// through to the env check rather than failing closed loudly — the env path
/// is the documented manual override.
///
/// This mirrors how Instagram and WhatsApp gate their live sends (see
/// `crates/augmentagent-channel-instagram/src/composer.rs::real_account_enabled`
/// and `crates/augmentagent-channel-whatsapp/src/channel.rs::CONTROL_ENABLED_ENV`).
pub fn twitter_armed(store: &Store) -> bool {
    use rusqlite::OptionalExtension;
    let from_sqlite: Option<String> = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM config WHERE key = ?1",
                rusqlite::params![ARMED_CONFIG_KEY],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .ok()
        .flatten();
    if let Some(v) = from_sqlite {
        return is_truthy(&v);
    }
    std::env::var(ARMED_ENV_KEY)
        .ok()
        .map(|v| is_truthy(&v))
        .unwrap_or(false)
}

/// The store-backed [`QueryIdResolver`]: full cache → env → static-default
/// chain, in trust order. Injected into [`crate::api::TwitterClient`] via
/// `with_resolver` so the live API client survives an X queryId rotation —
/// it walks every candidate before surfacing
/// [`TwitterError::QueryIdRotated`], at which point the operator captures a
/// fresh id (see docs/twitter-protocol.md runbook) and `put_twitter_query_id`
/// caches it for the next boot with no recompile.
pub struct StoreQueryIdResolver {
    store: Arc<Store>,
}

impl StoreQueryIdResolver {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl QueryIdResolver for StoreQueryIdResolver {
    fn candidates(&self, op: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push = |v: String, out: &mut Vec<String>| {
            if !v.is_empty() && !out.contains(&v) {
                out.push(v);
            }
        };
        // 1. store cache (operation name as stored, e.g. `CreateTweet`)
        if let Ok(Some(qid)) = self.store.twitter_query_id(op) {
            push(qid, &mut out);
        }
        // 2. env override (`AUGMENTAGENT_TWITTER_<OP>_QUERY_ID`)
        let env_key = format!(
            "AUGMENTAGENT_TWITTER_{}_QUERY_ID",
            op.chars()
                .enumerate()
                .flat_map(|(i, c)| {
                    if c.is_ascii_uppercase() && i != 0 {
                        vec!['_', c]
                    } else {
                        vec![c.to_ascii_uppercase()]
                    }
                })
                .collect::<String>()
        );
        if let Ok(v) = std::env::var(&env_key) {
            push(v, &mut out);
        }
        // 3. static default
        let default = match op {
            "UserTweets" => DEFAULT_USER_TWEETS_QUERY_ID,
            "CreateTweet" => DEFAULT_CREATE_TWEET_QUERY_ID,
            _ => "",
        };
        push(default.to_string(), &mut out);
        out
    }
}

/// Hard daily ceiling on agent-initiated posts (#79). Deliberately far below
/// X's published free-tier cap — the user should own most of their own
/// posting budget.
pub const DAILY_POST_QUOTA: u32 = 15;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Error)]
pub enum PostError {
    #[error("daily post quota exhausted: {used}/{cap} in last 24h")]
    QuotaExhausted { used: u32, cap: u32 },
    #[error("media attachments not supported yet (phase 2 deferred)")]
    Unsupported,
    #[error("empty tweet text")]
    Empty,
    #[error("tweet too long: {0} > 280 chars")]
    TooLong(usize),
    #[error("api: {0}")]
    Api(#[from] TwitterError),
    #[error("store: {0}")]
    Store(String),
}

/// Outcome of a `create` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// Posted live; carries the new tweet's rest_id.
    Posted(String),
    /// Dry-run: nothing sent, intent logged.
    DryRun,
}

pub struct CreateTweetClient<A: TwitterApi> {
    api: Arc<A>,
    store: Arc<Store>,
    dry_run: bool,
}

impl<A: TwitterApi> CreateTweetClient<A> {
    pub fn new(api: Arc<A>, store: Arc<Store>, dry_run: bool) -> Self {
        Self {
            api,
            store,
            dry_run,
        }
    }

    /// Resolve the `CreateTweet` queryId via the fallback chain:
    /// store cache → env override → static default.
    pub fn resolve_query_id(&self) -> String {
        if let Ok(Some(qid)) = self.store.twitter_query_id("CreateTweet") {
            if !qid.is_empty() {
                return qid;
            }
        }
        if let Ok(qid) = std::env::var("AUGMENTAGENT_TWITTER_CREATE_TWEET_QUERY_ID") {
            if !qid.is_empty() {
                return qid;
            }
        }
        DEFAULT_CREATE_TWEET_QUERY_ID.to_string()
    }

    /// How many quota-burning posts remain in the rolling 24h window.
    pub fn quota_remaining(&self, now_ms: i64) -> Result<u32, PostError> {
        let used = self
            .store
            .twitter_post_count_in_window(now_ms - DAY_MS, now_ms)
            .map_err(|e| PostError::Store(e.to_string()))?;
        Ok(DAILY_POST_QUOTA.saturating_sub(used))
    }

    /// Post an original tweet or reply.
    ///
    /// - `text` — tweet body (1..=280 chars).
    /// - `reply_to` — when `Some(id)`, post as a reply to that tweet.
    /// - `media` — deferred; a non-empty slice is rejected.
    pub async fn create(
        &self,
        text: &str,
        reply_to: Option<&str>,
        media: &[String],
    ) -> Result<PostOutcome, PostError> {
        if !media.is_empty() {
            return Err(PostError::Unsupported);
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(PostError::Empty);
        }
        // X counts by grapheme-ish; a char count is a safe over-approximation
        // for the preflight (real send still validates server-side).
        let len = trimmed.chars().count();
        if len > 280 {
            return Err(PostError::TooLong(len));
        }

        let now_ms = now_millis();
        let used = self
            .store
            .twitter_post_count_in_window(now_ms - DAY_MS, now_ms)
            .map_err(|e| PostError::Store(e.to_string()))?;
        if used >= DAILY_POST_QUOTA {
            return Err(PostError::QuotaExhausted {
                used,
                cap: DAILY_POST_QUOTA,
            });
        }

        let kind = if reply_to.is_some() { "reply" } else { "tweet" };
        let log_id = Uuid::new_v4().to_string();

        if self.dry_run {
            // Dry-run rows are excluded from the quota count by the store.
            self.store
                .log_twitter_post(
                    &log_id, kind, reply_to, "dry_run", None, now_ms, None,
                )
                .map_err(|e| PostError::Store(e.to_string()))?;
            tracing::info!(
                kind, reply_to = ?reply_to, "twitter post dry-run (nothing sent)"
            );
            return Ok(PostOutcome::DryRun);
        }

        // Issue #32 — arming gate. The CLI's `augmentagent channel twitter
        // arm/disarm` writes `twitter_real_enabled` to the sqlite `config`
        // table (issue #7); the env var `AUGMENTAGENT_TWITTER_REAL_ENABLED`
        // is the documented manual override. If neither is truthy we log
        // and skip the live wire call — same shape as Instagram's
        // `INSTAGRAM_REAL_ACCOUNT_ENABLED` gate and WhatsApp's
        // `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED` gate. Re-uses the dry-run
        // outcome variant so the existing caller in `main.rs` (which keys off
        // `PostOutcome::DryRun` ⇒ `PublishOutcome::DryRun`) treats a
        // disarmed skip as a clean "nothing sent" rather than a posted
        // success or an error. `--allow-live` (`validate`) is intentionally
        // NOT gated here — it's a per-call operator override.
        if !twitter_armed(&self.store) {
            tracing::info!(
                kind,
                reply_to = ?reply_to,
                config_key = ARMED_CONFIG_KEY,
                env_key = ARMED_ENV_KEY,
                "twitter send skipped: not armed"
            );
            return Ok(PostOutcome::DryRun);
        }

        // Live send. `reply_to_tweet` covers both cases — for an original
        // tweet we pass an empty parent (the API client treats reply as
        // optional via the variables map; here we only support reply +
        // original through the same op).
        let result = match reply_to {
            Some(parent) => self.api.reply_to_tweet(parent, trimmed).await,
            None => self.api.reply_to_tweet("", trimmed).await,
        };

        match result {
            Ok(tweet_id) => {
                self.store
                    .log_twitter_post(
                        &log_id,
                        kind,
                        reply_to,
                        "ok",
                        Some(&tweet_id),
                        now_ms,
                        None,
                    )
                    .map_err(|e| PostError::Store(e.to_string()))?;
                tracing::info!(tweet_id, kind, "twitter post sent");
                Ok(PostOutcome::Posted(tweet_id))
            }
            Err(e) => {
                // A failed live attempt still burns the platform quota.
                let meta = serde_json::json!({ "error": e.to_string() }).to_string();
                let _ = self.store.log_twitter_post(
                    &log_id,
                    kind,
                    reply_to,
                    "failed",
                    None,
                    now_ms,
                    Some(&meta),
                );
                Err(PostError::Api(e))
            }
        }
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tweet, TwitterDm};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct StubApi {
        sent: Mutex<Vec<(String, String)>>,
        fail: bool,
    }
    impl StubApi {
        fn ok() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }
    #[async_trait]
    impl TwitterApi for StubApi {
        async fn fetch_user_tweets(
            &self,
            _u: &str,
            _s: Option<&str>,
        ) -> Result<Vec<Tweet>, TwitterError> {
            Ok(vec![])
        }
        async fn reply_to_tweet(
            &self,
            tweet_id: &str,
            text: &str,
        ) -> Result<String, TwitterError> {
            if self.fail {
                return Err(TwitterError::AuthExpired);
            }
            self.sent
                .lock()
                .unwrap()
                .push((tweet_id.into(), text.into()));
            Ok("1900000000000000001".into())
        }
        async fn fetch_dm_inbox(
            &self,
            _c: Option<&str>,
        ) -> Result<Vec<TwitterDm>, TwitterError> {
            Ok(vec![])
        }
        async fn send_dm(
            &self,
            _c: &str,
            _t: &str,
        ) -> Result<String, TwitterError> {
            Ok("evt".into())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(file.path()).unwrap();
            // Minimal pre-existing schema the migration ALTERs expect.
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    /// Flip the issue-#32 arming row on. Mirrors `flip_armed` in
    /// `augmentagent-cli::channel_router` (writes the same `config` row the
    /// dashboard reads) so the gate sees an armed channel and the live-send
    /// code path runs end-to-end in unit tests. Creates the table on demand
    /// because `tmp_store`'s minimal schema doesn't include `config`.
    fn arm_twitter(store: &Store) {
        store
            .with_conn(|conn| {
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS config (\
                         key TEXT PRIMARY KEY,\
                         value TEXT NOT NULL,\
                         updatedAt INTEGER NOT NULL\
                     )",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO config (key, value, updatedAt) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![ARMED_CONFIG_KEY, "true", 0_i64],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn dry_run_logs_but_does_not_send() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api.clone(), store.clone(), true);
        let out = c.create("hello world", None, &[]).await.unwrap();
        assert_eq!(out, PostOutcome::DryRun);
        assert!(api.sent.lock().unwrap().is_empty());
        // dry-run rows don't burn quota.
        assert_eq!(c.quota_remaining(now_millis()).unwrap(), DAILY_POST_QUOTA);
    }

    #[tokio::test]
    async fn live_send_posts_and_burns_quota() {
        let (store, _f) = tmp_store();
        arm_twitter(&store);
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api.clone(), store.clone(), false);
        let out = c.create("hi", Some("123"), &[]).await.unwrap();
        assert_eq!(out, PostOutcome::Posted("1900000000000000001".into()));
        assert_eq!(api.sent.lock().unwrap().len(), 1);
        assert_eq!(api.sent.lock().unwrap()[0].0, "123");
        assert_eq!(c.quota_remaining(now_millis()).unwrap(), DAILY_POST_QUOTA - 1);
    }

    #[tokio::test]
    async fn quota_exhaustion_blocks_send() {
        let (store, _f) = tmp_store();
        let now = now_millis();
        for i in 0..DAILY_POST_QUOTA {
            store
                .log_twitter_post(
                    &format!("p{i}"),
                    "tweet",
                    None,
                    "ok",
                    Some("t"),
                    now,
                    None,
                )
                .unwrap();
        }
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api.clone(), store, false);
        let err = c.create("blocked", None, &[]).await.unwrap_err();
        assert!(matches!(
            err,
            PostError::QuotaExhausted { used, cap }
                if used == DAILY_POST_QUOTA && cap == DAILY_POST_QUOTA
        ));
        assert!(api.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn old_posts_outside_window_dont_count() {
        let (store, _f) = tmp_store();
        let now = now_millis();
        // 25h ago — outside the rolling 24h window.
        for i in 0..DAILY_POST_QUOTA {
            store
                .log_twitter_post(
                    &format!("old{i}"),
                    "tweet",
                    None,
                    "ok",
                    Some("t"),
                    now - 25 * 60 * 60 * 1000,
                    None,
                )
                .unwrap();
        }
        arm_twitter(&store);
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api, store, false);
        assert_eq!(c.quota_remaining(now).unwrap(), DAILY_POST_QUOTA);
        assert!(c.create("fresh", None, &[]).await.is_ok());
    }

    #[tokio::test]
    async fn media_is_rejected_not_dropped() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api, store, true);
        let err = c
            .create("with pic", None, &["/tmp/a.png".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, PostError::Unsupported));
    }

    #[tokio::test]
    async fn empty_and_overlong_rejected() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api, store, true);
        assert!(matches!(
            c.create("   ", None, &[]).await.unwrap_err(),
            PostError::Empty
        ));
        let long = "x".repeat(281);
        assert!(matches!(
            c.create(&long, None, &[]).await.unwrap_err(),
            PostError::TooLong(281)
        ));
    }

    #[tokio::test]
    async fn failed_live_send_still_burns_quota() {
        let (store, _f) = tmp_store();
        arm_twitter(&store);
        let api = Arc::new(StubApi::failing());
        let c = CreateTweetClient::new(api, store.clone(), false);
        let err = c.create("will fail", None, &[]).await.unwrap_err();
        assert!(matches!(err, PostError::Api(_)));
        // The failed attempt logged a row that burns quota.
        assert_eq!(
            store
                .twitter_post_count_in_window(now_millis() - DAY_MS, now_millis())
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn query_id_resolution_chain() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api, store.clone(), true);
        // Default when nothing cached / no env.
        std::env::remove_var("AUGMENTAGENT_TWITTER_CREATE_TWEET_QUERY_ID");
        assert_eq!(c.resolve_query_id(), DEFAULT_CREATE_TWEET_QUERY_ID);
        // Env override beats default.
        std::env::set_var("AUGMENTAGENT_TWITTER_CREATE_TWEET_QUERY_ID", "ENVID");
        assert_eq!(c.resolve_query_id(), "ENVID");
        // Store cache beats env.
        store
            .put_twitter_query_id("CreateTweet", "STOREID", now_millis())
            .unwrap();
        assert_eq!(c.resolve_query_id(), "STOREID");
        std::env::remove_var("AUGMENTAGENT_TWITTER_CREATE_TWEET_QUERY_ID");
    }

    // -----------------------------------------------------------------------
    // Issue #32 — arming-gate tests.
    // -----------------------------------------------------------------------

    /// Cargo runs `#[test]` fns concurrently per-process; the arming env var
    /// is process-global so any test that reads or mutates it must hold this
    /// guard for the duration to avoid races with peers. Poisoned-lock
    /// recovery: another test panicking inside the section doesn't make a
    /// later one falsely pass on a leaked env.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Armed=true (sqlite `config` row written by the CLI's `arm` verb) ⇒
    /// the live path runs end-to-end: the stub API records the call and the
    /// quota log advances. Mirrors `live_send_posts_and_burns_quota` but
    /// states the arming dependency explicitly.
    #[tokio::test]
    async fn armed_via_sqlite_runs_live_send() {
        let _g = env_lock();
        let (store, _f) = tmp_store();
        arm_twitter(&store);
        // Belt-and-suspenders: env unset so the sqlite branch is what's
        // exercised (otherwise the env fallback could mask a broken sqlite
        // read).
        std::env::remove_var(ARMED_ENV_KEY);
        assert!(twitter_armed(&store), "sqlite row must arm");
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api.clone(), store.clone(), false);
        let out = c.create("armed live", None, &[]).await.unwrap();
        assert_eq!(out, PostOutcome::Posted("1900000000000000001".into()));
        assert_eq!(api.sent.lock().unwrap().len(), 1);
    }

    /// Armed=false (no sqlite row, no env) ⇒ the live path is skipped:
    /// the stub API is never called, the quota log stays empty (so a
    /// disarmed channel can't burn the daily budget), and the caller sees
    /// `PostOutcome::DryRun` rather than an error — matching the dry-run
    /// shape so the main.rs publisher routes it to `PublishOutcome::DryRun`.
    #[tokio::test]
    async fn disarmed_skips_live_send_and_does_not_burn_quota() {
        let _g = env_lock();
        let (store, _f) = tmp_store();
        std::env::remove_var(ARMED_ENV_KEY);
        assert!(!twitter_armed(&store), "fresh store must not be armed");
        let api = Arc::new(StubApi::ok());
        let c = CreateTweetClient::new(api.clone(), store.clone(), false);
        let out = c.create("would-be live", Some("999"), &[]).await.unwrap();
        assert_eq!(out, PostOutcome::DryRun);
        assert!(
            api.sent.lock().unwrap().is_empty(),
            "disarmed channel must not call the wire"
        );
        // No row logged ⇒ no quota burned by a disarmed skip.
        let used = store
            .twitter_post_count_in_window(now_millis() - DAY_MS, now_millis())
            .unwrap();
        assert_eq!(used, 0);
    }

    /// Env-var fallback arms the channel when no sqlite row exists. Mirrors
    /// the per-channel `_REAL_ENABLED` env override Instagram/WhatsApp expose
    /// so an operator can flip live mode for a single run without writing the
    /// config row. Sqlite still wins on conflict — verified separately below.
    #[tokio::test]
    async fn armed_via_env_var_when_sqlite_row_missing() {
        let _g = env_lock();
        let (store, _f) = tmp_store();
        std::env::set_var(ARMED_ENV_KEY, "true");
        // No sqlite row in the fresh store.
        assert!(twitter_armed(&store));
        std::env::remove_var(ARMED_ENV_KEY);
        assert!(!twitter_armed(&store));
    }

    /// Sqlite row wins on conflict — disarm via `augmentagent channel twitter
    /// disarm` must shut off live sending even if the operator left the env
    /// var set on the box. Matches `getConfigStatus()` precedence in the
    /// dashboard (`src/dashboard.ts:78`).
    #[tokio::test]
    async fn sqlite_disarm_overrides_env_arm() {
        let _g = env_lock();
        let (store, _f) = tmp_store();
        // Write `twitter_real_enabled = false` directly.
        store
            .with_conn(|conn| {
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS config (\
                         key TEXT PRIMARY KEY,\
                         value TEXT NOT NULL,\
                         updatedAt INTEGER NOT NULL\
                     )",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO config (key, value, updatedAt) VALUES (?1, ?2, ?3)",
                    rusqlite::params![ARMED_CONFIG_KEY, "false", 0_i64],
                )?;
                Ok(())
            })
            .unwrap();
        std::env::set_var(ARMED_ENV_KEY, "true");
        assert!(
            !twitter_armed(&store),
            "explicit sqlite disarm must beat env arm"
        );
        std::env::remove_var(ARMED_ENV_KEY);
    }

    /// `is_truthy` recognises the canonical "on" forms `arm` writes
    /// (`"true"`) and the manual-override forms operators type (`"1"`),
    /// while every documented "off" form (`false`, `0`, empty, …) reads as
    /// disarmed. Keeps the gate's accept set in lockstep with what
    /// `channel_router::is_truthy` does in the CLI.
    #[test]
    fn is_truthy_matches_existing_gate_semantics() {
        for ok in ["true", "TRUE", "True", "1", "yes", "on"] {
            assert!(is_truthy(ok), "{ok:?} should be truthy");
        }
        for bad in ["", "  ", "false", "FALSE", "0", "off", "no"] {
            assert!(!is_truthy(bad), "{bad:?} should NOT be truthy");
        }
    }
}
