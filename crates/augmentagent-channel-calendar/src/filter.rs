//! Pre-WorkItem filter for Calendar events.
//!
//! Drops events that aren't real engagements (cancelled, OOO, focus time,
//! solo blocks) before they pay for an LLM ingest. Mirrors §8 of #82.

use crate::types::CalendarEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Cancelled,
    DeclinedBySelf,
    Transparent,
    PrivateOrConfidential,
    BlockingType(String),
    NotEnoughAttendees,
    SubscribedCalendarOrganizer,
}

impl SkipReason {
    pub fn label(&self) -> String {
        match self {
            Self::Cancelled => "cancelled".into(),
            Self::DeclinedBySelf => "declined-by-self".into(),
            Self::Transparent => "transparency=transparent".into(),
            Self::PrivateOrConfidential => "visibility=private/confidential".into(),
            Self::BlockingType(s) => format!("eventType={s}"),
            Self::NotEnoughAttendees => "attendees<2 (excluding self+resources)".into(),
            Self::SubscribedCalendarOrganizer => {
                "organizer is a subscribed @group.calendar.google.com calendar".into()
            }
        }
    }
}

/// Returns `Ok(())` if the event should produce a `WorkItem`, or
/// `Err(reason)` describing why we dropped it.
pub fn passes_filter(ev: &CalendarEvent) -> Result<(), SkipReason> {
    if ev
        .status
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("cancelled"))
        .unwrap_or(false)
    {
        return Err(SkipReason::Cancelled);
    }

    if let Some(attendees) = &ev.attendees {
        if let Some(me) = attendees.iter().find(|a| a.self_.unwrap_or(false)) {
            if me
                .response_status
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("declined"))
                .unwrap_or(false)
            {
                return Err(SkipReason::DeclinedBySelf);
            }
        }
    }

    if ev
        .transparency
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("transparent"))
        .unwrap_or(false)
    {
        return Err(SkipReason::Transparent);
    }

    if let Some(v) = ev.visibility.as_deref() {
        let lower = v.to_ascii_lowercase();
        if lower == "private" || lower == "confidential" {
            return Err(SkipReason::PrivateOrConfidential);
        }
    }

    if let Some(t) = ev.event_type.as_deref() {
        let lower = t.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "outofoffice" | "focustime" | "workinglocation" | "birthday"
        ) {
            return Err(SkipReason::BlockingType(t.to_string()));
        }
    }

    let real_attendees = ev
        .attendees
        .as_ref()
        .map(|list| {
            list.iter()
                .filter(|a| !a.self_.unwrap_or(false))
                .filter(|a| !a.resource.unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    if real_attendees < 1 {
        return Err(SkipReason::NotEnoughAttendees);
    }

    if let Some(org) = &ev.organizer {
        if let Some(email) = &org.email {
            if email.ends_with("@group.calendar.google.com") {
                return Err(SkipReason::SubscribedCalendarOrganizer);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CalendarEvent, EventTime, RawAttendee, RawOrganizer};

    fn base() -> CalendarEvent {
        CalendarEvent {
            id: "x".into(),
            status: Some("confirmed".into()),
            summary: Some("Catch-up".into()),
            start: Some(EventTime {
                date_time: Some("2026-05-14T15:00:00Z".into()),
                ..Default::default()
            }),
            end: Some(EventTime {
                date_time: Some("2026-05-14T15:30:00Z".into()),
                ..Default::default()
            }),
            attendees: Some(vec![
                RawAttendee {
                    email: Some("me@x.com".into()),
                    self_: Some(true),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
                RawAttendee {
                    email: Some("a@y.com".into()),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn passes_normal_two_party_meeting() {
        assert!(passes_filter(&base()).is_ok());
    }

    #[test]
    fn drops_cancelled() {
        let mut e = base();
        e.status = Some("cancelled".into());
        assert_eq!(passes_filter(&e), Err(SkipReason::Cancelled));
    }

    #[test]
    fn drops_when_declined_by_self() {
        let mut e = base();
        if let Some(list) = e.attendees.as_mut() {
            list[0].response_status = Some("declined".into());
        }
        assert_eq!(passes_filter(&e), Err(SkipReason::DeclinedBySelf));
    }

    #[test]
    fn drops_transparent_events() {
        let mut e = base();
        e.transparency = Some("transparent".into());
        assert_eq!(passes_filter(&e), Err(SkipReason::Transparent));
    }

    #[test]
    fn drops_private_visibility() {
        let mut e = base();
        e.visibility = Some("private".into());
        assert_eq!(passes_filter(&e), Err(SkipReason::PrivateOrConfidential));
    }

    #[test]
    fn drops_focus_time() {
        let mut e = base();
        e.event_type = Some("focusTime".into());
        let err = passes_filter(&e).unwrap_err();
        assert!(matches!(err, SkipReason::BlockingType(_)));
    }

    #[test]
    fn drops_solo_block() {
        let mut e = base();
        e.attendees = Some(vec![RawAttendee {
            email: Some("me@x.com".into()),
            self_: Some(true),
            response_status: Some("accepted".into()),
            ..Default::default()
        }]);
        assert_eq!(passes_filter(&e), Err(SkipReason::NotEnoughAttendees));
    }

    #[test]
    fn drops_resource_only_event() {
        let mut e = base();
        e.attendees = Some(vec![
            RawAttendee {
                email: Some("me@x.com".into()),
                self_: Some(true),
                ..Default::default()
            },
            RawAttendee {
                email: Some("room-a@x.com".into()),
                resource: Some(true),
                ..Default::default()
            },
        ]);
        assert_eq!(passes_filter(&e), Err(SkipReason::NotEnoughAttendees));
    }

    #[test]
    fn drops_subscribed_calendar_organizer() {
        let mut e = base();
        e.organizer = Some(RawOrganizer {
            email: Some("birthdays@group.calendar.google.com".into()),
            ..Default::default()
        });
        assert_eq!(
            passes_filter(&e),
            Err(SkipReason::SubscribedCalendarOrganizer)
        );
    }
}
