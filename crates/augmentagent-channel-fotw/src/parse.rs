//! Parse one exported meeting file (#917).
//!
//! The contract is FlyOnTheWall's `MeetingDoc::to_markdown()`: a YAML
//! frontmatter block, then `# Title`, then the current summary, then optional
//! `## Notes`, `## Action items` and `## Transcript` sections. Scalars are
//! emitted through its `yaml_scalar`, so most arrive double-quoted.
//!
//! Deliberately a hand-rolled reader rather than a YAML dependency: the
//! producer is one known function emitting a fixed key set, and a parser that
//! accepts *less* is the right failure here — a file we cannot read is skipped
//! with a warning, never half-ingested.

use std::collections::BTreeMap;

/// A parsed meeting file. Only the fields this crate acts on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MeetingDoc {
    /// Frontmatter `id` — a UUID, stable across re-pushes, so it is the dedup key.
    pub id: String,
    pub title: String,
    /// `YYYY-MM-DD`, as exported.
    pub date: String,
    /// Epoch milliseconds. Absolute, so the calendar join needs no timezone.
    pub started_at_ms: i64,
    /// Wall-clock length in milliseconds, from the `HH:MM:SS` field.
    pub duration_ms: i64,
    pub timezone: String,
    /// Frontmatter `attendees:` — empty in practice today (diarisation yields
    /// `S0`/`S1`, not names), which is why the calendar join exists.
    pub attendees: Vec<String>,
    pub tags: Vec<String>,
    /// §11's consent flag, carried through the export.
    pub disclosed: bool,
    /// The prose between the `# Title` heading and the first `##` section.
    pub summary: String,
    /// `## Notes` — the user's own words. Parsed so it can be *excluded*.
    pub notes: String,
    /// `## Action items`, one per line, `- [ ] ` markers stripped.
    pub action_items: Vec<String>,
    /// `## Transcript` — parsed only so [`crate::distill`] can prove it is
    /// never carried into a prompt.
    pub transcript: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("no YAML frontmatter block")]
    NoFrontmatter,
    #[error("frontmatter has no `id`")]
    NoId,
    #[error("not a meeting transcript (type: {0})")]
    NotAMeeting(String),
}

/// Strip one layer of `yaml_scalar` quoting.
fn unquote(raw: &str) -> String {
    let t = raw.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        t.to_string()
    }
}

/// `HH:MM:SS` → milliseconds. Saturating and lenient: a malformed duration is
/// zero, never a parse failure, because duration is not load-bearing for
/// ingestion — only for the calendar window, which tolerates a zero span.
fn hms_to_ms(raw: &str) -> i64 {
    let s = unquote(raw);
    let mut parts = s.split(':').map(|p| p.trim().parse::<i64>().unwrap_or(0));
    let h = parts.next().unwrap_or(0);
    let m = parts.next().unwrap_or(0);
    let sec = parts.next().unwrap_or(0);
    ((h * 3600) + (m * 60) + sec).max(0) * 1000
}

/// Split `---\n…\n---\n` off the front. Returns (frontmatter, body).
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some((&rest[..end], &rest[end + 5..]))
}

/// Frontmatter → key/value plus the block-list keys.
fn read_frontmatter(fm: &str) -> (BTreeMap<String, String>, BTreeMap<String, Vec<String>>) {
    let mut scalars = BTreeMap::new();
    let mut lists: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current_list: Option<String> = None;
    for line in fm.lines() {
        // A block-list item belongs to the key that opened the list.
        if let Some(item) = line.strip_prefix("  - ") {
            if let Some(key) = &current_list {
                lists.entry(key.clone()).or_default().push(unquote(item));
            }
            continue;
        }
        // `generated:` opens a nested map whose `  by:`/`  at:` we ignore.
        if line.starts_with("  ") {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            current_list = None;
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim();
        if value.is_empty() {
            // Either an empty block list or a nested map; both are recorded as
            // an empty list so `attendees:` with no items reads as "none".
            lists.entry(key.clone()).or_default();
            current_list = Some(key);
        } else {
            scalars.insert(key, unquote(value));
            current_list = None;
        }
    }
    (scalars, lists)
}

/// Body → (summary, sections). The summary is everything between the `#`
/// title and the first `##`; sections are keyed by their `##` heading.
fn read_sections(body: &str) -> (String, BTreeMap<String, String>) {
    let mut summary = String::new();
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        if let Some(h2) = line.strip_prefix("## ") {
            current = Some(h2.trim().to_string());
            sections.entry(h2.trim().to_string()).or_default();
            continue;
        }
        // The document's own `# Title` is already in the frontmatter.
        if line.starts_with("# ") && current.is_none() {
            continue;
        }
        match &current {
            Some(key) => {
                let buf = sections.entry(key.clone()).or_default();
                buf.push_str(line);
                buf.push('\n');
            }
            None => {
                summary.push_str(line);
                summary.push('\n');
            }
        }
    }
    (
        summary.trim().to_string(),
        sections
            .into_iter()
            .map(|(k, v)| (k, v.trim().to_string()))
            .collect(),
    )
}

