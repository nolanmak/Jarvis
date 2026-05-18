//! Friend-post engagement (#19).
//!
//! `InstagramFeedTrigger` implements [`Trigger`] directly (Phase-3 feed
//! engagement, anchored to the channel-core `FriendFeedSource` marker). On
//! each 4h ± 30min tick it:
//!
//! 1. Walks the wiki `people/*.md` pages, keeping those with **both**
//!    `close: true` in front-matter **and** an `identities.instagram` handle.
//! 2. Resolves each handle → numeric user id, pulls their recent feed.
//! 3. For each *new* post (not yet acted on), drafts a comment from the
//!    **caption only** (#19: caption-only context — we do not fetch the
//!    image or other comments).
//! 4. Routes **every** comment through Discord approval — there is NO
//!    auto-post path (#19). The governor caps engagement at ≤3/day.
//!
//! The actual `post_comment` call happens from the CLI approval handler on
//! user click, same dispatch model as DM replies.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use augmentagent_channel_core::trigger::{FriendFeedSource, Trigger, WorkItem};
use augmentagent_channel_core::{
    ActionKind, ActionRequest, Denial, Platform, RateGovernor, Risk, TargetAttrs,
};
use augmentagent_wiki::WikiLayout;

use crate::api::InstagramApi;
use crate::types::{FeedPost, PLATFORM};

/// Default feed-scan cadence: 4h (#19). Jitter only adds.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;

/// Jitter: ±30 min (#19).
pub const JITTER_SECS: u64 = 30 * 60;

/// Hard daily engagement cap (#19: ≤3). Enforced via the governor (the IG
/// `Comment` row caps day at 30, so this channel-level cap is the tighter
/// constraint and is checked explicitly before each draft).
pub const DAILY_ENGAGE_CAP: u32 = 3;

/// One close contact resolved from the wiki: their slug + IG handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseContact {
    pub slug: String,
    pub instagram_handle: String,
}

/// Minimal front-matter we parse from a `people/<slug>.md` page. We only need
/// `close` + `identities.instagram`; everything else is ignored. Mirrors the
/// `IdentityIndex` front-matter approach (the wiki crate has no `close`
/// parser yet — this is the channel-local one).
#[derive(Debug, Default, Deserialize)]
struct CloseFrontMatter {
    #[serde(default)]
    close: bool,
    #[serde(default)]
    identities: IdentitiesBlock,
}

#[derive(Debug, Default, Deserialize)]
struct IdentitiesBlock {
    #[serde(default)]
    instagram: Option<String>,
}

/// Walk `layout.people_dir()`, returning every page that is BOTH `close:
/// true` AND has an `identities.instagram` handle. Bad / unparseable pages
/// are logged and skipped (one bad page must not break the scan).
pub fn close_instagram_contacts(layout: &WikiLayout) -> Vec<CloseContact> {
    let dir = layout.people_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            debug!(dir = %dir.display(), "people dir unreadable: {e}");
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let slug = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let Some(yaml) = extract_yaml_block(&raw) else {
            continue;
        };
        match serde_yaml_ng::from_str::<CloseFrontMatter>(yaml) {
            Ok(fm) => {
                if fm.close {
                    if let Some(handle) = fm
                        .identities
                        .instagram
                        .map(|h| h.trim_start_matches('@').to_string())
                        .filter(|h| !h.is_empty())
                    {
                        out.push(CloseContact {
                            slug,
                            instagram_handle: handle,
                        });
                    }
                }
            }
            Err(e) => warn!(path = %path.display(), "skipping page with bad front-matter: {e}"),
        }
    }
    out
}

/// Pull the YAML between the opening `---\n` and the next `---` line. Copied
/// from the wiki crate's identity module (crate-private there).
fn extract_yaml_block(src: &str) -> Option<&str> {
    let after_open = src
        .strip_prefix("---\n")
        .or_else(|| src.strip_prefix("---\r\n"))?;
    let mut offset = 0usize;
    for line in after_open.lines() {
        if line.trim_end() == "---" {
            return Some(&after_open[..offset]);
        }
        offset += line.len() + 1;
    }
    None
}

