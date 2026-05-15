//! 17-row 2025/26 cap matrix per #83 §3.
//!
//! Caps are deliberately set to roughly 30% of each platform's published
//! soft-limit so we leave headroom for the user's *own* manual taps. For
//! free-tier X — where published caps are ~2400 posts+replies+reposts/day —
//! we cap *much* lower than 30% because the user should still own most of
//! that budget.
//!
//! Each row carries a `// citation:` comment with the URL the spec named.
//! This is a deliberately static table — caps shouldn't be user-editable
//! without code review.

use std::time::Duration;

use super::{ActionKind, Platform};

/// Per-(platform, action) caps. `None` for `hour` / `burst_5m` means the
/// platform doesn't merit a sub-day window for that action (post-frequency,
/// for instance, is naturally rate-limited by `min_gap`).
///
/// `min_gap` is enforced via `last_event_at()` on the same-action counter.
/// `burst_5m` is the strictest of the three windowed caps and serves as
/// the anti-runaway-loop floor.
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    pub platform: Platform,
    pub action: ActionKind,
    pub day: u32,
    pub hour: Option<u32>,
    pub burst_5m: Option<u32>,
    pub min_gap: Duration,
    /// URL of the published source (or community-research write-up) that
    /// motivated this row. Tied to a comment in code per #83 §3.
    pub source_url: &'static str,
}

/// Convenience: `RateLimit` projected onto the four numbers `permit()` cares
/// about. Scaled by warmup multiplier where applicable.
#[derive(Debug, Clone, Copy)]
pub struct RateCaps {
    pub day: u32,
    pub hour: Option<u32>,
    pub burst_5m: Option<u32>,
    pub min_gap: Duration,
}

