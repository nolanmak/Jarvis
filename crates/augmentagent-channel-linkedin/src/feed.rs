//! Friend-post engagement (#13).
//!
//! [`LinkedInFeedTrigger`] is a [`Trigger`] that, on each ~6h-with-jitter
//! tick, walks the wiki for `people/*.md` pages flagged `close: true` that
//! also carry a `linkedin:` identity, fetches each watched person's recent
//! feed posts via Voyager, dedups against ones we've already surfaced or
//! engaged, enforces a durable per-day engagement cap, and yields one
//! `WorkItem { platform:"linkedin", kind:"post_engagement" }` per fresh post.
//!
//! It produces *work items only* — the triage → draft → approval-card path
//! is the channel's job (mirrors how `LinkedInChannel` handles DMs). Every
//! engagement still requires Discord approval; nothing here auto-posts.
//!
//! Cadence: 6h base + jitter, same anti-fingerprint posture as the DM poll
//! (LinkedIn's anti-bot heuristics care about request regularity).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use augmentagent_channel_core::trigger::{Trigger, WorkItem};
use augmentagent_store::Store;
use augmentagent_wiki::{IdentityIndex, WikiLayout};

use crate::api::LinkedInApi;
use crate::types::FeedPost;

/// Default feed poll cadence: every 6h. The issue calls for "low cadence
/// (every 6h with jitter)".
pub const DEFAULT_FEED_POLL_SECS: u64 = 6 * 60 * 60;

/// Default per-day engagement cap (#13: "default 5"). Enforced durably via
/// `linkedin_action_log` so it survives daemon restarts.
pub const DEFAULT_MAX_ENGAGEMENTS_PER_DAY: u32 = 5;

/// `linkedin_action_log.action_kind` value the feed path writes / counts.
pub const ENGAGEMENT_ACTION_KIND: &str = "post_engagement";

/// Serialized payload carried in `WorkItem.payload` so the channel handler
/// can rebuild a [`FeedPost`]-equivalent without re-fetching the feed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FeedEngagementPayload {
    pub post_urn: String,
    pub author_name: String,
    pub author_urn: String,
    pub text: String,
    pub created_at_ms: i64,
    /// Wiki slug of the watched person — handy for the approval card and
    /// for tone lookup downstream.
    pub person_slug: String,
}

pub struct LinkedInFeedTrigger<L: LinkedInApi> {
    api: Arc<L>,
    store: Arc<Store>,
    /// Wiki root; `None` disables the trigger (no watch-list source).
    wiki_root: Option<PathBuf>,
    max_per_day: u32,
}

impl<L: LinkedInApi> LinkedInFeedTrigger<L> {
    pub fn new(
        api: Arc<L>,
        store: Arc<Store>,
        wiki_root: Option<PathBuf>,
        max_per_day: u32,
    ) -> Self {
        Self {
            api,
            store,
            wiki_root,
            max_per_day: max_per_day.max(1),
        }
    }

    /// Rolling-24h count of successful engagements, read from the durable
    /// `linkedin_action_log`. Used to gate the daily cap across restarts.
    fn engagements_last_24h(&self, now_ms: i64) -> u32 {
        let since = now_ms - 24 * 3600 * 1000;
        self.store
            .linkedin_action_count_since(ENGAGEMENT_ACTION_KIND, since)
            .unwrap_or(0)
    }

    fn already_seen(&self, post_urn: &str) -> bool {
        // Two-layer dedup: an existing email row (surfaced before) OR a
        // logged successful engagement on this exact post.
        if self
            .store
            .is_message_processed(post_urn)
            .unwrap_or(false)
        {
            return true;
        }
        self.store
            .linkedin_action_exists(ENGAGEMENT_ACTION_KIND, post_urn)
            .unwrap_or(false)
    }
}

#[async_trait]
impl<L: LinkedInApi + 'static> Trigger for LinkedInFeedTrigger<L> {
    async fn next_work_items(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        let Some(root) = self.wiki_root.clone() else {
            debug!("linkedin feed trigger: no wiki root configured; skipping");
            return Ok(Vec::new());
        };
        let layout = WikiLayout::new(root);
        let index = IdentityIndex::build(&layout)?;
        let watch = index.close_linkedin_people();
        if watch.is_empty() {
            debug!("linkedin feed trigger: no `close: true` + linkedin people");
            return Ok(Vec::new());
        }

        let now_ms = now_millis();
        let used = self.engagements_last_24h(now_ms);
        if used >= self.max_per_day {
            debug!(
                used,
                cap = self.max_per_day,
                "linkedin feed trigger: daily engagement cap reached; deferring"
            );
            return Ok(Vec::new());
        }
        let mut budget = self.max_per_day - used;

        let mut out = Vec::new();
        for (slug, urn) in watch {
            if cancel.is_cancelled() || budget == 0 {
                break;
            }
            let posts = match self.api.fetch_feed_posts_by_author(&urn).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(person = %slug, error = %e, "feed fetch failed; skipping author");
                    continue;
                }
            };
            for post in posts {
                if budget == 0 {
                    break;
                }
                if self.already_seen(&post.post_urn) {
                    continue;
                }
                out.push(to_work_item(&post, &slug));
                budget -= 1;
            }
        }
        Ok(out)
    }
}