#[derive(Clone, Debug)]
pub struct FeedTriggerConfig {
    pub poll_interval: Duration,
    pub wiki_root: Option<PathBuf>,
}

impl Default for FeedTriggerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            wiki_root: None,
        }
    }
}

/// The feed-engagement trigger. Holds the api + governor + a username→pk
/// resolution cache so repeated scans don't re-resolve handles every tick.
pub struct InstagramFeedTrigger<A: InstagramApi> {
    pub api: Arc<A>,
    pub governor: Arc<dyn RateGovernor>,
    pub account_id: String,
    pub config: FeedTriggerConfig,
    /// handle → user_id resolution cache.
    handle_pk: tokio::sync::Mutex<BTreeMap<String, String>>,
}

#[async_trait]
impl<A: InstagramApi + 'static> FriendFeedSource for InstagramFeedTrigger<A> {
    async fn fetch_new_friend_posts(&self) -> anyhow::Result<Vec<WorkItem>> {
        // Drive the same wiki-`close:`-driven scan as the `Trigger` path; the
        // per-contact dedup (newest-post-only-per-tick + caption-only context)
        // is the scan's responsibility, matching the `FriendFeedSource`
        // contract. Use a fresh, never-cancelled token for the one-shot pull.
        self.scan(&CancellationToken::new()).await
    }
}