impl From<&RateLimit> for RateCaps {
    fn from(l: &RateLimit) -> Self {
        Self {
            day: l.day,
            hour: l.hour,
            burst_5m: l.burst_5m,
            min_gap: l.min_gap,
        }
    }
}

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// The full 17-row cap table.
///
/// Order is (platform, action) ascending so a binary search would work if
/// we ever outgrow linear scan; today there are 17 rows so `lookup()` just
/// walks the slice.
pub const RATE_TABLE: &[RateLimit] = &[
    // -----------------------------------------------------------------
    // Instagram (5 rows). 30% of platform soft-limits except for `follow`,
    // which we floor at 20 because it's the most-banned action class.
    // -----------------------------------------------------------------
    // citation: https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/
    //           https://fameviso.com/blog/instagram-limits-2025-guide/
    // Community-reported feed-post tolerance is 25/day combined feed+story;
    // we cap at 2 to keep room for organic posting.
    RateLimit {
        platform: Platform::Instagram,
        action: ActionKind::Post,
        day: 2,
        hour: None,
        burst_5m: None,
        min_gap: secs(0),
        source_url:
            "https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/",
    },
    // citation: https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/
    // Platform: 300-500/day, ~20/hr; ours is ~20% of daily, ~75% of hourly to absorb manual taps.
    RateLimit {
        platform: Platform::Instagram,
        action: ActionKind::Like,
        day: 60,
        hour: Some(15),
        burst_5m: Some(5),
        min_gap: secs(30),
        source_url:
            "https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/",
    },
    // citation: https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/
    // Platform: 12-14/hr with 350-400s spacing; we keep our hourly under that.
    RateLimit {
        platform: Platform::Instagram,
        action: ActionKind::Comment,
        day: 30,
        hour: Some(10),
        burst_5m: Some(3),
        min_gap: secs(60),
        source_url:
            "https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/",
    },
    // citation: https://fameviso.com/blog/instagram-limits-2025-guide/
    // Platform: 200/day, 20/hr; 30% would be 60/day, but follow is the
    // most-banned action so we floor at 20.
    RateLimit {
        platform: Platform::Instagram,
        action: ActionKind::Follow,
        day: 20,
        hour: Some(5),
        burst_5m: None,
        min_gap: secs(300),
        source_url: "https://fameviso.com/blog/instagram-limits-2025-guide/",
    },
    // citation: https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/
    // Platform: 100/day to followers, 30-40/day to non-followers; we cap at
    // the cold-DM number to absorb the worst case.
    RateLimit {
        platform: Platform::Instagram,
        action: ActionKind::Dm,
        day: 10,
        hour: Some(3),
        burst_5m: None,
        min_gap: secs(60),
        source_url:
            "https://elfsight.com/blog/instagram-restrictions-limits-likes-followers-comments/",
    },
    // -----------------------------------------------------------------
    // LinkedIn (6 rows). No published caps in most cases — bounds come
    // from the Phantombuster + LinkedSDR community-research articles.
    // -----------------------------------------------------------------
    // citation: https://phantombuster.com/blog/social-selling/linkedin-connection-request-limit/
    // No published cap; >5-10/day flags spam classifier per Phantombuster.
    // 4-hour gap mimics "checked feed at lunch" cadence.
    RateLimit {
        platform: Platform::LinkedIn,
        action: ActionKind::Post,
        day: 3,
        hour: None,
        burst_5m: None,
        min_gap: secs(4 * 3600),
        source_url:
            "https://phantombuster.com/blog/social-selling/linkedin-connection-request-limit/",
    },
    // citation: https://www.linkedsdr.com/blog/linkedin-limits-complete-guide-to-connection-message-view-restrictions
    // No official cap; community-reported ~100/day soft, ~30/hr.
    RateLimit {
        platform: Platform::LinkedIn,
        action: ActionKind::Like,
        day: 80,
        hour: Some(20),
        burst_5m: Some(5),
        min_gap: secs(30),
        source_url:
            "https://www.linkedsdr.com/blog/linkedin-limits-complete-guide-to-connection-message-view-restrictions",
    },
    // citation: https://subzeroid.github.io/instagrapi/usage-guide/best-practices.html
    // Same source as like; comments have higher ban-weight per instagrapi
    // heuristics applied analogously.
    RateLimit {
        platform: Platform::LinkedIn,
        action: ActionKind::Comment,
        day: 25,
        hour: Some(8),
        burst_5m: Some(3),
        min_gap: secs(60),
        source_url: "https://subzeroid.github.io/instagrapi/usage-guide/best-practices.html",
    },
    // citation: https://phantombuster.com/blog/social-selling/linkedin-connection-request-limit/
    //           https://expandi.io/blog/linkedin-account-warm-up/
    // Free-tier published cap: ~100/week (~14/day) hard ceiling per
    // Phantombuster; cold accounts should target 10-20/day per Expandi.
    // 15/day with 10-min gap = 2.5h spread.
    RateLimit {
        platform: Platform::LinkedIn,
        action: ActionKind::ConnectionInvite,
        day: 15,
        hour: Some(5),
        burst_5m: None,
        min_gap: secs(600),
        source_url:
            "https://phantombuster.com/blog/social-selling/linkedin-connection-request-limit/",
    },
    // citation: https://phantombuster.com/blog/social-selling/linkedin-connection-request-limit/
    // Free tier: 100 messages/week per Phantombuster. 100/7 ≈ 14/day; we
    // cap at 20 with weekly accounting recommended for v2.
    RateLimit {
        platform: Platform::LinkedIn,
        action: ActionKind::Dm,
        day: 20,
        hour: Some(6),
        burst_5m: None,
        min_gap: secs(90),
        source_url:
            "https://phantombuster.com/blog/social-selling/linkedin-connection-request-limit/",
    },
    // citation: https://www.linkedsdr.com/blog/linkedin-limits-complete-guide-to-connection-message-view-restrictions
    // Free tier triggers commercial-use-limit warning at ~80/day.
    RateLimit {
        platform: Platform::LinkedIn,
        action: ActionKind::ProfileView,
        day: 50,
        hour: Some(15),
        burst_5m: None,
        min_gap: secs(20),
        source_url:
            "https://www.linkedsdr.com/blog/linkedin-limits-complete-guide-to-connection-message-view-restrictions",
    },
    // -----------------------------------------------------------------
    // X / Twitter (6 rows). Free tier publishes ~2400/day across
    // posts+replies+reposts; we cap aggressively to leave the user's own
    // typing dominant.
    // -----------------------------------------------------------------
    // citation: https://www.tendx.app/blog/x-twitter-limits-2026
    // Free tier: 2400/day across posts+replies+reposts, ~50 per 30-min
    // rolling; we cap at <1% to leave the daily allowance entirely for
    // the user.
    RateLimit {
        platform: Platform::Twitter,
        action: ActionKind::Post,
        day: 20,
        hour: Some(5),
        burst_5m: Some(3),
        min_gap: secs(60),
        source_url: "https://www.tendx.app/blog/x-twitter-limits-2026",
    },
    // citation: https://www.tendx.app/blog/x-twitter-limits-2026
    // Same 2400/day pool; replies are the agent's main use case so we
    // allocate more.
    RateLimit {
        platform: Platform::Twitter,
        action: ActionKind::Reply,
        day: 100,
        hour: Some(25),
        burst_5m: Some(5),
        min_gap: secs(30),
        source_url: "https://www.tendx.app/blog/x-twitter-limits-2026",
    },
    // citation: https://www.tendx.app/blog/x-twitter-limits-2026
    // No published like cap; behavior-based throttle. 200/day stays well
    // clear of any reasonable threshold.
    RateLimit {
        platform: Platform::Twitter,
        action: ActionKind::Like,
        day: 200,
        hour: Some(50),
        burst_5m: Some(10),
        min_gap: secs(15),
        source_url: "https://www.tendx.app/blog/x-twitter-limits-2026",
    },
    // citation: https://www.tendx.app/blog/x-twitter-limits-2026
    // Free: 400/day, 40-50/hr informal; 50/day = 12.5% leaves room for manual.
    RateLimit {
        platform: Platform::Twitter,
        action: ActionKind::Follow,
        day: 50,
        hour: Some(10),
        burst_5m: None,
        min_gap: secs(300),
        source_url: "https://www.tendx.app/blog/x-twitter-limits-2026",
    },
    // citation: https://www.tendx.app/blog/x-twitter-limits-2026
    // Free: ~500/day, varies by account standing; we cap aggressively
    // for cold-DM ban risk.
    RateLimit {
        platform: Platform::Twitter,
        action: ActionKind::Dm,
        day: 30,
        hour: Some(10),
        burst_5m: None,
        min_gap: secs(60),
        source_url: "https://www.tendx.app/blog/x-twitter-limits-2026",
    },
    // citation: https://www.tendx.app/blog/x-twitter-limits-2026
    // Counts toward the 2400/day post pool; same cap logic as post.
    RateLimit {
        platform: Platform::Twitter,
        action: ActionKind::Repost,
        day: 20,
        hour: Some(5),
        burst_5m: Some(3),
        min_gap: secs(60),
        source_url: "https://www.tendx.app/blog/x-twitter-limits-2026",
    },
];

