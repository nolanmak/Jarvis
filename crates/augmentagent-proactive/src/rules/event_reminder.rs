//! Event-reminder rule.
//!
//! Walks each person page's `events:` ledger for recurring annual events
//! (birthday, anniversary, wedding) and emits one signal when the next
//! occurrence falls inside a configurable lead window.

use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, Utc};

use crate::person::parse_people_dir;
use crate::scan::{
    Cadence, ProactiveSignal, ScanCtx, ScheduledScan, SignalKind, SuggestedAction, Urgency,
};

pub const DEFAULT_LEAD_DAYS: i64 = 7;

pub struct EventReminderScan {
    pub lead_days: i64,
}

impl Default for EventReminderScan {
    fn default() -> Self {
        Self {
            lead_days: DEFAULT_LEAD_DAYS,
        }
    }
}

fn is_recurring(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "birthday" | "anniversary" | "wedding" | "work_anniversary"
    )
}

fn next_occurrence(original: NaiveDate, today: NaiveDate) -> Option<NaiveDate> {
    let (m, d) = (original.month(), original.day());
    for year in [today.year(), today.year() + 1] {
        let cand = NaiveDate::from_ymd_opt(year, m, d)
            .or_else(|| NaiveDate::from_ymd_opt(year, m, 28));
        if let Some(c) = cand {
            if c >= today {
                return Some(c);
            }
        }
    }
    None
}

#[async_trait]
impl ScheduledScan for EventReminderScan {
    fn id(&self) -> &'static str {
        "event_reminder"
    }

    fn cadence(&self) -> Cadence {
        Cadence::Daily
    }

    async fn scan(&self, ctx: &ScanCtx) -> anyhow::Result<Vec<ProactiveSignal>> {
        let today = Utc::now().date_naive();
        let mut out = Vec::new();

        for page in parse_people_dir(&ctx.wiki.people_dir()) {
            let name = page.display_name();
            for ev in &page.events {
                if !is_recurring(&ev.kind) {
                    continue;
                }
                let Some(next) = next_occurrence(ev.date, today) else {
                    continue;
                };
                let days_out = (next - today).num_days();
                if days_out < 0 || days_out > self.lead_days {
                    continue;
                }
                let urgency = if days_out <= 1 {
                    Urgency::High
                } else {
                    Urgency::Normal
                };
                let when = if days_out == 0 {
                    "today".to_string()
                } else if days_out == 1 {
                    "tomorrow".to_string()
                } else {
                    format!("in {days_out} days ({next})")
                };
                let kind_label = ev.kind.replace('_', " ");
                let headline = format!("{name}'s {kind_label} is {when}");
                let detail = format!(
                    "{name}'s {kind_label} ({next}) is {when}. Source: `people/{}.md`.",
                    page.slug,
                );
                let dedup = format!(
                    "event_reminder:{}:{}:{}",
                    page.slug,
                    ev.kind,
                    next.year()
                );
                out.push(
                    ProactiveSignal::new(
                        SignalKind::EventReminder,
                        urgency,
                        headline,
                        detail,
                        dedup,
                    )
                    .with_person(&page.slug)
                    .with_action(SuggestedAction {
                        label: format!("Draft a note to {name}"),
                        draft_prompt: Some(format!(
                            "{name}'s {kind_label} is {when}. Draft a short, personal note. \
                             Pull a detail from `people/{}.md` if one fits.",
                            page.slug,
                        )),
                    }),
                );
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::Store;
    use augmentagent_wiki::WikiLayout;
    use std::sync::Arc;

    fn ctx_with(body: &str) -> (tempfile::TempDir, tempfile::TempDir, ScanCtx) {
        let dbd = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dbd.path().join("data.db")).unwrap());
        let wd = tempfile::tempdir().unwrap();
        let layout = WikiLayout::new(wd.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("p@x.com"), body).unwrap();
        let ctx = ScanCtx::new(store, wd.path().to_path_buf(), 0);
        (dbd, wd, ctx)
    }

    #[tokio::test]
    async fn fires_for_birthday_inside_window() {
        let bday = Utc::now().date_naive() + chrono::Duration::days(3);
        let body = format!(
            "---\nname: Jane\nevents:\n  - date: 1990-{:02}-{:02}\n    kind: birthday\n---\n# Jane\n",
            bday.month(),
            bday.day()
        );
        let (_a, _b, ctx) = ctx_with(&body);
        let sigs = EventReminderScan::default().scan(&ctx).await.unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].kind, SignalKind::EventReminder);
        assert!(sigs[0].headline.contains("Jane"));
        assert!(sigs[0].headline.contains("birthday"));
    }

    #[tokio::test]
    async fn silent_when_event_far_off() {
        let far = Utc::now().date_naive() + chrono::Duration::days(60);
        let body = format!(
            "---\nname: Jane\nevents:\n  - date: 1990-{:02}-{:02}\n    kind: birthday\n---\n# Jane\n",
            far.month(),
            far.day()
        );
        let (_a, _b, ctx) = ctx_with(&body);
        assert!(EventReminderScan::default()
            .scan(&ctx)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn ignores_non_recurring_events() {
        let soon = Utc::now().date_naive() + chrono::Duration::days(2);
        let body = format!(
            "---\nname: Jane\nevents:\n  - date: 2026-{:02}-{:02}\n    kind: new_job\n---\n# Jane\n",
            soon.month(),
            soon.day()
        );
        let (_a, _b, ctx) = ctx_with(&body);
        assert!(EventReminderScan::default()
            .scan(&ctx)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn next_occurrence_rolls_forward() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let orig = NaiveDate::from_ymd_opt(1990, 5, 10).unwrap();
        assert_eq!(
            next_occurrence(orig, today),
            NaiveDate::from_ymd_opt(2027, 5, 10)
        );
        let orig2 = NaiveDate::from_ymd_opt(1985, 7, 20).unwrap();
        assert_eq!(
            next_occurrence(orig2, today),
            NaiveDate::from_ymd_opt(2026, 7, 20)
        );
    }

    #[test]
    fn leap_day_falls_back_to_feb_28() {
        let today = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        let orig = NaiveDate::from_ymd_opt(2000, 2, 29).unwrap();
        assert_eq!(
            next_occurrence(orig, today),
            NaiveDate::from_ymd_opt(2027, 2, 28)
        );
    }
}
