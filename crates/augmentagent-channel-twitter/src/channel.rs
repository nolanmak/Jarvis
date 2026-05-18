//! Trigger implementations for the X / Twitter channel.
//!
//! - **#15 `TwitterFeedTrigger`** — implements [`Trigger`] directly. Each tick
//!   it walks the wiki's `people/*.md` pages, keeps only those marked
//!   `close: true` in front-matter AND carrying a `twitter:` identity, then
//!   pulls each close friend's recent tweets since the last seen id. New
//!   tweets become `WorkItem { platform: "twitter", kind: "post_engagement" }`.
//!   The triage → draft → Discord-approval pipeline (driven by the CLI's
//!   ChannelRunner, same as LinkedIn) decides whether to draft a reply; no
//!   tweet is ever auto-posted — replies go through Discord approval.
//!   Cadence: 2h base ± up to 20min jitter; a daily reply cap is enforced by
//!   the shared [`RateGovernor`] (`Platform::Twitter` / `ActionKind::Reply`).
//!
//! - **#16 `TwitterDmTrigger`** — implements [`InboundSource`] (wrapped by
//!   [`InboundMessageTrigger`]). Polls the DM inbox no more often than every
//!   30min. Inbound DMs become `WorkItem { platform: "twitter", kind: "dm" }`.
//!   Shares the same `TwitterApi` session as the feed trigger.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use augmentagent_channel_core::trigger::{InboundSource, Trigger, WorkItem};
use augmentagent_wiki::{IdentityIndex, WikiLayout};

use crate::api::{TwitterApi, TwitterError};
use crate::types::{Tweet, TwitterDm};

/// Feed-poll base cadence: 12×/day. X anti-automation cares about request
/// rhythm; 2h ± jitter stays well inside human range.
pub const FEED_POLL_SECS: u64 = 2 * 60 * 60;
/// Jitter half-window: ±20min around the base interval.
pub const FEED_JITTER_SECS: u64 = 20 * 60;
/// DM poll floor — never poll the inbox more often than this.
pub const DM_POLL_MIN_SECS: u64 = 30 * 60;

/// A close friend resolved from the wiki: their twitter id + a stable slug
/// for last-seen bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFriend {
    pub slug: String,
    pub twitter_id: String,
}

/// Walk `people/*.md` and return the set of close friends with a `twitter:`
/// identity. "Close" = a `close: true` line inside the page's YAML
/// front-matter. The identity (`identities.twitter`) is parsed by the wiki
/// crate's [`IdentityIndex`]; the `close` flag is a lightweight scan because
/// the wiki front-matter struct doesn't model it.
pub fn close_friends_with_twitter(layout: &WikiLayout) -> std::io::Result<Vec<CloseFriend>> {
    let index = IdentityIndex::build(layout)?;
    let mut out = Vec::new();
    for page in index.pages() {
        let Some(tw) = page.identities.twitter.as_deref() else {
            continue;
        };
        if tw.is_empty() {
            continue;
        }
        if page_is_close(&page.path) {
            out.push(CloseFriend {
                slug: page.slug.clone(),
                twitter_id: tw.to_string(),
            });
        }
    }
    Ok(out)
}

/// True iff the page's front-matter contains a `close: true` line. Cheap
/// scan of the YAML block only (between the leading `---` fences) so a stray
/// "close" in body prose can't trip it.
fn page_is_close(path: &std::path::Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(after) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return false;
    };
    for line in after.lines() {
        let t = line.trim_end();
        if t == "---" {
            return false; // end of front-matter, no close flag seen
        }
        let l = t.trim();
        if l == "close: true" || l == "close: yes" {
            return true;
        }
    }
    false
}

/// Per-friend last-seen tweet id, so successive ticks don't re-yield. In
/// memory only — the triage pipeline's `emails` dedup (by `message_id`) is
/// the durable backstop across restarts.
type SeenMap = std::collections::HashMap<String, String>;

/// #15 — friend-post engagement trigger.
pub struct TwitterFeedTrigger<A: TwitterApi> {
    api: Arc<A>,
    wiki_root: PathBuf,
    my_user_id: String,
    seen: Mutex<SeenMap>,
}

