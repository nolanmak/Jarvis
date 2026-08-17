//! GitHub-native types + conversion into the shared `augmentagent_store::Email`.
//!
//! The store + broker + wiki pipeline are channel-agnostic — they all consume
//! `Email`. We repurpose the fields:
//!
//! - `message_id` ← `"gh:<thread_id>"` (notifications API thread, stable across polls)
//! - `thread_id`  ← `<owner>/<repo>#<number>` (so the approver knows where to post a reply)
//! - `from`       ← `"<actor login> <github:<actor login>>"`
//! - `subject`    ← `"[<kind>] <repo> #<number> — <title>"` (Discord card title)
//! - `body`       ← rendered notification subject body (latest comment / PR description)
//! - `date`       ← RFC3339 from `updated_at`
//! - `account_entity_id` ← `"github:<user login>"`
//! - `platform`   ← `"github"`
//! - `kind`       ← `"mention" | "review_request" | "assignment"`
//!
//! The `github:` prefix on `account_entity_id` is how the approver in
//! `augmentagent-cli` knows to route send requests through the GitHub REST
//! client instead of Gmail/Slack/etc.

use augmentagent_store::Email;
use serde::Deserialize;

use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// Raw GitHub notifications API response item.
///
/// We accept the wire shape under `#[serde(default)]` so unknown fields don't
/// trip us up — the upstream API has shipped fields in additive layers over
/// the years.
///
/// Wire reference: <https://docs.github.com/en/rest/activity/notifications>
#[derive(Debug, Clone, Deserialize)]
pub struct Notification {
    /// String per the API ("19874442"). We keep it as a string and parse only
    /// when we need to PATCH `/notifications/threads/{thread_id}`.
    pub id: String,
    /// Why GitHub thinks we should care. We allowlist a subset (see
    /// [`Notification::triage_kind`]).
    pub reason: String,
    /// `true` if the notification has been viewed.
    #[serde(default)]
    pub unread: bool,
    /// Wall-clock RFC3339 timestamp of the latest update on the thread.
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub repository: Repository,
    #[serde(default)]
    pub subject: NotificationSubject,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Repository {
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub html_url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationSubject {
    #[serde(default)]
    pub title: String,
    /// e.g. `"PullRequest"`, `"Issue"`, `"Discussion"`, `"Commit"`.
    #[serde(default, rename = "type")]
    pub subject_type: String,
    /// REST URL pointing at the linked PR/Issue/Discussion. The trailing
    /// integer is the issue/PR number.
    #[serde(default)]
    pub url: String,
}

/// Mapped kind we use downstream. Returns `None` for reasons we deliberately
/// skip (the subscribed / comment-firehose noise that prompted this filter).
pub fn map_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "mention" => Some("mention"),
        "review_requested" => Some("review_request"),
        "assign" => Some("assignment"),
        "subscribed-when-mentioned" => Some("mention"),
        // All other reasons are excluded by default per #49 — see the open
        // question on subscription threshold. Notable exclusions: "subscribed"
        // (the firehose), "author", "comment", "manual", "ci_activity",
        // "team_mention", "state_change".
        _ => None,
    }
}

impl Notification {
    /// Allowlist filter — returns the canonical kind iff the notification's
    /// reason is one we propagate into triage.
    pub fn triage_kind(&self) -> Option<&'static str> {
        map_reason(&self.reason)
    }

    /// Pull `<owner>/<repo>#<number>` from the subject URL. Returns `None`
    /// when the subject has no numeric tail (e.g. a Commit notification —
    /// which we don't currently triage anyway).
    pub fn thread_locator(&self) -> Option<ThreadLocator> {
        ThreadLocator::parse(&self.repository.full_name, &self.subject.url)
    }

    /// Parse `id` to a u64 for the mark-as-read PATCH endpoint. Cheap; if it
    /// ever fails we surface as a soft warning at call sites.
    pub fn thread_id_u64(&self) -> Option<u64> {
        self.id.parse().ok()
    }
}

