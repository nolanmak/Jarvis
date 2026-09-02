//! Turn a parsed meeting into the synthetic `Email` the wiki ingest funnel
//! consumes (#918, #919).
//!
//! This is where a transcript becomes knowledge, and it is deliberately the
//! *voice channel's* pattern rather than a new one: synthesize the same
//! `Email` shape, call the same public `spawn_ingest`. Zero changes to the
//! ingest pipeline.
//!
//! # What rides, and what must never
//!
//! The body carries the title, when it happened, the **summary** and the
//! **action items** — the grounded distillation FlyOnTheWall already produced
//! with a model that had the whole meeting in front of it. It does not carry
//! the transcript. Three reasons, in order: a 20k-token transcript per meeting
//! is slow and expensive against a Haiku ingest; re-deriving facts from raw
//! speech invites attribution the summary already got right; and the
//! wiki-skill contract is "never invent, cite sources, pages under 400 lines",
//! which a wall of dialogue actively fights. The words are not lost — they are
//! one `Read` away in the repo clone, which is what #921 opens to the ask path.
//!
//! `## Notes` does not ride either. Those are the operator's own words typed
//! during the call; they are already theirs, and the ingest agent rewriting
//! them into third-person wiki prose is a downgrade.
//!
//! [`no_transcript_line_ever_reaches_the_prompt`](tests) is the structural
//! guarantee, in the same spirit as the calendar channel's description/location
//! leak test.

use augmentagent_store::Email;

use crate::match_event::Match;
use crate::parse::MeetingDoc;
use crate::{KIND, PLATFORM};

/// A calendar attendee, reduced to what a roster line needs. Mirrors the
/// calendar channel's `MeetingAttendee` without depending on that crate — this
/// crate must stay unit-testable with no calendar in the build graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterMember {
    pub email: String,
    pub display_name: Option<String>,
    pub response_status: Option<String>,
}

/// Why a meeting was not ingested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// §11's consent flag is unset on the export. Honored *before* any model
    /// call, so an undisclosed recording never reaches an LLM at all.
    NotDisclosed,
    /// Nothing to record: no summary, no action items. A recording with only a
    /// title tells the wiki nothing a person would want cited.
    Empty,
}

/// Should this meeting be ingested at all?
///
/// `require_disclosed` is the operator's switch. Default on: it gives
/// FlyOnTheWall's consent flag teeth on this side. Off is legitimate for a
/// library recorded before the flag meant anything.
///
/// # Errors
///
/// [`Skip`] with the reason, which the caller logs against the meeting id so a
/// wrongly-skipped meeting is diagnosable rather than merely absent.
pub fn admit(doc: &MeetingDoc, require_disclosed: bool) -> Result<(), Skip> {
    if require_disclosed && !doc.disclosed {
        return Err(Skip::NotDisclosed);
    }
    if doc.summary.trim().is_empty() && doc.action_items.is_empty() {
        return Err(Skip::Empty);
    }
    Ok(())
}

/// Render the roster as body lines, self excluded.
fn roster_block(roster: &[RosterMember], my_email: &str) -> String {
    let mut out = String::new();
    for m in roster
        .iter()
        .filter(|m| !m.email.eq_ignore_ascii_case(my_email))
    {
        let name = m.display_name.as_deref().unwrap_or("");
        let rsvp = m
            .response_status
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        out.push_str(&format!("\n- {name} <{}>{rsvp}", m.email).replace("- <", "- "));
    }
    out
}