impl<A: TwitterApi> TwitterFeedTrigger<A> {
    pub fn new(api: Arc<A>, wiki_root: PathBuf, my_user_id: String) -> Self {
        Self {
            api,
            wiki_root,
            my_user_id,
            seen: Mutex::new(SeenMap::new()),
        }
    }

    /// One poll pass: resolve close friends, pull each one's new tweets.
    pub async fn poll(&self) -> anyhow::Result<Vec<WorkItem>> {
        let layout = WikiLayout::new(self.wiki_root.clone());
        let friends = match close_friends_with_twitter(&layout) {
            Ok(f) => f,
            Err(e) => {
                warn!("twitter feed: wiki scan failed: {e}");
                return Ok(Vec::new());
            }
        };
        if friends.is_empty() {
            debug!("twitter feed: no close friends with a twitter: identity");
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let mut seen = self.seen.lock().await;
        for f in friends {
            let since = seen.get(&f.twitter_id).cloned();
            let tweets = match self
                .api
                .fetch_user_tweets(&f.twitter_id, since.as_deref())
                .await
            {
                Ok(t) => t,
                Err(TwitterError::AuthExpired) => {
                    warn!("twitter auth expired — run `augmentagent twitter login`");
                    return Ok(items);
                }
                Err(e) => {
                    warn!(friend = %f.slug, "twitter feed fetch failed: {e}");
                    continue;
                }
            };
            // Advance the high-water mark to the newest id we saw.
            if let Some(max) = max_id(&tweets) {
                seen.insert(f.twitter_id.clone(), max);
            }
            for t in tweets {
                if t.is_own(&self.my_user_id) {
                    continue;
                }
                items.push(tweet_to_work_item(t, &self.my_user_id));
            }
        }
        info!(count = items.len(), "twitter feed poll complete");
        Ok(items)
    }

    /// Base poll interval. Channel-runner adds jitter on top.
    pub fn poll_interval() -> Duration {
        let secs = std::env::var("AUGMENTAGENT_TWITTER_FEED_POLL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s >= 60)
            .unwrap_or(FEED_POLL_SECS);
        Duration::from_secs(secs)
    }
}

#[async_trait]
impl<A: TwitterApi + 'static> Trigger for TwitterFeedTrigger<A> {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        self.poll().await
    }
}

/// #16 — DM inbound source. Wrapped by `InboundMessageTrigger`.
pub struct TwitterDmSource<A: TwitterApi> {
    api: Arc<A>,
    my_user_id: String,
    last_poll_ms: Mutex<i64>,
    seen: Mutex<std::collections::HashSet<String>>,
}

impl<A: TwitterApi> TwitterDmSource<A> {
    pub fn new(api: Arc<A>, my_user_id: String) -> Self {
        Self {
            api,
            my_user_id,
            last_poll_ms: Mutex::new(0),
            seen: Mutex::new(std::collections::HashSet::new()),
        }
    }

    fn now_ms() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

#[async_trait]
impl<A: TwitterApi + 'static> InboundSource for TwitterDmSource<A> {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        // Self-throttle: never hit the inbox more than once per 30min even
        // if the runner ticks faster.
        {
            let mut last = self.last_poll_ms.lock().await;
            let now = Self::now_ms();
            if *last != 0 && now - *last < (DM_POLL_MIN_SECS as i64) * 1000 {
                debug!("twitter dm: skipping poll (under 30min floor)");
                return Ok(Vec::new());
            }
            *last = now;
        }

        let dms = match self.api.fetch_dm_inbox(None).await {
            Ok(d) => d,
            Err(TwitterError::AuthExpired) => {
                warn!("twitter auth expired — run `augmentagent twitter login`");
                return Ok(Vec::new());
            }
            Err(e) => {
                warn!("twitter dm inbox fetch failed: {e}");
                return Ok(Vec::new());
            }
        };

