//! Stale-contact rule.
//!
//! Walks `wiki/people/*.md`, resolves each page's contact cadence and emits
//! one signal per person whose last touch is older than their cadence.
//! "Last touch" is the page's `updated:` (fallback `created:`) frontmatter
//! date. Pages with no explicit cadence are skipped — the engine only
//! enforces cadences the user configured via the CRM form.

use async_trait::async_trait;
use chrono::Utc;

use crate::person::parse_people_dir;
use crate::scan::{
    Cadence, ProactiveSignal, ScanCtx, ScheduledScan, SignalKind, SuggestedAction, Urgency,
};

pub struct StaleContactScan;

#[async_trait]
impl ScheduledScan for StaleContactScan {
    fn id(&self) -> &'static str {
        "stale_contact"
    }

    fn cadence(&self) -> Cadence {
        Cadence::Daily
    }

    async fn scan(&self, ctx: &ScanCtx) -> anyhow::Result<Vec<ProactiveSignal>> {
        let today = Utc::now().date_naive();
        let mut out = Vec::new();

        for page in parse_people_dir(&ctx.wiki.people_dir()) {
            if !page.cadence_explicit {
                continue;
            }
            let Some(last) = page.last_touch else {
                continue;
            };
            let age_days = (today - last).num_days();
            if age_days <= page.cadence_days {
                continue;
            }

            let overdue_by = age_days - page.cadence_days;
            let name = page.display_name();
            let urgency = if overdue_by >= page.cadence_days {
                Urgency::High
            } else {
                Urgency::Normal
            };

            let headline = format!("Reconnect with {name}");
            let detail = format!(
                "Last interaction {age_days}d ago; your cadence for this contact is {cadence}d \
                 (overdue by {overdue_by}d). Source: `people/{slug}.md`.",
                cadence = page.cadence_days,
                slug = page.slug,
            );
            let dedup = format!("stale_contact:{}", page.slug);

            out.push(
                ProactiveSignal::new(
                    SignalKind::StaleContact,
                    urgency,
                    headline,
                    detail,
                    dedup,
                )
                .with_person(&page.slug)
                .with_action(SuggestedAction {
                    label: format!("Draft a check-in to {name}"),
                    draft_prompt: Some(format!(
                        "Write a short, warm check-in message to {name}. It's been {age_days} \
                         days since the last contact. Keep it light and low-pressure. Ground \
                         any specifics in `people/{slug}.md`.",
                        slug = page.slug,
                    )),
                }),
            );
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_wiki::WikiLayout;

    fn ctx_with_pages(
        pages: &[(&str, &str)],
    ) -> (tempfile::TempDir, tempfile::TempDir, ScanCtx) {
        let (dbd, store) = crate::testutil::test_store();
        let wd = tempfile::tempdir().unwrap();
        let layout = WikiLayout::new(wd.path().to_path_buf());
        layout.bootstrap().unwrap();
        for (slug, body) in pages {
            std::fs::write(layout.person_page(&format!("{slug}@x.com")), body).unwrap();
        }
        let ctx = ScanCtx::new(store, wd.path().to_path_buf(), 0);
        (dbd, wd, ctx)
    }

    #[tokio::test]
    async fn fires_for_overdue_explicit_cadence() {
        let old = (Utc::now().date_naive() - chrono::Duration::days(60))
            .format("%Y-%m-%d")
            .to_string();
        let body =
            format!("---\nname: Jane\ncadence: weekly\nupdated: {old}\n---\n\n# Jane\n");
        let (_a, _b, ctx) = ctx_with_pages(&[("jane", &body)]);
        let sigs = StaleContactScan.scan(&ctx).await.unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].kind, SignalKind::StaleContact);
        assert!(sigs[0].headline.contains("Jane"));
        assert!(sigs[0].suggested_action.is_some());
    }

    #[tokio::test]
    async fn silent_for_recent_contact() {
        let recent = Utc::now().date_naive().format("%Y-%m-%d").to_string();
        let body =
            format!("---\nname: Jane\ncadence: monthly\nupdated: {recent}\n---\n# Jane\n");
        let (_a, _b, ctx) = ctx_with_pages(&[("jane", &body)]);
        assert!(StaleContactScan.scan(&ctx).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_pages_without_explicit_cadence() {
        let old = (Utc::now().date_naive() - chrono::Duration::days(400))
            .format("%Y-%m-%d")
            .to_string();
        let body = format!("---\nname: NoCad\nupdated: {old}\n---\n# NoCad\n");
        let (_a, _b, ctx) = ctx_with_pages(&[("nocad", &body)]);
        assert!(StaleContactScan.scan(&ctx).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn escalates_when_far_overdue() {
        let old = (Utc::now().date_naive() - chrono::Duration::days(40))
            .format("%Y-%m-%d")
            .to_string();
        let body = format!("---\nname: J\ncadence: weekly\nupdated: {old}\n---\n# J\n");
        let (_a, _b, ctx) = ctx_with_pages(&[("j", &body)]);
        let sigs = StaleContactScan.scan(&ctx).await.unwrap();
        assert_eq!(sigs[0].urgency, Urgency::High);
    }
}