/// Resolve `(platform, action) → RateLimit`. Linear scan over a 17-row
/// const slice; unmeasurably fast vs. the SQL roundtrip that follows.
///
/// Returns `None` if the combination isn't in the table — caller maps that
/// to a permissive (`u32::MAX`) cap, since the absence of a row means the
/// agent has no opinion on this combination yet (e.g. Instagram::Reply).
pub fn lookup(platform: Platform, action: ActionKind) -> Option<&'static RateLimit> {
    RATE_TABLE
        .iter()
        .find(|r| r.platform == platform && r.action == action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_has_seventeen_rows() {
        // Spec lock — adding/removing a row should be a deliberate diff
        // bumping this count, not a quiet oops.
        assert_eq!(RATE_TABLE.len(), 17);
    }

    #[test]
    fn no_duplicate_platform_action_pairs() {
        for (i, a) in RATE_TABLE.iter().enumerate() {
            for b in RATE_TABLE.iter().skip(i + 1) {
                assert!(
                    !(a.platform == b.platform && a.action == b.action),
                    "duplicate row in RATE_TABLE: {:?}/{:?}",
                    a.platform,
                    a.action,
                );
            }
        }
    }

    #[test]
    fn every_row_has_nonempty_citation() {
        for r in RATE_TABLE {
            assert!(
                r.source_url.starts_with("http"),
                "row {:?}/{:?} missing http(s) citation",
                r.platform,
                r.action
            );
        }
    }

    #[test]
    fn lookup_round_trip() {
        let li_invite = lookup(Platform::LinkedIn, ActionKind::ConnectionInvite).unwrap();
        assert_eq!(li_invite.day, 15);
        assert_eq!(li_invite.min_gap, Duration::from_secs(600));
        assert!(lookup(Platform::Instagram, ActionKind::Reply).is_none());
    }
}