/// Build the synthetic email for one meeting.
///
/// `event` is the calendar join's answer: only [`Match::Single`] contributes a
/// roster, because only it identifies people without guessing.
#[must_use]
pub fn synthetic_meeting_email(
    doc: &MeetingDoc,
    event: &Match,
    roster: &[RosterMember],
    my_email: &str,
) -> Email {
    let title = if doc.title.trim().is_empty() {
        "Untitled meeting".to_string()
    } else {
        doc.title.trim().to_string()
    };

    let mut body = String::new();
    let minutes = doc.duration_ms / 60_000;
    body.push_str(&format!(
        "Meeting: {title}\nWhen: {} ({minutes} min)\n",
        doc.date
    ));

    match event {
        Match::Single(ev) => {
            body.push_str(&format!("Calendar event: {}\n", ev.event_id));
            let block = roster_block(roster, my_email);
            if block.is_empty() {
                body.push_str("Invited: (only me)\n");
            } else {
                body.push_str("Invited:");
                body.push_str(&block);
                body.push('\n');
                // The instruction that makes the roster worth attaching: the
                // transcript says "Priya", the roster says which Priya.
                body.push_str(
                    "\nThese are the invited attendees, by email. Attribute names \
                     appearing in the summary to these people where they clearly \
                     correspond, and use the email to identify the right person page.\n",
                );
            }
        }
        Match::Ambiguous(ids) => {
            // Said out loud rather than silently dropped: the model should know
            // attribution here is weaker, and the log line has the same fact.
            body.push_str(&format!(
                "Calendar: {} candidate events, none chosen — attendees unknown.\n",
                ids.len()
            ));
        }
        Match::None => {
            body.push_str("Calendar: no matching event — this was an ad-hoc recording.\n");
        }
    }

    if !doc.summary.trim().is_empty() {
        body.push_str("\nSummary:\n");
        body.push_str(doc.summary.trim());
        body.push('\n');
    } else {
        // Thin, and honest about being thin. A transcript-only push (before
        // FlyOnTheWall's enrichment grace elapses) has no summary yet.
        body.push_str("\nSummary: none was generated for this meeting.\n");
    }

    if !doc.action_items.is_empty() {
        body.push_str("\nAction items:");
        for item in &doc.action_items {
            body.push_str(&format!("\n- {item}"));
        }
        body.push('\n');
    }

    Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: crate::source_id(&doc.id),
        thread_id: None,
        from: format!("{PLATFORM}:recorder"),
        subject: format!("Meeting: {title}"),
        body: body.trim().to_string(),
        date: doc.date.clone(),
        account_entity_id: Some(format!("{PLATFORM}:recorder")),
        platform: PLATFORM.into(),
        kind: KIND.into(),
    }
}

/// The extra instruction appended to the ingest message for this platform.
///
/// The relevance gate (#919). The transcript repo mixes client meetings with
/// personal captures — a recorder left running, a voice note to self — and
/// blind ingestion smears those across person pages. The gate rides *inside*
/// the one Haiku call we already make rather than as a second classifier:
/// cheaper, and it is a judgement the ingest agent is already making when it
/// decides which pages a message touches. The one-line-reply convention is the
/// funnel's own (`"reply with one line: ingested"`).
pub const RELEVANCE_GATE: &str = "\nThis is a recorded meeting, not a message addressed to anyone. If it is a personal capture — a note to self, a recording left running, a conversation with no identifiable people, projects or commitments worth remembering — write nothing at all and reply with one line: \"skipped\".\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_event::EventWindow;
    use crate::parse::parse_meeting_file;

    const REAL: &str = r#"---
type: meeting-transcript
id: "01a05b42"
title: "Azure cost reduction"
date: "2026-09-01"
started_at_ms: 1788233602823
duration: "01:05:02"
disclosed: true
---

# Azure cost reduction

The team agreed to move the batch workloads off Azure VMs.

## Notes

- ask Priya about the reserved instances

## Action items

- [ ] Priya to price out the reserved instances — Priya

## Transcript