/// Parse one exported meeting file.
///
/// # Errors
///
/// [`ParseError`] when the file is not a readable meeting transcript. Callers
/// warn and skip: a half-synced working tree is a normal state, and one bad
/// file must never stall the feed.
pub fn parse_meeting_file(text: &str) -> Result<MeetingDoc, ParseError> {
    let (fm, body) = split_frontmatter(text).ok_or(ParseError::NoFrontmatter)?;
    let (scalars, lists) = read_frontmatter(fm);

    // `type:` is a late addition to the exporter: most of the meetings in a
    // real repo predate it and carry no discriminator at all. So a wrong
    // `type` is a rejection, and a *missing* one is not — what actually
    // separates a meeting from the OKF sidecars is that `index.md` declares
    // `okf_version` and has no `id`, and `log.md` has no frontmatter to read.
    // Found by running the scan against the live repo, where the strict form
    // silently accepted two files out of twenty-two.
    match scalars.get("type").map(String::as_str) {
        Some("meeting-transcript") | None => {}
        Some(other) => return Err(ParseError::NotAMeeting(other.to_string())),
    }
    if scalars.contains_key("okf_version") {
        return Err(ParseError::NotAMeeting("okf-sidecar".to_string()));
    }
    let id = scalars.get("id").cloned().unwrap_or_default();
    if id.is_empty() {
        return Err(ParseError::NoId);
    }

    let (summary, sections) = read_sections(body);
    let action_items = sections
        .get("Action items")
        .map(|block| {
            block
                .lines()
                .filter_map(|l| {
                    let t = l.trim();
                    let t = t.strip_prefix("- ")?;
                    // `- [ ] ` / `- [x] ` checkbox markers are noise to the LLM.
                    let t = t
                        .strip_prefix("[ ] ")
                        .or_else(|| t.strip_prefix("[x] "))
                        .unwrap_or(t);
                    (!t.trim().is_empty()).then(|| t.trim().to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(MeetingDoc {
        id,
        title: scalars.get("title").cloned().unwrap_or_default(),
        date: scalars.get("date").cloned().unwrap_or_default(),
        started_at_ms: scalars
            .get("started_at_ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        duration_ms: scalars.get("duration").map(|d| hms_to_ms(d)).unwrap_or(0),
        timezone: scalars.get("timezone").cloned().unwrap_or_default(),
        attendees: lists.get("attendees").cloned().unwrap_or_default(),
        tags: lists.get("tags").cloned().unwrap_or_default(),
        // Absent reads as `false`: never claim disclosure that was not recorded.
        disclosed: scalars
            .get("disclosed")
            .map(|v| v == "true")
            .unwrap_or(false),
        summary,
        notes: sections.get("Notes").cloned().unwrap_or_default(),
        action_items,
        transcript: sections.get("Transcript").cloned().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape FlyOnTheWall writes, taken from a real exported file:
    /// quoted scalars, an empty `attendees:` block, `disclosed: false`, and a
    /// summary that opens with the `> [!WARNING]` grounding banner (#84).
    const REAL: &str = r#"---
type: meeting-transcript
id: "01a05b42-e36c-74b0-aa79-246017e1e650"
title: "Azure cost reduction and Kubernetes migration plan"
description: "> [!WARNING]"
date: "2026-09-01"
started_at_ms: 1788233602823
duration: "01:05:02"
timezone: "America/New_York"
attendees:
tags:
folder: ""
disclosed: false
generated:
  by: "claude-cli claude-cli"
  at: "2026-09-01"
---

# Azure cost reduction and Kubernetes migration plan

> [!WARNING]
> Grounding was low for this summary.

The team agreed to move the batch workloads off Azure VMs.

## Notes

- ask Priya about the reserved instances

## Action items

- [ ] Priya to price out the reserved instances — Priya
- [x] Draft the migration RFC

## Transcript

- [00:00:01] S0: So the bill came in at forty thousand.
- [00:00:09] S1: That is double last quarter.
"#;

    #[test]
    fn a_real_meeting_file_parses() {
        let doc = parse_meeting_file(REAL).expect("the live export shape must parse");
        assert_eq!(doc.id, "01a05b42-e36c-74b0-aa79-246017e1e650");
        assert_eq!(
            doc.title,
            "Azure cost reduction and Kubernetes migration plan"
        );
        assert_eq!(doc.date, "2026-09-01");
        assert_eq!(doc.started_at_ms, 1_788_233_602_823);
        // 01:05:02 → 3902s. The calendar join reads this, so it must be exact.
        assert_eq!(doc.duration_ms, 3_902_000);
        assert_eq!(doc.timezone, "America/New_York");
        assert!(!doc.disclosed);
        // Empty in practice — the fact that motivates the calendar join (#920).
        assert!(doc.attendees.is_empty());
    }

    #[test]
    fn the_sections_are_split_at_their_headings() {
        let doc = parse_meeting_file(REAL).unwrap();
        assert!(doc.summary.contains("move the batch workloads off Azure"));
        // The admonition is part of the summary and must survive: it is the
        // caveat the reader most needs.
        assert!(doc.summary.contains("[!WARNING]"));
        // Section content must not bleed into the summary.
        assert!(!doc.summary.contains("reserved instances"));
        assert!(!doc.summary.contains("00:00:01"));

        assert_eq!(doc.notes, "- ask Priya about the reserved instances");
        assert_eq!(
            doc.action_items,
            vec![
                "Priya to price out the reserved instances — Priya".to_string(),
                "Draft the migration RFC".to_string(),
            ],
            "checkbox markers are stripped, both states kept"
        );
        assert!(doc.transcript.contains("[00:00:01] S0:"));
    }

    /// Real data, found by running the scan against the live repo: exports
    /// written before FlyOnTheWall added `type:` carry no discriminator at all.
    /// Twenty of the twenty-two meetings in the repo today are this shape, and
    /// rejecting them would have silently ingested only the newest two.
    #[test]
    fn a_legacy_export_without_a_type_field_still_parses() {
        let legacy = "---\nid: \"01a043fb-db59-72a0-bc92-f4f64d10fe44\"\ntitle: \"Untitled recording — 1787846974\"\ndate: \"2026-08-27\"\nstarted_at_ms: 1787843123804\nduration: \"01:04:10\"\ntimezone: \"America/New_York\"\nattendees:\ntags:\nfolder: \"\"\ndisclosed: false\n---\n\n# Untitled recording — 1787846974\n\n## Transcript\n\n- [00:00:00] S0: I feel like I'll Jack\n";
        let doc = parse_meeting_file(legacy).expect("a pre-`type` export is still a meeting");
        assert_eq!(doc.id, "01a043fb-db59-72a0-bc92-f4f64d10fe44");
        assert_eq!(doc.duration_ms, 3_850_000);
        assert!(doc.summary.is_empty(), "no summary was generated for it");
    }

    #[test]
    fn okf_index_and_log_are_not_meetings() {
        // What FlyOnTheWall writes beside the meetings; they must not ingest.
        let index = "---\nokf_version: \"0.2\"\n---\n\n# Meeting transcripts\n\n- [A](./a.md) — 2026-09-01\n";
        assert_eq!(
            parse_meeting_file(index),
            Err(ParseError::NotAMeeting("okf-sidecar".to_string()))
        );
        let log = "# Change log\n\n## 2026-09-01\n\n- Added [A](./a.md)\n";
        assert_eq!(parse_meeting_file(log), Err(ParseError::NoFrontmatter));
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_panic() {
        // A half-written file is a normal state mid-sync.
        assert_eq!(parse_meeting_file(""), Err(ParseError::NoFrontmatter));
        assert_eq!(
            parse_meeting_file("---\ntype: meeting-transcript\n"),
            Err(ParseError::NoFrontmatter),
            "an unterminated frontmatter block is not a document"
        );
        assert_eq!(
            parse_meeting_file("---\ntype: meeting-transcript\nid: \"\"\n---\n\n# x\n"),
            Err(ParseError::NoId),
            "no id means no dedup key, so there is nothing safe to do with it"
        );
    }

    #[test]
    fn a_meeting_with_no_summary_and_no_transcript_still_parses() {
        // FlyOnTheWall pushes transcript-only after its 30-minute enrichment
        // grace, and a recording with no speech has neither.
        let thin = "---\ntype: meeting-transcript\nid: \"abc\"\ntitle: \"Untitled recording\"\ndate: \"2026-08-27\"\nstarted_at_ms: 1000\nduration: \"00:00:12\"\n---\n\n# Untitled recording\n";
        let doc = parse_meeting_file(thin).unwrap();
        assert_eq!(doc.id, "abc");
        assert_eq!(doc.duration_ms, 12_000);
        assert!(doc.summary.is_empty());
        assert!(doc.action_items.is_empty());
        assert!(doc.transcript.is_empty());
    }

    #[test]
    fn lists_and_flags_are_read() {
        let with = "---\ntype: meeting-transcript\nid: \"x\"\ntitle: \"T\"\nattendees:\n  - \"Dana Reyes\"\n  - \"Sam\"\ntags:\n  - \"client\"\ndisclosed: true\nduration: \"00:30:00\"\n---\n\n# T\n\nprose\n";
        let doc = parse_meeting_file(with).unwrap();
        assert_eq!(doc.attendees, vec!["Dana Reyes", "Sam"]);
        assert_eq!(doc.tags, vec!["client"]);
        assert!(doc.disclosed);
        assert_eq!(doc.duration_ms, 1_800_000);
    }

    #[test]
    fn a_malformed_duration_is_zero_rather_than_a_failure() {
        // Duration only feeds the calendar window, which tolerates zero.
        assert_eq!(hms_to_ms("\"garbage\""), 0);
        assert_eq!(hms_to_ms("\"00:00:00\""), 0);
        assert_eq!(hms_to_ms("\"99:59:59\""), 359_999_000);
    }
}
