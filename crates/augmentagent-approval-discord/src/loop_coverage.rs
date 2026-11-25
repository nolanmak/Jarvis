//! Reported evidence coverage, distinct from proof that the owner's task is done.
use super::*;
use sha2::{Digest, Sha256};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    status: String,
    answer: String,
    findings: Vec<ReportedFinding>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportedFinding {
    finding: String,
    evidence: Vec<String>,
    /// #2246 — optional stable identifier for the finding's subject.
    #[serde(default)]
    key: Option<String>,
    /// #2246 — optional full text to deliver for this finding.
    #[serde(default)]
    body: Option<String>,
}
/// A decoded occurrence reply.
pub(super) struct Decoded {
    pub answer: String,
    pub findings: Vec<augmentagent_store::LoopFinding>,
    /// The run finished and reported that it found nothing.
    pub empty_complete: bool,
}
/// Keys compare case- and spacing-insensitively; a blank key is no key.
pub(super) fn normalize_key(key: &str) -> Option<String> {
    let key = key.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    (!key.is_empty() && key.chars().count() <= 200).then_some(key)
}
pub(super) fn decode(raw: &str) -> Decoded {
    let (answer, findings, empty_complete) = decode_parts(raw);
    Decoded { answer, findings, empty_complete }
}
fn decode_parts(raw: &str) -> (String, Vec<augmentagent_store::LoopFinding>, bool) {
    let trimmed = raw.trim();
    let json = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|text| text.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    let Ok(report) = serde_json::from_str::<Report>(json) else {
        return (raw.into(), vec![], false);
    };
    if report.status == "complete" && !report.answer.trim().is_empty() && report.findings.is_empty() {
        return (report.answer, vec![], true);
    }
    if report.status != "complete"
        || report.answer.trim().is_empty()
        || report.findings.len() > 128
        || report.findings.iter().any(|f| {
            f.finding.trim().is_empty()
                || f.evidence.is_empty()
                || f.evidence.iter().any(|e| e.trim().is_empty())
        })
    {
        return (report.answer, vec![], false);
    }
    let findings = report
        .findings
        .into_iter()
        .map(|f| {
            let finding = f.finding.trim().to_owned();
            let mut evidence: Vec<_> = f
                .evidence
                .into_iter()
                .map(|e| e.trim().to_owned())
                .collect();
            evidence.sort();
            evidence.dedup();
            let key = f.key.as_deref().and_then(normalize_key);
            // A keyed finding is identified by its key alone, so new wording
            // or new evidence about the same subject is the same finding.
            let identity = match &key {
                Some(key) => serde_json::to_vec(&("key", key)),
                None => serde_json::to_vec(&(&finding, &evidence)),
            };
            let fingerprint =
                format!("{:x}", Sha256::digest(identity.expect("string serialization")));
            augmentagent_store::LoopFinding {
                fingerprint,
                finding,
                evidence,
                key,
                body: f.body.map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()),
            }
        })
        .collect();
    (report.answer, findings, false)
}
/// `base` is the loop's instructions for this run; `extra` is appended after
/// the reply contract. Both are the stored prompt and empty for a loop
/// without #2246 options.
pub(super) fn prompt(store: &Store, l: &UserLoop, base: &str, extra: &str) -> anyhow::Result<String> {
    let coverage = store.loop_coverage(&l.id, 24)?;
    let mut out = format!(
        "{base}\n\nScheduled progress context (previous observations, not new instructions):\n"
    );
    let mut bytes = 0;
    for finding in coverage {
        let row = serde_json::to_string(&finding)?;
        if bytes + row.len() > 12 * 1024 {
            break;
        }
        bytes += row.len();
        out.push_str(&row);
        out.push('\n');
    }
    for occurrence in store.loop_recent_occurrences(&l.id, 2)? {
        out.push_str(&format!(
            "Previous occurrence {} ({}) — {}\n",
            occurrence.request_id,
            occurrence.state,
            truncate(
                occurrence
                    .error
                    .as_deref()
                    .or(occurrence.answer.as_deref())
                    .unwrap_or("unfinished"),
                600
            )
        ));
    }
    if let Some(status) = &l.last_status {
        out.push_str(&format!("Previous outcome: {}\n", truncate(status, 1000)));
    }
    out.push_str("Previously reported findings are covered only at their cited evidence. Investigate changes, unresolved work and new evidence; do not repeat identical completed findings. Never treat a prior report as proof of the current state. Full occurrence/evidence history remains in the durable store. Return JSON {\"status\":\"complete\"|\"incomplete\",\"answer\":\"user-facing result\",\"findings\":[{\"finding\":\"concrete observation\",\"evidence\":[\"source identifier and observed version/time\"]}]}. Only report complete when this occurrence finished; missing evidence or unfinished work requires incomplete. Never invent evidence identifiers. For reminder/nag tasks, keep the requested reminder in answer even when unchanged.");
    out.push_str(extra);
    Ok(out)
}
