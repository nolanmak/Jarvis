//! Stale-commitment rule.
//!
//! Parses each person page's `## Commitments` section for unchecked markdown
//! checklist items carrying an inline `(due: YYYY-MM-DD)` and emits one
//! signal per item whose due date has passed.

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};

use crate::person::parse_people_dir;
use crate::scan::{
    Cadence, ProactiveSignal, ScanCtx, ScheduledScan, SignalKind, SuggestedAction, Urgency,
};

pub struct StaleCommitmentScan;

#[async_trait]
impl ScheduledScan for StaleCommitmentScan {
    fn id(&self) -> &'static str {
        "stale_commitment"
    }

    fn cadence(&self) -> Cadence {
        Cadence::Daily
    }

    async fn scan(&self, ctx: &ScanCtx) -> anyhow::Result<Vec<ProactiveSignal>> {
        let today = Utc::now().date_naive();
        let mut out = Vec::new();

        for page in parse_people_dir(&ctx.wiki.people_dir()) {
            let Some(block) = &page.commitments_block else {
                continue;
            };
            let name = page.display_name();
            for item in unchecked_overdue_items(block, today) {
                let overdue_by = (today - item.due).num_days();
                let urgency = if overdue_by >= 14 {
                    Urgency::High
                } else {
                    Urgency::Normal
                };
                let headline = format!("Overdue commitment to {name}");
                let detail = format!(
                    "\u{201c}{text}\u{201d} was due {due} ({overdue_by}d ago). \
                     Source: `people/{slug}.md`.",
                    text = item.text,
                    due = item.due,
                    slug = page.slug,
                );
                let dedup = format!(
                    "stale_commitment:{}:{}",
                    page.slug,
                    short_hash(&item.text)
                );
                out.push(
                    ProactiveSignal::new(
                        SignalKind::StaleCommitment,
                        urgency,
                        headline,
                        detail,
                        dedup,
                    )
                    .with_person(&page.slug)
                    .with_action(SuggestedAction {
                        label: format!("Draft a follow-up to {name}"),
                        draft_prompt: Some(format!(
                            "You owe {name}: \u{201c}{text}\u{201d}. It was due {due}. Draft a \
                             brief, accountable message. Context in `people/{slug}.md`.",
                            text = item.text,
                            due = item.due,
                            slug = page.slug,
                        )),
                    }),
                );
            }
        }

        Ok(out)
    }
}

struct OverdueItem {
    text: String,
    due: NaiveDate,
}

fn unchecked_overdue_items(block: &str, today: NaiveDate) -> Vec<OverdueItem> {
    let mut out = Vec::new();
    for line in block.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) else {
            continue;
        };
        let rest = rest.trim_start();
        let body = match rest.strip_prefix("[ ] ").or_else(|| rest.strip_prefix("[ ]")) {
            Some(b) => b,
            None => continue,
        };
        let Some(due) = parse_due(body) else {
            continue;
        };
        if due >= today {
            continue;
        }
        let text = strip_due(body).trim().to_string();
        if text.is_empty() {
            continue;
        }
        out.push(OverdueItem { text, due });
    }
    out
}

fn parse_due(s: &str) -> Option<NaiveDate> {
    let lower = s.to_ascii_lowercase();
    let idx = lower.find("(due:")?;
    let after = &s[idx + "(due:".len()..];
    let end = after.find(')')?;
    let date_str = after[..end].trim();
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(date_str, "%Y/%m/%d"))
        .ok()
}

fn strip_due(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if let Some(idx) = lower.find("(due:") {
        let after = &s[idx..];
        if let Some(end) = after.find(')') {
            let mut out = String::with_capacity(s.len());
            out.push_str(&s[..idx]);
            out.push_str(&after[end + 1..]);
            return out;
        }
    }
    s.to_string()
}

fn short_hash(s: &str) -> String {
    let mut h: u64 = 1469598103934665603;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_wiki::WikiLayout;

    fn ctx_with(body: &str) -> (tempfile::TempDir, tempfile::TempDir, ScanCtx) {
        let (dbd, store) = crate::testutil::test_store();
        let wd = tempfile::tempdir().unwrap();
        let layout = WikiLayout::new(wd.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("sam@x.com"), body).unwrap();
        let ctx = ScanCtx::new(store, wd.path().to_path_buf(), 0);
        (dbd, wd, ctx)
    }

    #[tokio::test]
    async fn fires_for_overdue_unchecked_item() {
        let body = "---\nname: Sam\n---\n\n# Sam\n\n## Commitments\n- [ ] send the deck (due: 2020-01-01)\n- [x] done thing (due: 2019-01-01)\n- [ ] no due date here\n";
        let (_a, _b, ctx) = ctx_with(body);
        let sigs = StaleCommitmentScan.scan(&ctx).await.unwrap();
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].detail.contains("send the deck"));
        assert_eq!(sigs[0].kind, SignalKind::StaleCommitment);
        assert_eq!(sigs[0].urgency, Urgency::High);
    }

    #[tokio::test]
    async fn ignores_future_due_dates() {
        let future = (Utc::now().date_naive() + chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string();
        let body = format!(
            "---\nname: Sam\n---\n# Sam\n\n## Commitments\n- [ ] later (due: {future})\n"
        );
        let (_a, _b, ctx) = ctx_with(&body);
        assert!(StaleCommitmentScan.scan(&ctx).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_commitments_section_is_silent() {
        let body = "---\nname: Sam\n---\n# Sam\n\n## Tone\nwarm\n";
        let (_a, _b, ctx) = ctx_with(body);
        assert!(StaleCommitmentScan.scan(&ctx).await.unwrap().is_empty());
    }

    #[test]
    fn parse_due_handles_both_separators() {
        assert!(parse_due("text (due: 2026-01-02)").is_some());
        assert!(parse_due("text (due:2026/01/02)").is_some());
        assert!(parse_due("no due clause").is_none());
    }

    #[test]
    fn strip_due_removes_clause() {
        assert_eq!(
            strip_due("send deck (due: 2026-01-02) please").trim(),
            "send deck  please".trim()
        );
    }
}
