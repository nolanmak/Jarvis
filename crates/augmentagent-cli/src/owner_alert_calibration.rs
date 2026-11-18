//! Offline deterministic gate replay. No Store, providers, transports or live
//! configuration are opened; the previous recorded gate result stays fixed.
use augmentagent_channel_email::owner_alert_gate::{classify, Catalog, Gate};
use augmentagent_store::owner_alert_gate::urgency;
use serde::{Deserialize, Serialize};
use std::io::BufRead;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema: u32,
    since_ms: i64,
    until_ms: i64,
    priorities: Vec<(String, String)>,
    people: Vec<Person>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Person {
    slug: String,
    context: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    from: String,
    body: String,
    before_high: bool,
}
#[derive(Debug, Default, Serialize)]
pub(super) struct Report {
    since_ms: i64,
    until_ms: i64,
    triaged: usize,
    before_high: usize,
    after_high: usize,
    added_priority: usize,
    added_relationship: usize,
    muted: usize,
    before_rate: f64,
    after_rate: f64,
    method: &'static str,
}
pub(super) fn replay(reader: impl BufRead) -> anyhow::Result<Report> {
    let mut lines = reader.lines();
    let header = lines
        .next()
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("missing calibration snapshot"))?;
    let snapshot: Snapshot = serde_json::from_str(&header)
        .map_err(|_| anyhow::anyhow!("invalid calibration snapshot"))?;
    anyhow::ensure!(
        snapshot.schema == 1 && snapshot.until_ms > snapshot.since_ms,
        "unsupported calibration snapshot"
    );
    let temp = tempfile::tempdir()?;
    std::fs::create_dir(temp.path().join("people"))?;
    for p in snapshot.people {
        let canonical = augmentagent_store::owner_alert_gate::canonical_selector(&format!(
            "person:{}",
            p.slug
        ))?;
        let slug = canonical.strip_prefix("person:").unwrap();
        std::fs::write(
            temp.path().join("people").join(format!("{slug}.md")),
            p.context,
        )?;
    }
    let catalog = Catalog::load(Some(temp.path()))?;
    let priorities = snapshot
        .priorities
        .into_iter()
        .map(|(s, u)| (s, urgency(&u)))
        .collect::<Vec<_>>();
    let mut report=Report {since_ms:snapshot.since_ms,until_ms:snapshot.until_ms,
        method:"recorded legacy high/critical outcomes plus deterministic owner gates; no model calls or sends",..Report::default()};
    for (n, line) in lines.enumerate() {
        let line = line?;
        let r: Record = serde_json::from_str(&line)
            .map_err(|_| anyhow::anyhow!("invalid calibration record {}", n + 1))?;
        report.triaged += 1;
        report.before_high += usize::from(r.before_high);
        let after = match classify(&r.from, &r.body, &priorities, &catalog) {
            Gate::Muted => {
                if r.before_high {
                    report.muted += 1;
                }
                false
            }
            Gate::Priority { .. } => {
                if !r.before_high {
                    report.added_priority += 1;
                }
                true
            }
            Gate::Relationship { .. } => {
                if !r.before_high {
                    report.added_relationship += 1;
                }
                true
            }
            Gate::Legacy => r.before_high,
        };
        report.after_high += usize::from(after);
    }
    if report.triaged > 0 {
        report.before_rate = report.before_high as f64 / report.triaged as f64;
        report.after_rate = report.after_high as f64 / report.triaged as f64;
    }
    Ok(report)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_reports_counts_without_private_content_or_sends() {
        let header = serde_json::json!({"schema":1,"since_ms":1,"until_ms":100,"priorities":[["domain:priority.test","high"],["mute@example.test","routine"]],"people":[{"slug":"matt","context":"---\nidentities:\n  email: [matt@example.test, second@example.test]\nrelationship: close-contact\n---\n"}]});
        let records = [
            ("matt@example.test", "can you send me the deck?", false),
            ("second@example.test", "can you send me the deck?", false),
            ("unknown@example.test", "can you send me the deck?", false),
            ("x@priority.test", "FYI", false),
            ("mute@example.test", "please send the deck", true),
            ("legacy@example.test", "please review", true),
        ];
        let mut input = header.to_string();
        for (from, body, before_high) in records {
            input.push('\n');
            input.push_str(
                &serde_json::json!({"from":from,"body":body,"before_high":before_high}).to_string(),
            );
        }
        let r = replay(input.as_bytes()).unwrap();
        assert_eq!(
            (
                r.triaged,
                r.before_high,
                r.after_high,
                r.added_relationship,
                r.added_priority,
                r.muted
            ),
            (6, 2, 4, 2, 1, 1)
        );
        let rendered = serde_json::to_string(&r).unwrap();
        assert!(!rendered.contains("example.test"));
        assert!(!rendered.contains("deck"));
    }
    #[test]
    fn snapshot_cannot_escape_private_wiki_tempdir() {
        let input = r#"{"schema":1,"since_ms":1,"until_ms":2,"priorities":[],"people":[{"slug":"../../escape","context":"secret"}]}"#;
        assert!(replay(input.as_bytes()).is_err());
    }
}
