//! The daily sweep's candidates (#2285): things that slipped through days
//! ago, gathered by code so the model only judges a short list.
//!
//! Every collector is a bounded SQL query plus cheap filters on sender and
//! category. Nothing here decides that an item matters; filters only drop
//! known noise (CI and deploy bots, bulk and marketing senders, trial and
//! renewal reminders). Nothing that arrived in the last [`RECENT_GRACE_MS`]
//! is ever a candidate: real-time triage owns that.
//!
//! Sources, in priority order. 1 and 2 are built; the rest are follow-ups.
//! 1. Unanswered people ([`unanswered`]).
//! 2. Automated mail triage skipped ([`automated`]).
//! 3. Open promises in the owner's sent messages: follow-up.
//! 4. Calendar invites never answered: follow-up (the store holds no
//!    invites yet).
//! 5. Relationship gaps: follow-up.
//! 6. Money (unpaid invoices, failed payments): follow-up.

use std::collections::{HashMap, HashSet};

use augmentagent_store::{rusqlite, Store};
use rusqlite::params;
use serde::Serialize;

use crate::replies::{InboundRef, ReplyEvidence};

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Anything newer than this is real-time triage's job, never the sweep's.
pub const RECENT_GRACE_MS: i64 = 2 * DAY_MS;
/// How far back the sweep looks.
pub const LOOKBACK_MS: i64 = 14 * DAY_MS;
/// Inbound rows read per sweep for the unanswered-people check.
pub const UNANSWERED_SCAN: i64 = 300;
/// Unanswered conversations offered to the model per sweep.
pub const UNANSWERED_LIMIT: usize = 15;
/// Skipped-mail rows read per sweep.
pub const AUTOMATED_SCAN: i64 = 800;
/// Skipped mails offered to the model per sweep.
pub const AUTOMATED_LIMIT: usize = 60;
/// A sender address with this many mails in [`BULK_WINDOW_MS`] is a bulk
/// sender (newsletter, digest, notification stream).
pub const BULK_ADDRESS_MIN: usize = 6;
/// A sending domain with this many mails in the window is a bulk platform.
pub const BULK_DOMAIN_MIN: usize = 20;
pub const BULK_WINDOW_MS: i64 = 30 * DAY_MS;
/// Snippet length shown to the model.
pub const SNIPPET_CHARS: usize = 200;

/// Developer-tool and deploy senders: their mail is never a slipped outcome.
const NOISE_DOMAINS: &[&str] = &[
    "github.com",
    "githubusercontent.com",
    "vercel.com",
    "netlify.com",
    "circleci.com",
    "travis-ci.com",
    "gitlab.com",
    "buildkite.com",
    "sentry.io",
    "dependabot.com",
];

/// First labels that marketing platforms put on their sending domains.
const MARKETING_LABELS: &[&str] = &[
    "promotion",
    "promotions",
    "promo",
    "marketing",
    "mkt",
    "news",
    "newsletter",
    "newsletters",
    "offers",
    "deals",
];

/// The owner asked for no trial-ending, renewal or promo reminders. This
/// only removes them; it never marks anything important.
const REMINDER_PHRASES: &[&str] = &[
    "free trial",
    "your trial",
    "trial ends",
    "trial ending",
    "trial expires",
    "trial has ended",
    "renewal",
    "will renew",
    "auto-renew",
    "renews on",
    "% off",
    "promo code",
    "coupon",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// An item the owner snoozed whose snooze has run out.
    Snoozed,
    /// A person wrote and the owner never answered.
    Unanswered,
    /// Automated mail triage skipped.
    Automated,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Snoozed => "snoozed",
            Source::Unanswered => "unanswered",
            Source::Automated => "automated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// Stable identity for dedup (`heartbeat_items.item_key`).
    pub key: String,
    pub source: Source,
    pub at_ms: i64,
    /// Platform (`gmail`, `imessage`, ...).
    pub channel: String,
    pub who: String,
    pub title: String,
    pub snippet: String,
}

/// The time bounds of one sweep.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub now_ms: i64,
}

impl Window {
    pub fn newest_ms(self) -> i64 {
        self.now_ms - RECENT_GRACE_MS
    }
    pub fn oldest_ms(self) -> i64 {
        self.now_ms - LOOKBACK_MS
    }
}

/// Short id for an item key, printed on the notice for `heartbeat done`.
pub fn item_ref(key: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(key.as_bytes()))[..6].to_string()
}

fn snippet(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SNIPPET_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(SNIPPET_CHARS).collect();
    format!("{cut}…")
}

