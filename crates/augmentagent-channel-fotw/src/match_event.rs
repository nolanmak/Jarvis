//! Which calendar event was this recording (#920)?
//!
//! FlyOnTheWall's `attendees:` is empty in practice — diarisation yields `S0`
//! and `S1`, not names — so without a calendar match the ingest can only link
//! people by names appearing in the summary. A matched event upgrades that to
//! identities: attendee *emails* are the identity index's primary key, so
//! `slug_from_email` resolves them to exact person pages with no guessing.
//!
//! The join is time-overlap, and it is a pure function so it can be tested
//! exhaustively without a calendar, a network or a daemon.
//!
//! # Three answers, never two
//!
//! [`Match::Single`] is the only one that attaches a roster.
//! [`Match::Ambiguous`] is a real state — back-to-back calls, a recording left
//! running across two invites — and guessing between candidates would put the
//! wrong people on a page, which is worse than putting none: a wrong fact is
//! cited, propagates into drafts, and nobody knows to look for it.
//! [`Match::None`] is the ordinary case for ad-hoc recordings, and the reason
//! the name-based path stays.

/// A calendar event reduced to what the join needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWindow {
    pub event_id: String,
    /// Epoch milliseconds, UTC.
    pub start_ms: i64,
    /// Epoch milliseconds, UTC. Equal to `start_ms` for a zero-length event.
    pub end_ms: i64,
}

/// The outcome of the join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Exactly one event overlapped. The only case that attaches a roster.
    Single(EventWindow),
    /// More than one overlapped. Carries the ids so the skip is diagnosable.
    Ambiguous(Vec<String>),
    /// Nothing overlapped — an ad-hoc recording, or a meeting nobody invited.
    None,
}

/// How late a recording may start, or how early it may stop, and still be
/// considered the same meeting.
///
/// Ten minutes because that is what the failure looks like in practice: people
/// join, chat, and someone remembers to hit record. Widening it past this
/// starts merging genuinely adjacent calendar entries, and the cost of a wrong
/// merge (wrong people cited on a page) is much higher than the cost of a miss
/// (fall back to name linking, which is what we did before this existed).
pub const SLOP_MS: i64 = 10 * 60 * 1000;

/// Do a recording and an event overlap, given the slop?
///
/// Half-open in spirit but inclusive at the boundary: an event that ends
/// exactly `SLOP_MS` before the recording starts still matches, because the
/// boundary case is real (a 30-minute invite, a recording started ten minutes
/// in) and excluding it would be arbitrary precision about a fuzzy quantity.
fn overlaps(rec_start: i64, rec_end: i64, ev: &EventWindow) -> bool {
    // Zero-length recordings and events are points, and a point inside the
    // other's window is a match — a recording that captured nothing still
    // happened during the call.
    rec_start <= ev.end_ms.saturating_add(SLOP_MS) && ev.start_ms <= rec_end.saturating_add(SLOP_MS)
}