        let mut seen = self.seen.lock().await;
        let mut items = Vec::new();
        for dm in dms {
            if dm.is_outbound(&self.my_user_id) {
                continue;
            }
            if !seen.insert(dm.event_id.clone()) {
                continue; // already yielded this run-lifetime
            }
            items.push(dm_to_work_item(dm, &self.my_user_id));
        }
        info!(count = items.len(), "twitter dm poll complete");
        Ok(items)
    }
}

fn max_id(tweets: &[Tweet]) -> Option<String> {
    tweets
        .iter()
        .max_by(|a, b| {
            match (a.rest_id.parse::<u128>(), b.rest_id.parse::<u128>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => a.rest_id.cmp(&b.rest_id),
            }
        })
        .map(|t| t.rest_id.clone())
}

fn tweet_to_work_item(t: Tweet, my_user_id: &str) -> WorkItem {
    let email = t.clone().into_email(my_user_id);
    WorkItem {
        platform: "twitter".into(),
        kind: "post_engagement".into(),
        external_id: email.message_id.clone(),
        payload: serde_json::to_value(&email).unwrap_or(serde_json::Value::Null),
    }
}

fn dm_to_work_item(dm: TwitterDm, my_user_id: &str) -> WorkItem {
    let email = dm.clone().into_email(my_user_id);
    WorkItem {
        platform: "twitter".into(),
        kind: "dm".into(),
        external_id: email.message_id.clone(),
        payload: serde_json::to_value(&email).unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct StubApi {
        tweets: StdMutex<Vec<Tweet>>,
        dms: StdMutex<Vec<TwitterDm>>,
        expired: bool,
    }
    impl StubApi {
        fn with(tweets: Vec<Tweet>, dms: Vec<TwitterDm>) -> Self {
            Self {
                tweets: StdMutex::new(tweets),
                dms: StdMutex::new(dms),
                expired: false,
            }
        }
        fn expired() -> Self {
            Self {
                tweets: StdMutex::new(vec![]),
                dms: StdMutex::new(vec![]),
                expired: true,
            }
        }
    }
    #[async_trait]
    impl TwitterApi for StubApi {
        async fn fetch_user_tweets(
            &self,
            _u: &str,
            since: Option<&str>,
        ) -> Result<Vec<Tweet>, TwitterError> {
            if self.expired {
                return Err(TwitterError::AuthExpired);
            }
            let all = self.tweets.lock().unwrap().clone();
            Ok(match since {
                None => all,
                Some(s) => all
                    .into_iter()
                    .filter(|t| t.rest_id.as_str() > s)
                    .collect(),
            })
        }
        async fn reply_to_tweet(
            &self,
            _t: &str,
            _x: &str,
        ) -> Result<String, TwitterError> {
            Ok("rid".into())
        }
        async fn fetch_dm_inbox(
            &self,
            _c: Option<&str>,
        ) -> Result<Vec<TwitterDm>, TwitterError> {
            if self.expired {
                return Err(TwitterError::AuthExpired);
            }
            Ok(self.dms.lock().unwrap().clone())
        }
        async fn send_dm(&self, _c: &str, _t: &str) -> Result<String, TwitterError> {
            Ok("evt".into())
        }
    }

    fn wiki_with(pages: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        for (slug, fm) in pages {
            let body = format!("---\n{fm}\n---\n\n# {slug}\n");
            std::fs::write(layout.people_dir().join(format!("{slug}.md")), body).unwrap();
        }
        let root = d.path().to_path_buf();
        (d, root)
    }

    fn tweet(id: &str, author_id: &str) -> Tweet {
        Tweet {
            rest_id: id.into(),
            conversation_id: id.into(),
            author_name: "Jane".into(),
            author_handle: "jane".into(),
            author_id: author_id.into(),
            text: format!("tweet {id}"),
            created_at_ms: 1,
        }
    }

    #[test]
    fn close_friends_filters_on_flag_and_identity() {
        let (_d, root) = wiki_with(&[
            (
                "jane",
                "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
            ),
            (
                "bob",
                "kind: person\nkey: bob\nidentities:\n  twitter: \"66\"",
            ), // not close
            (
                "amy",
                "kind: person\nkey: amy\nclose: true", // close but no twitter
            ),
        ]);
        let layout = WikiLayout::new(root);
        let friends = close_friends_with_twitter(&layout).unwrap();
        assert_eq!(friends.len(), 1);
        assert_eq!(friends[0].slug, "jane");
        assert_eq!(friends[0].twitter_id, "55");
    }

    #[test]
    fn page_is_close_only_scans_front_matter() {
        let (_d, root) = wiki_with(&[(
            "x",
            "kind: person\nkey: x\nidentities:\n  twitter: \"1\"",
        )]);
        // body says "close: true" but not in front-matter
        let p = root.join("people/x.md");
        let mut body = std::fs::read_to_string(&p).unwrap();
        body.push_str("\nthe deal will close: true tomorrow\n");
        std::fs::write(&p, body).unwrap();
        assert!(!page_is_close(&p));
    }

    #[tokio::test]
    async fn feed_trigger_yields_new_tweets_and_skips_own() {
        let (_d, root) = wiki_with(&[(
            "jane",
            "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
        )]);
        let api = Arc::new(StubApi::with(
            vec![tweet("200", "55"), tweet("300", "99")],
            vec![],
        ));
        let tr = TwitterFeedTrigger::new(api, root, "99".into());
        let cancel = CancellationToken::new();
        let items = tr.next_work_items(&cancel).await.unwrap();
        // own tweet (author 99) skipped; only the "55" one yields
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].platform, "twitter");
        assert_eq!(items[0].kind, "post_engagement");
        assert_eq!(items[0].external_id, "200");
    }

    #[tokio::test]
    async fn feed_trigger_dedups_via_seen_high_water_mark() {
        let (_d, root) = wiki_with(&[(
            "jane",
            "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
        )]);
        let api = Arc::new(StubApi::with(vec![tweet("200", "55")], vec![]));
        let tr = TwitterFeedTrigger::new(api, root, "99".into());
        let cancel = CancellationToken::new();
        assert_eq!(tr.next_work_items(&cancel).await.unwrap().len(), 1);
        // Second tick: same single tweet, now <= seen high-water mark → none.
        assert_eq!(tr.next_work_items(&cancel).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn feed_trigger_auth_expired_returns_empty_not_err() {
        let (_d, root) = wiki_with(&[(
            "jane",
            "kind: person\nkey: jane\nclose: true\nidentities:\n  twitter: \"55\"",
        )]);
        let api = Arc::new(StubApi::expired());
        let tr = TwitterFeedTrigger::new(api, root, "99".into());
        let cancel = CancellationToken::new();
        assert!(tr.next_work_items(&cancel).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn dm_source_yields_inbound_skips_outbound() {
        let dms = vec![
            TwitterDm {
                event_id: "e1".into(),
                conversation_id: "55-99".into(),
                sender_name: "Jane".into(),
                sender_handle: "jane".into(),
                sender_id: "55".into(),
                text: "hi".into(),
                created_at_ms: 1,
            },
            TwitterDm {
                event_id: "e2".into(),
                conversation_id: "55-99".into(),
                sender_name: "Me".into(),
                sender_handle: "me".into(),
                sender_id: "99".into(),
                text: "my own".into(),
                created_at_ms: 2,
            },
        ];
        let api = Arc::new(StubApi::with(vec![], dms));
        let src = TwitterDmSource::new(api, "99".into());
        let items = src.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "dm");
        assert_eq!(items[0].external_id, "e1");
    }

    #[tokio::test]
    async fn dm_source_throttles_to_30min_floor() {
        let dms = vec![TwitterDm {
            event_id: "e1".into(),
            conversation_id: "c".into(),
            sender_name: "Jane".into(),
            sender_handle: "jane".into(),
            sender_id: "55".into(),
            text: "hi".into(),
            created_at_ms: 1,
        }];
        let api = Arc::new(StubApi::with(vec![], dms));
        let src = TwitterDmSource::new(api, "99".into());
        assert_eq!(src.fetch_new().await.unwrap().len(), 1);
        // Immediate re-poll is throttled (under 30min floor) → empty.
        assert!(src.fetch_new().await.unwrap().is_empty());
    }
}