/// `Name <user@host>` or `user@host` to the lowercase address.
pub fn address_of(from: &str) -> String {
    let inner = match (from.rfind('<'), from.rfind('>')) {
        (Some(a), Some(b)) if a < b => &from[a + 1..b],
        _ => from,
    };
    inner.trim().trim_matches('"').to_ascii_lowercase()
}

pub fn domain_of(address: &str) -> &str {
    address.rsplit_once('@').map_or("", |(_, d)| d)
}

fn is_noise_domain(domain: &str) -> bool {
    NOISE_DOMAINS
        .iter()
        .any(|n| domain == *n || domain.ends_with(&format!(".{n}")))
}

fn is_marketing_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() > 2 && MARKETING_LABELS.contains(&labels[0])
}

fn is_reminder(subject: &str) -> bool {
    let s = subject.to_lowercase();
    REMINDER_PHRASES.iter().any(|p| s.contains(p))
}

fn table_exists(store: &Store, name: &str) -> bool {
    store
        .with_conn(|c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [name],
                |r| r.get::<_, bool>(0),
            )
        })
        .unwrap_or(false)
}

/// Keys the sweep must not offer again: everything recorded except snoozes
/// that have run out.
const NOT_BLOCKED: &str = "NOT EXISTS (SELECT 1 FROM heartbeat_items h WHERE h.item_key = {key} \
     AND NOT (h.state = 'snoozed' AND h.snooze_until_ms <= ?{now}))";

fn not_blocked(key_expr: &str, now_param: usize) -> String {
    NOT_BLOCKED
        .replace("{key}", key_expr)
        .replace("{now}", &now_param.to_string())
}

