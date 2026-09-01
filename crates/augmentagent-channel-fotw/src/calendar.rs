//! Recover calendar rosters from rows the gcal channel already wrote (#920).
//!
//! The calendar channel fans out one synthetic email per attendee, keyed
//! `gcal:{event_id}:{attendee_email}`, carrying `From: Display Name <email>`
//! and the event start in `receivedAt`. That means the roster for every event
//! the daemon has ever seen is already in the `emails` table — no Google API
//! call, no second OAuth path, no new failure mode. This module is the pure
//! half: rows in, events and rosters out.
//!
//! # Why events are points, not spans
//!
//! Those rows do not carry the event's *end*. Rather than invent a duration —
//! a guessed 60 minutes would manufacture confident matches for meetings that
//! ran 15 — an event is modelled as an instant at its start. The overlap test
//! in [`crate::match_event`] then asks "did the recording span, or nearly span,
//! the moment this meeting began", which is exactly the question the data can
//! answer. A recording of a real meeting always contains its start; a recording
//! three hours later never does.

use std::collections::BTreeMap;

use crate::distill::RosterMember;
use crate::match_event::EventWindow;

/// One `emails` row from the gcal channel, as this module needs it.
#[derive(Debug, Clone)]
pub struct GcalRow {
    /// `gcal:{event_id}:{attendee_email}`.
    pub message_id: String,
    /// `Display Name <email>`, or a bare address.
    pub from: String,
    /// The event start, RFC3339, as the channel wrote it.
    pub received_at: String,
}

/// An event and everyone invited to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRoster {
    pub window: EventWindow,
    pub roster: Vec<RosterMember>,
}

/// Split `gcal:{event_id}:{email}`. The event id may itself contain colons —
/// Google's recurring-instance ids do — so the *last* colon is the separator,
/// and only if what follows looks like an address.
fn split_key(message_id: &str) -> Option<(String, String)> {
    let rest = message_id.strip_prefix("gcal:")?;
    let (event, email) = rest.rsplit_once(':')?;
    if event.is_empty() || !email.contains('@') {
        return None;
    }
    Some((event.to_string(), email.to_string()))
}

