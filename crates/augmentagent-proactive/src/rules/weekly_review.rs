//! Weekly relationship-review rule (#57).
//!
//! Fires once a week (target: Sunday ~18:00 local) with a single Low-urgency
//! roll-up signal pointing the user at the `/relationships` dashboard. It
//! does NOT re-walk every person (the per-rule scans already do that and
//! their signals carry the detail) — its job is the once-a-week "spend 10
//! minutes on your network" nudge, batched, never a per-person card.
//!
//! Low urgency ⇒ the runner persists it but never posts a Discord card; it
//! surfaces only in the daily digest `## Relationships` section, honoring the
//! "≤1/day, batched" constraint.

use async_trait::async_trait;
use chrono::{Datelike, Timelike, Utc, Weekday};

use crate::scan::{
    Cadence, ProactiveSignal, ScanCtx, ScheduledScan, SignalKind, SuggestedAction, Urgency,
};

/// Hour-of-day (local-naive via UTC for determinism in tests) the review is
/// allowed to fire. We use a window [18:00, 21:00) so a 30-min runner tick
/// reliably catches it without needing a precise cron.
pub const REVIEW_HOUR_START: u32 = 18;
pub const REVIEW_HOUR_END: u32 = 21;

pub struct WeeklyReviewScan;

/// True when `now` is inside the Sunday-evening review window. Pulled out so
/// it's unit-testable without mocking the clock everywhere.
pub fn in_review_window(now: chrono::DateTime<Utc>) -> bool {
    now.weekday() == Weekday::Sun
        && now.hour() >= REVIEW_HOUR_START
        && now.hour() < REVIEW_HOUR_END
}

#[async_trait]
impl ScheduledScan for WeeklyReviewScan {
    fn id(&self) -> &'static str {
        "weekly_review"
    }

    fn cadence(&self) -> Cadence {
        // Weekly dedup window: even though the runner ticks every 30 min and
        // the review window is 3h wide, the ~6-day dedup window guarantees
        // exactly one persisted review per week.
        Cadence::Weekly
    }

    async fn scan(&self, _ctx: &ScanCtx) -> anyhow::Result<Vec<ProactiveSignal>> {
        let now = Utc::now();
        if !in_review_window(now) {
            return Ok(Vec::new());
        }
        // ISO week number makes the dedup key naturally unique per week.
        let iso = now.iso_week();
        let dedup = format!("weekly_review:{}:{}", iso.year(), iso.week());
        let sig = ProactiveSignal::new(
            SignalKind::StaleContact, // closest existing kind; review is a roll-up
            Urgency::Low,             // digest-only, never a card
            "Weekly relationship review",
            "Your end-of-week network check-in. Open the dashboard to triage \
             stale contacts, overdue commitments, and upcoming events in one \
             pass — see `/relationships`.",
            dedup,
        )
        .with_action(SuggestedAction {
            label: "Open /relationships".into(),
            draft_prompt: None,
        });
        Ok(vec![sig])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn window_is_sunday_evening_only() {
        // 2026-05-17 is a Sunday.
        let sun_18 = Utc.with_ymd_and_hms(2026, 5, 17, 18, 30, 0).unwrap();
        assert!(in_review_window(sun_18));
        let sun_09 = Utc.with_ymd_and_hms(2026, 5, 17, 9, 0, 0).unwrap();
        assert!(!in_review_window(sun_09));
        // 2026-05-18 is a Monday.
        let mon_18 = Utc.with_ymd_and_hms(2026, 5, 18, 18, 30, 0).unwrap();
        assert!(!in_review_window(mon_18));
        let sun_21 = Utc.with_ymd_and_hms(2026, 5, 17, 21, 0, 0).unwrap();
        assert!(!in_review_window(sun_21));
    }

    #[tokio::test]
    async fn emits_low_urgency_rollup_only_in_window() {
        // We can't move the wall clock; assert structure conditionally on the
        // real clock, but always assert the helper logic above. When the test
        // happens to run inside the window the scan yields exactly one Low
        // signal; otherwise zero. Either way it must never panic / err.
        let (_d, store) = crate::testutil::test_store();
        let wd = tempfile::tempdir().unwrap();
        let ctx = ScanCtx::new(store, wd.path().to_path_buf(), 0);
        let out = WeeklyReviewScan.scan(&ctx).await.unwrap();
        if in_review_window(Utc::now()) {
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].urgency, Urgency::Low);
        } else {
            assert!(out.is_empty());
        }
    }
}