/// Source 1: a real person wrote 2 to 14 days ago on a direct channel, has
/// not written since, and the owner has not answered on any channel.
/// Gmail counts only when triage judged it worth a flag or a reply; skipped
/// and digest-only messages are never "people waiting".
pub fn unanswered(store: &Store, evidence: &dyn ReplyEvidence, w: Window) -> Vec<Candidate> {
    let list_headers = if table_exists(store, "email_list_headers") {
        "AND NOT EXISTS (SELECT 1 FROM email_list_headers l WHERE l.message_id = m.message_id)"
    } else {
        ""
    };
    let sql = format!(
        "SELECT m.message_id, m.platform, m.conversation_id, m.sender_handle, \
                COALESCE(NULLIF(m.sender_label, ''), e.fromEmail, m.sender_handle), \
                m.ts_ms, COALESCE(e.subject, ''), substr(COALESCE(e.body, ''), 1, 600) \
         FROM message_index m JOIN emails e ON e.messageId = m.message_id \
         WHERE m.from_me = 0 AND m.conv_kind IN ('dm', 'email') \
           AND m.ts_ms BETWEEN ?1 AND ?2 AND e.firstSeenAt <= ?2 \
           AND e.messageId NOT LIKE 'compose:%' \
           AND COALESCE(e.triageResult, '') NOT IN ('skip', 'digest_only') \
           AND (m.platform <> 'gmail' OR e.triageResult IN ('flag', 'reply')) \
           {list_headers} \
           AND NOT EXISTS (SELECT 1 FROM message_index n WHERE n.conversation_id = m.conversation_id \
                           AND n.from_me = 0 AND n.ts_ms > ?2) \
           AND {blocked} \
         ORDER BY m.ts_ms DESC LIMIT ?4",
        blocked = not_blocked("'unanswered:' || m.platform || ':' || m.conversation_id || ':' || m.message_id", 3),
    );
    let rows = store
        .with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(
                params![w.oldest_ms(), w.newest_ms(), w.now_ms, UNANSWERED_SCAN],
                |r| {
                    Ok((
                        InboundRef {
                            message_id: r.get(0)?,
                            platform: r.get(1)?,
                            conversation_id: r.get(2)?,
                            sender_handle: r.get(3)?,
                            at_ms: r.get(5)?,
                        },
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (m, who, subject, body) in rows {
        // Newest first, so the first row per conversation is its latest.
        if !seen.insert(m.conversation_id.clone()) {
            continue;
        }
        if evidence.owner_replied_since(&m) {
            continue;
        }
        out.push(Candidate {
            key: format!(
                "unanswered:{}:{}:{}",
                m.platform, m.conversation_id, m.message_id
            ),
            source: Source::Unanswered,
            at_ms: m.at_ms,
            channel: m.platform,
            who,
            title: subject,
            snippet: snippet(&body),
        });
        if out.len() >= UNANSWERED_LIMIT {
            break;
        }
    }
    out
}

/// Source 2: mail triage skipped (or never triaged) 2 to 14 days ago, minus
/// mailing-list mail, bulk senders, developer-tool senders, marketing
/// domains and trial/renewal/promo reminders. One mail per sender, newest
/// first. The model decides whether any of it records an outcome.
pub fn automated(store: &Store, w: Window) -> Vec<Candidate> {
    let list_headers = if table_exists(store, "email_list_headers") {
        "AND NOT EXISTS (SELECT 1 FROM email_list_headers l WHERE l.message_id = e.messageId)"
    } else {
        ""
    };
    let sql = format!(
        "SELECT e.messageId, e.fromEmail, e.subject, substr(COALESCE(e.body, ''), 1, 600), e.firstSeenAt \
         FROM emails e \
         WHERE e.platform = 'gmail' AND e.kind = 'dm' AND e.messageId NOT LIKE 'compose:%' \
           AND (e.triageResult IS NULL OR e.triageResult = 'skip') \
           AND e.firstSeenAt BETWEEN ?1 AND ?2 \
           {list_headers} \
           AND {blocked} \
         ORDER BY e.firstSeenAt DESC LIMIT ?4",
        blocked = not_blocked("'automated:' || e.messageId", 3),
    );
    let rows = store
        .with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(
                params![w.oldest_ms(), w.newest_ms(), w.now_ms, AUTOMATED_SCAN],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return Vec::new();
    }

    let (by_address, by_domain) = sender_volume(store, w.now_ms - BULK_WINDOW_MS);
    let mut seen_senders = HashSet::new();
    let mut out = Vec::new();
    for (id, from, subject, body, at_ms) in rows {
        let address = address_of(&from);
        let domain = domain_of(&address).to_string();
        let bulk = by_address.get(&address).copied().unwrap_or(0) >= BULK_ADDRESS_MIN
            || by_domain.get(&domain).copied().unwrap_or(0) >= BULK_DOMAIN_MIN;
        if bulk
            || is_noise_domain(&domain)
            || is_marketing_domain(&domain)
            || is_reminder(&subject)
            || !seen_senders.insert(address)
        {
            continue;
        }
        out.push(Candidate {
            key: format!("automated:{id}"),
            source: Source::Automated,
            at_ms,
            channel: "gmail".into(),
            who: from,
            title: subject,
            snippet: snippet(&body),
        });
        if out.len() >= AUTOMATED_LIMIT {
            break;
        }
    }
    out
}

/// Gmail volume per sender address and per domain since `since_ms`.
fn sender_volume(store: &Store, since_ms: i64) -> (HashMap<String, usize>, HashMap<String, usize>) {
    let froms: Vec<String> = store
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT fromEmail FROM emails WHERE platform = 'gmail' AND firstSeenAt >= ?1",
            )?;
            let rows = stmt.query_map([since_ms], |r| r.get(0))?;
            rows.collect()
        })
        .unwrap_or_default();
    let mut by_address = HashMap::new();
    let mut by_domain = HashMap::new();
    for from in froms {
        let address = address_of(&from);
        *by_domain
            .entry(domain_of(&address).to_string())
            .or_insert(0) += 1;
        *by_address.entry(address).or_insert(0) += 1;
    }
    (by_address, by_domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_and_domain_parse() {
        assert_eq!(
            address_of("Example Cloud <No-Reply@Startups.Example.com>"),
            "no-reply@startups.example.com"
        );
        assert_eq!(address_of("plain@example.org"), "plain@example.org");
        assert_eq!(domain_of("a@b.example.org"), "b.example.org");
    }

    #[test]
    fn noise_and_marketing_are_sender_based() {
        assert!(is_noise_domain("github.com"));
        assert!(is_noise_domain("bounce.vercel.com"));
        assert!(!is_noise_domain("notgithub.com"));
        assert!(is_marketing_domain("promotion.shop.example.com"));
        assert!(!is_marketing_domain("example.com"));
        assert!(!is_marketing_domain("startups.example.com"));
    }

    #[test]
    fn reminders_are_dropped_but_outcomes_are_not() {
        assert!(is_reminder("Your free trial ends in 3 days"));
        assert!(is_reminder("Subscription renewal notice"));
        assert!(!is_reminder("Your credits application was approved"));
    }

    #[test]
    fn refs_are_short_and_stable() {
        assert_eq!(item_ref("automated:m1"), item_ref("automated:m1"));
        assert_eq!(item_ref("automated:m1").len(), 6);
        assert_ne!(item_ref("automated:m1"), item_ref("automated:m2"));
    }
}