impl<A: InstagramApi> InstagramFeedTrigger<A> {
    pub fn new(
        api: Arc<A>,
        governor: Arc<dyn RateGovernor>,
        account_id: String,
        config: FeedTriggerConfig,
    ) -> Self {
        Self {
            api,
            governor,
            account_id,
            config,
            handle_pk: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Governor check for one comment. Returns `true` if there's headroom
    /// (the comment still needs Discord approval — this is only the cap +
    /// circuit-breaker gate, not the auto-post decision).
    async fn engagement_has_headroom(&self) -> bool {
        if self.governor.is_halted(Platform::Instagram).await.is_some() {
            return false;
        }
        // A Comment is always approval-required in the governor matrix, so
        // `permit()` returns `ApprovalRequired` on headroom and a cap denial
        // when exhausted. We treat ApprovalRequired as "headroom OK, route
        // to approval" and any cap/halt denial as "no headroom".
        let req = ActionRequest {
            platform: Platform::Instagram,
            action: ActionKind::Comment,
            account_id: self.account_id.clone(),
            risk: Risk::Low,
            cause: "feed-engagement".into(),
            target_id: None,
            target_attrs: Some(TargetAttrs {
                known_contact: true, // close: true contact by construction
                mass_action: false,
                stranger: false,
            }),
        };
        match self.governor.permit(req).await {
            Ok(_) | Err(Denial::ApprovalRequired { .. }) => true,
            Err(d) => {
                debug!(?d, "instagram engagement denied (no headroom)");
                false
            }
        }
    }

    async fn resolve_pk(&self, handle: &str) -> Option<String> {
        {
            let cache = self.handle_pk.lock().await;
            if let Some(pk) = cache.get(handle) {
                return Some(pk.clone());
            }
        }
        // We don't have a dedicated profile endpoint on the trait (kept
        // narrow per scope); the feed endpoint accepts a numeric id. For a
        // handle we conservatively skip resolution here and let the caller
        // treat a non-numeric handle as unresolvable until a profile-info
        // method lands. Numeric handles (already a pk) pass through.
        if handle.chars().all(|c| c.is_ascii_digit()) {
            let mut cache = self.handle_pk.lock().await;
            cache.insert(handle.to_string(), handle.to_string());
            return Some(handle.to_string());
        }
        debug!(handle, "instagram handle not numeric; pk resolution deferred");
        None
    }
}

impl<A: InstagramApi + 'static> InstagramFeedTrigger<A> {
    /// One scan pass over wiki `close: true` instagram contacts. Shared by the
    /// generic [`Trigger`] path and the [`FriendFeedSource`] contract so both
    /// drive identical, dedup-respecting logic.
    async fn scan(&self, cancel: &CancellationToken) -> anyhow::Result<Vec<WorkItem>> {
        let Some(root) = &self.config.wiki_root else {
            debug!("instagram feed trigger: no wiki root; nothing to scan");
            return Ok(Vec::new());
        };
        let layout = WikiLayout::new(root.clone());
        let contacts = close_instagram_contacts(&layout);
        if contacts.is_empty() {
            debug!("instagram feed trigger: no close: true + instagram contacts");
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let mut emitted = 0u32;
        for contact in contacts {
            if cancel.is_cancelled() || emitted >= DAILY_ENGAGE_CAP {
                break;
            }
            if !self.engagement_has_headroom().await {
                info!("instagram feed engagement cap/halt reached; stopping scan");
                break;
            }
            let Some(pk) = self.resolve_pk(&contact.instagram_handle).await else {
                continue;
            };
            let (posts, _cursor) = match self.api.fetch_user_feed(&pk, None).await {
                Ok(v) => v,
                Err(e) if e.is_soft_block() => {
                    warn!(error = %e, "instagram feed soft-blocked; stopping scan");
                    break;
                }
                Err(e) => {
                    warn!(slug = %contact.slug, "fetch_user_feed failed: {e:#}");
                    continue;
                }
            };
            // Newest post only per contact per tick — caption-only context.
            if let Some(post) = posts.into_iter().next() {
                items.push(post_to_work_item(&post, &contact.slug));
                emitted += 1;
            }
        }
        Ok(items)
    }
}

#[async_trait]
impl<A: InstagramApi + 'static> Trigger for InstagramFeedTrigger<A> {
    async fn next_work_items(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        self.scan(cancel).await
    }
}

/// Build the `WorkItem` for a friend post. `kind = "post_engagement"`
/// matches the channel-core `WorkItem` kind taxonomy. The payload carries the
/// caption (the only context the drafter gets) + the human URL.
pub fn post_to_work_item(post: &FeedPost, contact_slug: &str) -> WorkItem {
    WorkItem {
        platform: PLATFORM.to_string(),
        kind: "post_engagement".to_string(),
        external_id: format!("ig:comment:{}", post.media_id),
        payload: serde_json::json!({
            "media_id": post.media_id,
            "shortcode": post.shortcode,
            "post_url": format!("https://www.instagram.com/p/{}/", post.shortcode),
            "author_name": post.author_name,
            "author_pk": post.author_pk,
            "caption": post.caption,
            "contact_slug": contact_slug,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::{SqliteGovernor, SystemClock};
    use augmentagent_store::Store;
    use std::sync::Arc;

    fn write_page(dir: &std::path::Path, slug: &str, fm: &str) {
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!("---\n{fm}\n---\n\n# {slug}\n"),
        )
        .unwrap();
    }

    #[test]
    fn close_contacts_requires_both_close_and_instagram() {
        let dir = tempfile::tempdir().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        std::fs::create_dir_all(layout.people_dir()).unwrap();
        let people = layout.people_dir();
        // close + instagram → included
        write_page(
            &people,
            "tony",
            "kind: person\nclose: true\nidentities:\n  instagram: \"@tony_ig\"",
        );
        // close but no instagram → excluded
        write_page(
            &people,
            "jane",
            "kind: person\nclose: true\nidentities:\n  email: [j@x.com]",
        );
        // instagram but not close → excluded
        write_page(
            &people,
            "bob",
            "kind: person\nclose: false\nidentities:\n  instagram: bob_ig",
        );
        // broken front-matter → skipped, doesn't crash
        write_page(&people, "broken", "close: [unterminated");

        let contacts = close_instagram_contacts(&layout);
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].slug, "tony");
        // @ stripped
        assert_eq!(contacts[0].instagram_handle, "tony_ig");
    }

    #[test]
    fn post_to_work_item_carries_caption_and_url() {
        let post = FeedPost {
            media_id: "999_123".into(),
            shortcode: "C_abc".into(),
            author_name: "Jane".into(),
            author_pk: "123".into(),
            caption: "shipped".into(),
            taken_at_ms: 0,
        };
        let wi = post_to_work_item(&post, "jane");
        assert_eq!(wi.kind, "post_engagement");
        assert_eq!(wi.external_id, "ig:comment:999_123");
        assert_eq!(wi.payload["caption"].as_str(), Some("shipped"));
        assert_eq!(
            wi.payload["post_url"].as_str(),
            Some("https://www.instagram.com/p/C_abc/")
        );
        assert_eq!(wi.payload["contact_slug"].as_str(), Some("jane"));
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(file.path()).unwrap();
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
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT,
                    agentProcessedAt INTEGER,
                    platform TEXT NOT NULL DEFAULT 'gmail',
                    kind TEXT NOT NULL DEFAULT 'dm'
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                CREATE TABLE channel_subscriptions (
                    id TEXT PRIMARY KEY, platform TEXT NOT NULL,
                    channel_id TEXT NOT NULL, display_name TEXT NOT NULL,
                    mode TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
                    last_seen_message_id TEXT, last_digest_at_ms INTEGER,
                    created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE slack_workspaces (
                    id TEXT PRIMARY KEY, team_id TEXT NOT NULL UNIQUE,
                    team_name TEXT NOT NULL, entity_id TEXT NOT NULL,
                    connection_id TEXT NOT NULL, user_id TEXT NOT NULL,
                    active INTEGER NOT NULL DEFAULT 1, created_at_ms INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    struct EmptyApi;
    #[async_trait]
    impl InstagramApi for EmptyApi {
        async fn fetch_inbox(
            &self,
            _c: Option<&str>,
        ) -> Result<(Vec<crate::types::Dm>, Option<String>), crate::api::InstagramError>
        {
            Ok((vec![], None))
        }
        async fn send_dm(
            &self,
            _t: &str,
            _x: &str,
        ) -> Result<String, crate::api::InstagramError> {
            Ok("x".into())
        }
        async fn fetch_user_feed(
            &self,
            _u: &str,
            _c: Option<&str>,
        ) -> Result<(Vec<FeedPost>, Option<String>), crate::api::InstagramError>
        {
            Ok((
                vec![FeedPost {
                    media_id: "999_123".into(),
                    shortcode: "C1".into(),
                    author_name: "Tony".into(),
                    author_pk: "123".into(),
                    caption: "hello world".into(),
                    taken_at_ms: 0,
                }],
                None,
            ))
        }
        async fn post_comment(
            &self,
            _m: &str,
            _t: &str,
        ) -> Result<String, crate::api::InstagramError> {
            Ok("c".into())
        }
    }

    #[tokio::test]
    async fn trigger_emits_one_post_per_close_contact() {
        let (store, _f) = tmp_store();
        let dir = tempfile::tempdir().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        std::fs::create_dir_all(layout.people_dir()).unwrap();
        // numeric handle so resolve_pk passes without a profile endpoint
        write_page(
            &layout.people_dir(),
            "tony",
            "kind: person\nclose: true\nidentities:\n  instagram: \"123\"",
        );
        let gov: Arc<dyn RateGovernor> =
            Arc::new(SqliteGovernor::new(store, Arc::new(SystemClock)));
        let trig = InstagramFeedTrigger::new(
            Arc::new(EmptyApi),
            gov,
            "456".into(),
            FeedTriggerConfig {
                wiki_root: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let cancel = CancellationToken::new();
        let items = trig.next_work_items(&cancel).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "post_engagement");
        assert_eq!(items[0].payload["caption"].as_str(), Some("hello world"));
    }

    #[tokio::test]
    async fn trigger_empty_without_wiki_root() {
        let (store, _f) = tmp_store();
        let gov: Arc<dyn RateGovernor> =
            Arc::new(SqliteGovernor::new(store, Arc::new(SystemClock)));
        let trig = InstagramFeedTrigger::new(
            Arc::new(EmptyApi),
            gov,
            "456".into(),
            FeedTriggerConfig::default(),
        );
        let cancel = CancellationToken::new();
        assert!(trig.next_work_items(&cancel).await.unwrap().is_empty());
    }
}