fn to_work_item(post: &FeedPost, slug: &str) -> WorkItem {
    let payload = FeedEngagementPayload {
        post_urn: post.post_urn.clone(),
        author_name: post.author_name.clone(),
        author_urn: post.author_urn.0.clone(),
        text: post.text.clone(),
        created_at_ms: post.created_at_ms,
        person_slug: slug.to_string(),
    };
    WorkItem {
        platform: "linkedin".into(),
        kind: "post_engagement".into(),
        external_id: post.post_urn.clone(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
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
    use crate::api::LinkedInError;
    use crate::posting::{PostDraft, ShareUrn};
    use crate::types::{Dm, MemberUrn};

    struct StubApi {
        posts: Vec<FeedPost>,
    }

    #[async_trait]
    impl LinkedInApi for StubApi {
        async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
            Ok(vec![])
        }
        async fn send_message(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
            Ok("urn:li:messagingMessage:STUB".into())
        }
        async fn fetch_feed_posts_by_author(
            &self,
            _author_urn: &str,
        ) -> Result<Vec<FeedPost>, LinkedInError> {
            Ok(self.posts.clone())
        }
        async fn post_comment(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
            Ok("urn:li:comment:STUB".into())
        }
        async fn react(&self, _: &str, _: &str) -> Result<(), LinkedInError> {
            Ok(())
        }
        async fn create_share(
            &self,
            _draft: PostDraft<'_>,
        ) -> Result<ShareUrn, LinkedInError> {
            Ok(ShareUrn("urn:li:share:STUB".into()))
        }
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

    fn wiki_with_close_jane() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let people = dir.path().join("people");
        std::fs::create_dir_all(&people).unwrap();
        std::fs::write(
            people.join("jane.md"),
            "---\nkind: person\nkey: jane\nclose: true\nidentities:\n  linkedin: urn:li:fsd_profile:JANE\n---\n\n# Jane\n",
        )
        .unwrap();
        dir
    }

    fn post(urn: &str) -> FeedPost {
        FeedPost {
            post_urn: urn.into(),
            author_name: "Jane Doe".into(),
            author_urn: MemberUrn("urn:li:fsd_profile:JANE".into()),
            text: "Shipped a big release today!".into(),
            created_at_ms: 1_776_630_000_000,
        }
    }

    #[tokio::test]
    async fn yields_work_items_for_close_people() {
        let (store, _f) = tmp_store();
        let dir = wiki_with_close_jane();
        let api = Arc::new(StubApi {
            posts: vec![post("urn:li:activity:1"), post("urn:li:activity:2")],
        });
        let trig = LinkedInFeedTrigger::new(
            api,
            store,
            Some(dir.path().to_path_buf()),
            DEFAULT_MAX_ENGAGEMENTS_PER_DAY,
        );
        let items = trig.next_work_items(&CancellationToken::new()).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].platform, "linkedin");
        assert_eq!(items[0].kind, "post_engagement");
        assert_eq!(items[0].external_id, "urn:li:activity:1");
        let p: FeedEngagementPayload =
            serde_json::from_value(items[0].payload.clone()).unwrap();
        assert_eq!(p.person_slug, "jane");
        assert_eq!(p.author_urn, "urn:li:fsd_profile:JANE");
    }

    #[tokio::test]
    async fn daily_cap_limits_yield() {
        let (store, _f) = tmp_store();
        let dir = wiki_with_close_jane();
        let api = Arc::new(StubApi {
            posts: vec![
                post("urn:li:activity:1"),
                post("urn:li:activity:2"),
                post("urn:li:activity:3"),
            ],
        });
        // Cap of 2 → only 2 of the 3 posts surface.
        let trig =
            LinkedInFeedTrigger::new(api, store, Some(dir.path().to_path_buf()), 2);
        let items = trig.next_work_items(&CancellationToken::new()).await.unwrap();
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn prior_engagement_counts_against_cap_and_dedups() {
        let (store, _f) = tmp_store();
        let dir = wiki_with_close_jane();
        // Log one prior successful engagement → cap of 2 leaves budget 1,
        // and the engaged post is also deduped out.
        store
            .log_linkedin_action(
                "evt-1",
                ENGAGEMENT_ACTION_KIND,
                Some("urn:li:activity:1"),
                "ok",
                now_millis() - 1000,
                None,
            )
            .unwrap();
        let api = Arc::new(StubApi {
            posts: vec![
                post("urn:li:activity:1"), // already engaged
                post("urn:li:activity:2"),
                post("urn:li:activity:3"),
            ],
        });
        let trig =
            LinkedInFeedTrigger::new(api, store, Some(dir.path().to_path_buf()), 2);
        let items = trig.next_work_items(&CancellationToken::new()).await.unwrap();
        // budget = 2 - 1 = 1; activity:1 deduped; so exactly activity:2.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "urn:li:activity:2");
    }

    #[tokio::test]
    async fn no_wiki_root_yields_empty() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi { posts: vec![] });
        let trig = LinkedInFeedTrigger::new(api, store, None, 5);
        let items = trig.next_work_items(&CancellationToken::new()).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn no_close_people_yields_empty() {
        let (store, _f) = tmp_store();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("people")).unwrap();
        let api = Arc::new(StubApi {
            posts: vec![post("urn:li:activity:1")],
        });
        let trig =
            LinkedInFeedTrigger::new(api, store, Some(dir.path().to_path_buf()), 5);
        let items = trig.next_work_items(&CancellationToken::new()).await.unwrap();
        assert!(items.is_empty());
    }
}