/// `Display Name <email>` → the display name, when there is one.
fn display_name(from: &str) -> Option<String> {
    let (name, _) = from.split_once('<')?;
    let name = name.trim().trim_matches('"').trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// RFC3339 → epoch milliseconds, without a date library.
///
/// Deliberately strict: anything it cannot read is dropped rather than guessed,
/// because a mis-parsed timestamp is a wrong match, and a wrong match cites the
/// wrong people. Handles `Z` and `±HH:MM` offsets, which is everything
/// `DateTime<Utc>::to_rfc3339` emits.
pub fn rfc3339_to_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days since the Unix epoch (Howard Hinnant's civil-from-days).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut ms = ((days * 86_400) + h * 3600 + mi * 60 + sec) * 1000;

    // Trailing offset, if any.
    let tail = &s[19..];
    let tail = tail.split(['+', '-']).next().unwrap_or("");
    let off_start = 19 + tail.len();
    if let Some(sign) = s.as_bytes().get(off_start) {
        if *sign == b'+' || *sign == b'-' {
            let oh = s.get(off_start + 1..off_start + 3)?.parse::<i64>().ok()?;
            let om = s.get(off_start + 4..off_start + 6)?.parse::<i64>().ok()?;
            let offset = (oh * 3600 + om * 60) * 1000;
            ms += if *sign == b'+' { -offset } else { offset };
        }
    }
    Some(ms)
}

/// Group attendee rows into events with rosters.
///
/// Rows that cannot be read — an unexpected key shape, an unparseable date —
/// are dropped silently: they are the calendar channel's rows, not ours, and a
/// shape change there must degrade this feature rather than break the daemon.
#[must_use]
pub fn rosters_from_rows(rows: &[GcalRow]) -> Vec<EventRoster> {
    let mut by_event: BTreeMap<String, (i64, Vec<RosterMember>)> = BTreeMap::new();
    for row in rows {
        let Some((event_id, email)) = split_key(&row.message_id) else {
            continue;
        };
        let Some(start_ms) = rfc3339_to_ms(&row.received_at) else {
            continue;
        };
        let entry = by_event.entry(event_id).or_insert((start_ms, Vec::new()));
        if entry
            .1
            .iter()
            .any(|m: &RosterMember| m.email.eq_ignore_ascii_case(&email))
        {
            continue;
        }
        entry.1.push(RosterMember {
            email,
            display_name: display_name(&row.from),
            // The channel does not persist RSVP on the attendee row; absent
            // rather than invented.
            response_status: None,
        });
    }
    by_event
        .into_iter()
        .map(|(event_id, (start_ms, roster))| EventRoster {
            window: EventWindow {
                event_id,
                start_ms,
                end_ms: start_ms,
            },
            roster,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, from: &str, at: &str) -> GcalRow {
        GcalRow {
            message_id: id.into(),
            from: from.into(),
            received_at: at.into(),
        }
    }

    #[test]
    fn rfc3339_parses_what_the_channel_writes() {
        // chrono's to_rfc3339 on a Utc datetime.
        assert_eq!(
            rfc3339_to_ms("2026-09-01T14:00:00+00:00"),
            Some(1_788_271_200_000)
        );
        assert_eq!(
            rfc3339_to_ms("2026-09-01T14:00:00Z"),
            Some(1_788_271_200_000)
        );
        // An offset is applied, not ignored — this is the bug that would put a
        // meeting five hours from where it happened.
        assert_eq!(
            rfc3339_to_ms("2026-09-01T10:00:00-04:00"),
            rfc3339_to_ms("2026-09-01T14:00:00Z")
        );
        assert_eq!(
            rfc3339_to_ms("2026-09-01T16:00:00+02:00"),
            rfc3339_to_ms("2026-09-01T14:00:00Z")
        );
        // Fractional seconds ride along without shifting the value.
        assert_eq!(
            rfc3339_to_ms("2026-09-01T14:00:00.123Z"),
            rfc3339_to_ms("2026-09-01T14:00:00Z")
        );
    }

    #[test]
    fn the_epoch_and_leap_years_are_right() {
        assert_eq!(rfc3339_to_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_to_ms("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        assert_eq!(rfc3339_to_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
    }

    #[test]
    fn garbage_dates_are_dropped_not_guessed() {
        for bad in [
            "",
            "not a date",
            "2026-09-01",
            "2026-13-01T00:00:00Z",
            "2026-09-01T99:00:00Z",
        ] {
            assert_eq!(rfc3339_to_ms(bad), None, "{bad} must not parse");
        }
    }

    #[test]
    fn attendee_rows_group_into_one_event_with_a_roster() {
        let rows = vec![
            row(
                "gcal:evt-1:priya@example.com",
                "Priya Raman <priya@example.com>",
                "2026-09-01T14:00:00Z",
            ),
            row(
                "gcal:evt-1:sam@example.com",
                "Sam <sam@example.com>",
                "2026-09-01T14:00:00Z",
            ),
            row(
                "gcal:evt-2:dana@example.com",
                "dana@example.com",
                "2026-09-02T09:00:00Z",
            ),
        ];
        let out = rosters_from_rows(&rows);
        assert_eq!(out.len(), 2);
        let one = out.iter().find(|e| e.window.event_id == "evt-1").unwrap();
        assert_eq!(one.roster.len(), 2);
        assert_eq!(one.roster[0].email, "priya@example.com");
        assert_eq!(one.roster[0].display_name.as_deref(), Some("Priya Raman"));
        assert_eq!(one.window.start_ms, 1_788_271_200_000);
        // No end is stored, so the event is an instant — never a guessed span.
        assert_eq!(one.window.end_ms, one.window.start_ms);

        let two = out.iter().find(|e| e.window.event_id == "evt-2").unwrap();
        assert_eq!(
            two.roster[0].display_name, None,
            "a bare address has no name"
        );
    }

    #[test]
    fn a_recurring_event_id_containing_colons_survives() {
        // Google recurring-instance ids look like `abc123_20260901T140000Z`,
        // and some carry colons. The last colon is the separator.
        let rows = vec![row(
            "gcal:abc:123_20260901T140000Z:priya@example.com",
            "Priya <priya@example.com>",
            "2026-09-01T14:00:00Z",
        )];
        let out = rosters_from_rows(&rows);
        assert_eq!(out[0].window.event_id, "abc:123_20260901T140000Z");
        assert_eq!(out[0].roster[0].email, "priya@example.com");
    }

    #[test]
    fn unreadable_rows_are_dropped_rather_than_breaking_the_batch() {
        let rows = vec![
            row(
                "not-a-gcal-key",
                "x <x@example.com>",
                "2026-09-01T14:00:00Z",
            ),
            row("gcal:evt-1:notanemail", "x", "2026-09-01T14:00:00Z"),
            row(
                "gcal:evt-1:ok@example.com",
                "OK <ok@example.com>",
                "nonsense",
            ),
            row(
                "gcal:evt-2:good@example.com",
                "Good <good@example.com>",
                "2026-09-01T14:00:00Z",
            ),
        ];
        let out = rosters_from_rows(&rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].window.event_id, "evt-2");
    }

    #[test]
    fn a_duplicate_attendee_row_does_not_double_the_roster() {
        let rows = vec![
            row(
                "gcal:e:priya@example.com",
                "Priya <priya@example.com>",
                "2026-09-01T14:00:00Z",
            ),
            row(
                "gcal:e:PRIYA@example.com",
                "Priya <PRIYA@example.com>",
                "2026-09-01T14:00:00Z",
            ),
        ];
        assert_eq!(rosters_from_rows(&rows)[0].roster.len(), 1);
    }

    /// The end-to-end shape of the join, on the matcher this feeds.
    #[test]
    fn a_recording_of_the_meeting_matches_and_a_later_one_does_not() {
        use crate::match_event::{match_event, Match};
        let rows = vec![row(
            "gcal:evt-1:priya@example.com",
            "Priya <priya@example.com>",
            "2026-09-01T14:00:00Z",
        )];
        let events: Vec<EventWindow> = rosters_from_rows(&rows)
            .into_iter()
            .map(|e| e.window)
            .collect();
        let start = 1_788_271_200_000;

        // Recorded from 14:03 for 40 minutes: this is the meeting.
        assert!(matches!(
            match_event(start + 3 * 60_000, 40 * 60_000, &events),
            Match::Single(_)
        ));
        // Recorded at 17:00: it is not.
        assert_eq!(
            match_event(start + 3 * 3_600_000, 20 * 60_000, &events),
            Match::None
        );
    }
}