- [00:00:01] S0: So the bill came in at forty thousand.
- [00:00:09] S1: That is double last quarter.
"#;

    fn doc() -> MeetingDoc {
        parse_meeting_file(REAL).unwrap()
    }

    #[test]
    fn the_body_carries_the_summary_and_the_action_items() {
        let e = synthetic_meeting_email(&doc(), &Match::None, &[], "me@example.com");
        assert!(e.body.contains("move the batch workloads off Azure"));
        assert!(e.body.contains("Priya to price out the reserved instances"));
        assert!(e.body.contains("Azure cost reduction"));
    }

    /// The structural guarantee. `text/html`-style leaks are not the risk here;
    /// the risk is a 20k-token wall of speech reaching a Haiku ingest and being
    /// re-derived into invented attribution. Same class of test as the calendar
    /// channel's description/location exclusion.
    #[test]
    fn no_transcript_line_ever_reaches_the_prompt() {
        let d = doc();
        assert!(
            !d.transcript.is_empty(),
            "the fixture must have a transcript, or this test proves nothing"
        );
        for event in [
            Match::None,
            Match::Ambiguous(vec!["a".into(), "b".into()]),
            Match::Single(EventWindow {
                event_id: "e".into(),
                start_ms: 0,
                end_ms: 1,
            }),
        ] {
            let e = synthetic_meeting_email(&d, &event, &[], "me@example.com");
            assert!(
                !e.body.contains("the bill came in"),
                "transcript speech reached the body via {event:?}"
            );
            assert!(
                !e.body.contains("[00:00:01]"),
                "a transcript timestamp reached the body via {event:?}"
            );
            assert!(
                !e.body.contains("S0:") && !e.body.contains("S1:"),
                "a diarisation label reached the body via {event:?}"
            );
        }
    }

    #[test]
    fn the_operators_own_notes_do_not_flow() {
        let e = synthetic_meeting_email(&doc(), &Match::None, &[], "me@example.com");
        assert!(
            !e.body.contains("ask Priya about the reserved instances"),
            "`## Notes` are the operator's own words and stay theirs"
        );
    }

    #[test]
    fn the_envelope_dedups_on_the_meeting_id() {
        let e = synthetic_meeting_email(&doc(), &Match::None, &[], "me@example.com");
        assert_eq!(e.message_id, "fotw:01a05b42");
        assert_eq!(e.platform, PLATFORM);
        assert_eq!(e.kind, KIND);
        assert_eq!(e.date, "2026-09-01");
    }

    #[test]
    fn a_single_match_attaches_the_roster_and_excludes_me() {
        let roster = vec![
            RosterMember {
                email: "priya@example.com".into(),
                display_name: Some("Priya Raman".into()),
                response_status: Some("accepted".into()),
            },
            RosterMember {
                email: "me@example.com".into(),
                display_name: Some("Me".into()),
                response_status: Some("accepted".into()),
            },
        ];
        let ev = Match::Single(EventWindow {
            event_id: "evt-1".into(),
            start_ms: 0,
            end_ms: 1,
        });
        let e = synthetic_meeting_email(&doc(), &ev, &roster, "me@example.com");
        assert!(e.body.contains("priya@example.com"));
        assert!(e.body.contains("Priya Raman"));
        assert!(e.body.contains("evt-1"));
        assert!(
            !e.body.contains("me@example.com"),
            "the operator is not an attendee of their own meeting"
        );
    }

    #[test]
    fn ambiguous_and_none_say_so_instead_of_naming_anyone() {
        let roster = vec![RosterMember {
            email: "priya@example.com".into(),
            display_name: None,
            response_status: None,
        }];
        let amb = synthetic_meeting_email(
            &doc(),
            &Match::Ambiguous(vec!["a".into(), "b".into()]),
            &roster,
            "me@example.com",
        );
        assert!(amb.body.contains("2 candidate events"));
        assert!(
            !amb.body.contains("priya@example.com"),
            "an unchosen roster must never be attached"
        );

        let none = synthetic_meeting_email(&doc(), &Match::None, &roster, "me@example.com");
        assert!(none.body.contains("ad-hoc recording"));
        assert!(!none.body.contains("priya@example.com"));
    }

    #[test]
    fn a_summaryless_meeting_is_thin_rather_than_invented() {
        let thin = parse_meeting_file(
            "---\ntype: meeting-transcript\nid: \"x\"\ntitle: \"Untitled recording\"\ndate: \"2026-08-27\"\nduration: \"00:00:40\"\ndisclosed: true\n---\n\n# Untitled recording\n\n## Transcript\n\n- [00:00:01] S0: testing\n",
        )
        .unwrap();
        let e = synthetic_meeting_email(&thin, &Match::None, &[], "me@example.com");
        assert!(e.body.contains("none was generated"));
        assert!(!e.body.contains("testing"));
    }

    #[test]
    fn an_undisclosed_meeting_is_refused_before_any_model_call() {
        let mut d = doc();
        d.disclosed = false;
        assert_eq!(admit(&d, true), Err(Skip::NotDisclosed));
        // …and the operator can switch that off for a pre-flag library.
        assert_eq!(admit(&d, false), Ok(()));
    }

    #[test]
    fn a_meeting_with_nothing_to_say_is_skipped() {
        let mut d = doc();
        d.summary.clear();
        d.action_items.clear();
        assert_eq!(admit(&d, false), Err(Skip::Empty));
        // A transcript alone is not something to record: the words stay in the
        // repo, and the wiki gains nothing citable from a title.
        assert!(!d.transcript.is_empty());
    }

    #[test]
    fn the_relevance_gate_asks_for_the_funnels_own_one_line_reply() {
        assert!(RELEVANCE_GATE.contains("skipped"));
        assert!(RELEVANCE_GATE.contains("personal capture"));
    }
}