/// Decomposed thread identifier — what the outbound comment endpoint needs.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ThreadLocator {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl ThreadLocator {
    /// Parse from a repo full name like `"octocat/Hello-World"` plus a
    /// notification subject URL like
    /// `"https://api.github.com/repos/octocat/Hello-World/pulls/123"`.
    pub fn parse(full_name: &str, subject_url: &str) -> Option<Self> {
        let (owner, repo) = full_name.split_once('/')?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        // Last path segment is the numeric id — works for both `/issues/N` and
        // `/pulls/N`. Discussions use a non-numeric id (the GraphQL node id),
        // so we'll fall through to `None` and the channel will treat the
        // notification as "no reply target".
        let tail = subject_url.rsplit('/').next()?;
        let number = tail.parse::<u64>().ok()?;
        Some(ThreadLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    /// `<owner>/<repo>#<number>` for `Email::thread_id`. The outbound approver
    /// parses this back into owner / repo / number on Approve.
    pub fn as_thread_id(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }

    /// Recover an owner/repo/number triple from `Email::thread_id`. Inverse
    /// of [`as_thread_id`].
    pub fn from_thread_id(s: &str) -> Option<Self> {
        let (owner_repo, number_str) = s.split_once('#')?;
        let (owner, repo) = owner_repo.split_once('/')?;
        let number = number_str.parse::<u64>().ok()?;
        Some(ThreadLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }
}

/// Subject detail fetched from `notification.subject.url`. We use only the
/// barest slice — body text + a stable author label — for the triage prompt.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubjectDetail {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub user: SubjectUser,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubjectUser {
    #[serde(default)]
    pub login: String,
}

/// A fully-decorated triage candidate — what the channel hands the reasoner.
#[derive(Debug, Clone)]
pub struct TriageCandidate {
    pub thread_id_u64: u64,
    pub kind: &'static str,
    pub locator: Option<ThreadLocator>,
    pub repo_full_name: String,
    pub title: String,
    pub body: String,
    pub author_login: String,
    pub html_url: String,
    pub updated_at: String,
}

impl TriageCandidate {
    /// Convert to the store's generic `Email`. `my_login` is the authenticated
    /// GitHub user's login, stamped into `account_entity_id` so the approver
    /// can route outbound back to the right PAT.
    pub fn into_email(self, my_login: &str) -> Email {
        let from = format!("{} <github:{}>", self.author_login, self.author_login);
        let subject = format!(
            "[{}] {} #{} — {}",
            self.kind,
            self.repo_full_name,
            self.locator
                .as_ref()
                .map(|l| l.number.to_string())
                .unwrap_or_default(),
            self.title,
        );
        let thread_id = self.locator.as_ref().map(ThreadLocator::as_thread_id);
        let account_entity_id = format!("{ACCOUNT_ENTITY_ID_PREFIX}:{my_login}");
        Email {
            to: String::new(),
            cc: String::new(),
            message_id: format!("gh:{}", self.thread_id_u64),
            thread_id,
            from,
            subject,
            body: self.body,
            date: self.updated_at,
            account_entity_id: Some(account_entity_id),
            platform: PLATFORM.to_string(),
            kind: self.kind.to_string(),
        }
    }
}

/// `account_entity_id` prefix on `Email` rows that came from this channel.
/// Used by the approver to fast-route Approve clicks through `octocrab`/REST
/// rather than Composio/Gmail.
pub const ACCOUNT_PREFIX: &str = "github:";

/// True iff the email row came from this channel.
pub fn is_github_email(email: &Email) -> bool {
    email
        .account_entity_id
        .as_deref()
        .is_some_and(|a| a.starts_with(ACCOUNT_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_reason_passes_allowed_reasons() {
        assert_eq!(map_reason("mention"), Some("mention"));
        assert_eq!(map_reason("review_requested"), Some("review_request"));
        assert_eq!(map_reason("assign"), Some("assignment"));
        assert_eq!(map_reason("subscribed-when-mentioned"), Some("mention"));
    }

    #[test]
    fn map_reason_rejects_floods() {
        assert_eq!(map_reason("subscribed"), None);
        assert_eq!(map_reason("comment"), None);
        assert_eq!(map_reason("author"), None);
        assert_eq!(map_reason("ci_activity"), None);
        assert_eq!(map_reason("state_change"), None);
        assert_eq!(map_reason(""), None);
    }

    #[test]
    fn thread_locator_parses_pull_url() {
        let loc = ThreadLocator::parse(
            "octocat/Hello-World",
            "https://api.github.com/repos/octocat/Hello-World/pulls/42",
        )
        .unwrap();
        assert_eq!(loc.owner, "octocat");
        assert_eq!(loc.repo, "Hello-World");
        assert_eq!(loc.number, 42);
        assert_eq!(loc.as_thread_id(), "octocat/Hello-World#42");
    }

    #[test]
    fn thread_locator_parses_issue_url() {
        let loc = ThreadLocator::parse(
            "nolanmak/AugmentAgent",
            "https://api.github.com/repos/nolanmak/AugmentAgent/issues/99",
        )
        .unwrap();
        assert_eq!(loc.number, 99);
    }

    #[test]
    fn thread_locator_round_trips_via_thread_id() {
        let loc = ThreadLocator {
            owner: "a".into(),
            repo: "b".into(),
            number: 7,
        };
        let s = loc.as_thread_id();
        let back = ThreadLocator::from_thread_id(&s).unwrap();
        assert_eq!(back, loc);
    }

    #[test]
    fn thread_locator_rejects_non_numeric_tail() {
        let loc = ThreadLocator::parse(
            "x/y",
            "https://api.github.com/repos/x/y/discussions/D_kwDOABC",
        );
        assert!(loc.is_none());
    }

    #[test]
    fn into_email_sets_github_prefix() {
        let cand = TriageCandidate {
            thread_id_u64: 12345,
            kind: "mention",
            locator: Some(ThreadLocator {
                owner: "octocat".into(),
                repo: "Hello-World".into(),
                number: 7,
            }),
            repo_full_name: "octocat/Hello-World".into(),
            title: "Need a hand".into(),
            body: "@nolanmak can you take a look?".into(),
            author_login: "octocat".into(),
            html_url: "https://github.com/octocat/Hello-World/pull/7".into(),
            updated_at: "2026-05-14T12:00:00Z".into(),
        };
        let email = cand.into_email("nolanmak");
        assert_eq!(email.platform, "github");
        assert_eq!(email.kind, "mention");
        assert_eq!(email.message_id, "gh:12345");
        assert_eq!(email.thread_id.as_deref(), Some("octocat/Hello-World#7"));
        assert_eq!(
            email.account_entity_id.as_deref(),
            Some("github:nolanmak")
        );
        assert!(email.subject.starts_with("[mention] octocat/Hello-World #7 —"));
        assert!(is_github_email(&email));
    }

    #[test]
    fn notification_triage_kind_filters() {
        let mut n: Notification = serde_json::from_str(
            r#"{"id":"1","reason":"subscribed","updated_at":"","unread":true,
                "repository":{"full_name":"a/b","html_url":""},
                "subject":{"title":"x","type":"Issue","url":"https://api.github.com/repos/a/b/issues/1"}}"#,
        )
        .unwrap();
        assert!(n.triage_kind().is_none());
        n.reason = "mention".into();
        assert_eq!(n.triage_kind(), Some("mention"));
    }
}