/// Find the calendar event a recording belongs to.
///
/// `duration_ms` may be zero (a recording with no measured length); the
/// recording is then treated as an instant, which still matches an event
/// containing it.
#[must_use]
pub fn match_event(started_at_ms: i64, duration_ms: i64, events: &[EventWindow]) -> Match {
    let rec_end = started_at_ms.saturating_add(duration_ms.max(0));
    let hits: Vec<&EventWindow> = events
        .iter()
        .filter(|ev| overlaps(started_at_ms, rec_end, ev))
        .collect();
    match hits.len() {
        0 => Match::None,
        1 => Match::Single(hits[0].clone()),
        _ => Match::Ambiguous(hits.into_iter().map(|e| e.event_id.clone()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    /// 2026-09-01T14:00:00Z, an arbitrary but fixed instant.
    const NOON: i64 = 1_788_256_800_000;

    fn ev(id: &str, start_min: i64, len_min: i64) -> EventWindow {
        EventWindow {
            event_id: id.to_string(),
            start_ms: NOON + start_min * MIN,
            end_ms: NOON + (start_min + len_min) * MIN,
        }
    }

    #[test]
    fn an_exact_overlap_matches() {
        let events = vec![ev("a", 0, 60)];
        assert_eq!(
            match_event(NOON, 60 * MIN, &events),
            Match::Single(events[0].clone())
        );
    }

    #[test]
    fn a_late_start_within_slop_matches() {
        // The ordinary case: a 14:00 invite, recording started at 14:08.
        let events = vec![ev("a", 0, 60)];
        assert!(matches!(
            match_event(NOON + 8 * MIN, 45 * MIN, &events),
            Match::Single(_)
        ));
    }

    #[test]
    fn a_recording_wholly_before_the_invite_still_matches_inside_the_slop() {
        // Started early — walked into the room and hit record.
        let events = vec![ev("a", 0, 60)];
        assert!(matches!(
            match_event(NOON - 9 * MIN, 5 * MIN, &events),
            Match::Single(_)
        ));
    }

    #[test]
    fn the_slop_boundary_is_inclusive_and_stops_there() {
        let events = vec![ev("a", 0, 60)];
        // Recording begins exactly SLOP after the event ended: still a match.
        let at = NOON + 60 * MIN + SLOP_MS;
        assert!(matches!(match_event(at, 0, &events), Match::Single(_)));
        // One millisecond further out: not this meeting.
        assert_eq!(match_event(at + 1, 0, &events), Match::None);
    }

    #[test]
    fn two_candidate_events_are_ambiguous_not_a_guess() {
        // Back-to-back calls with a recording spanning both. Picking one would
        // cite the wrong people, which is worse than citing none.
        let events = vec![ev("a", 0, 30), ev("b", 30, 30)];
        match match_event(NOON, 60 * MIN, &events) {
            Match::Ambiguous(ids) => assert_eq!(ids, vec!["a", "b"]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn adjacent_meetings_are_ambiguous_through_the_slop_alone() {
        // Nothing overlaps in wall-clock terms, but a 5-minute recording at the
        // seam is within slop of both. The slop must not manufacture a
        // confident single match here.
        let events = vec![ev("a", 0, 30), ev("b", 32, 30)];
        assert!(matches!(
            match_event(NOON + 31 * MIN, 0, &events),
            Match::Ambiguous(_)
        ));
    }

    #[test]
    fn no_overlap_is_none() {
        let events = vec![ev("a", 0, 30)];
        // Three hours later: an ad-hoc recording, the ordinary case.
        assert_eq!(
            match_event(NOON + 180 * MIN, 20 * MIN, &events),
            Match::None
        );
        // And with no calendar at all.
        assert_eq!(match_event(NOON, 20 * MIN, &[]), Match::None);
    }

    #[test]
    fn zero_length_recordings_and_events_do_not_panic() {
        let point = EventWindow {
            event_id: "p".into(),
            start_ms: NOON,
            end_ms: NOON,
        };
        assert!(matches!(
            match_event(NOON, 0, std::slice::from_ref(&point)),
            Match::Single(_)
        ));
        // A recording with no measured duration inside a real event.
        assert!(matches!(
            match_event(NOON + 5 * MIN, 0, &[ev("a", 0, 60)]),
            Match::Single(_)
        ));
    }

    #[test]
    fn extreme_timestamps_saturate_rather_than_overflow() {
        let events = vec![ev("a", 0, 60)];
        assert_eq!(match_event(i64::MAX, i64::MAX, &events), Match::None);
        let huge = EventWindow {
            event_id: "h".into(),
            start_ms: i64::MIN,
            end_ms: i64::MAX,
        };
        assert!(matches!(match_event(NOON, 0, &[huge]), Match::Single(_)));
    }

    #[test]
    fn a_negative_duration_is_treated_as_an_instant() {
        // Defensive: `duration_ms` comes from a parsed field.
        let events = vec![ev("a", 0, 60)];
        assert!(matches!(
            match_event(NOON + 5 * MIN, -1, &events),
            Match::Single(_)
        ));
    }
}
