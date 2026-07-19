//! Proactive operator alerts over the hot-window poll (#396 / #397).
//!
//! Three alert kinds, all deduped through the `calendar_alerts` store table
//! keyed by alert kind + a fingerprint of the times the alert described:
//!
//! - `upcoming:<event_id>` — event starts within the configured lead window.
//!   Fingerprint = the start timestamp, so a reschedule re-arms the alert
//!   while a re-poll of the unchanged event stays silent.
//! - `conflict:<idA>:<idB>` — two busy events overlap. Fingerprint = both
//!   events' start/end, so moving either event re-arms the pair.
//! - `agenda:<local-date>` — once-per-day morning summary; keyed by local
//!   calendar date, fires on the first poll at/after the configured hour.
//!
//! Detection is pure functions over [`AlertCandidate`] so the logic is
//! unit-testable without a store or a transport. Delivery goes through the
//! [`AlertSink`] trait; the CLI wires a Discord implementation, tests wire a
//! capture mock. Message rendering uses only [`MeetingPayload`] fields — the
//! privacy allowlist holds for alerts exactly as it does for wiki ingest.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};

use crate::types::{CalendarEvent, MeetingPayload};

/// Transport for operator-facing alert messages. The channel stays
/// transport-agnostic; `run_calendar_poll_once` wires a Discord sink.
#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn send(&self, text: &str) -> anyhow::Result<()>;
}

/// A calendar event projected for the alert pass. Carries the raw-event
/// busy/response flags that the privacy-allowlisted [`MeetingPayload`]
/// intentionally drops but conflict/upcoming logic needs.
#[derive(Debug, Clone)]
pub struct AlertCandidate {
    pub payload: MeetingPayload,
    /// Date-only event (no `dateTime` on `start`).
    pub all_day: bool,
    /// `transparency == "transparent"` — marked free, doesn't block time.
    pub transparent: bool,
    /// The account owner declined this event.
    pub declined_by_self: bool,
}

impl AlertCandidate {
    /// Build from a raw event. Returns `None` for cancelled events, special
    /// event types (OOO / focus time / working location / birthday), and
    /// events without a parseable start — none of those are alert-worthy.
    pub fn from_event(
        ev: &CalendarEvent,
        entity_id: &str,
        calendar_id: &str,
    ) -> Option<Self> {
        if ev
            .status
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("cancelled"))
            .unwrap_or(false)
        {
            return None;
        }
        if let Some(t) = ev.event_type.as_deref() {
            if matches!(
                t.to_ascii_lowercase().as_str(),
                "outofoffice" | "focustime" | "workinglocation" | "birthday"
            ) {
                return None;
            }
        }
        let payload = MeetingPayload::from_event(ev, entity_id, calendar_id)?;
        let all_day = ev
            .start
            .as_ref()
            .map(|s| s.date_time.is_none())
            .unwrap_or(true);
        let transparent = ev
            .transparency
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("transparent"))
            .unwrap_or(false);
        let declined_by_self = ev
            .attendees
            .as_ref()
            .and_then(|list| list.iter().find(|a| a.self_.unwrap_or(false)))
            .and_then(|me| me.response_status.as_deref())
            .map(|s| s.eq_ignore_ascii_case("declined"))
            .unwrap_or(false);
        Some(Self {
            payload,
            all_day,
            transparent,
            declined_by_self,
        })
    }

    /// Timed, opaque, not declined — participates in upcoming + conflict
    /// detection.
    fn blocks_time(&self) -> bool {
        !self.all_day && !self.transparent && !self.declined_by_self
    }
}

pub fn upcoming_key(event_id: &str) -> String {
    format!("upcoming:{event_id}")
}

/// Fingerprint = the start we alerted about. A reschedule changes it and
/// re-arms the alert; cancellation simply never matches again.
pub fn upcoming_fingerprint(p: &MeetingPayload) -> String {
    p.start.to_rfc3339()
}

/// Should this candidate produce an imminent-start alert right now?
pub fn is_upcoming(c: &AlertCandidate, now: DateTime<Utc>, lead: Duration) -> bool {
    c.blocks_time() && c.payload.start > now && c.payload.start - now <= lead
}

/// Overlapping pairs among the candidates, as indices into the input slice.
/// Half-open intervals: `a` overlaps `b` iff `a.start < b.end && b.start <
/// a.end`, so back-to-back meetings do NOT conflict. Only `blocks_time`
/// candidates participate.
pub fn conflict_pairs(cands: &[AlertCandidate]) -> Vec<(usize, usize)> {
    let mut idx: Vec<usize> = (0..cands.len())
        .filter(|&i| cands[i].blocks_time())
        .collect();
    idx.sort_by_key(|&i| cands[i].payload.start);
    let mut out = Vec::new();
    for x in 0..idx.len() {
        for y in (x + 1)..idx.len() {
            let a = &cands[idx[x]].payload;
            let b = &cands[idx[y]].payload;
            if b.start >= a.end {
                // Sorted by start: nothing later can overlap `a` either.
                break;
            }
            out.push((idx[x], idx[y]));
        }
    }
    out
}

/// Stable pair key — event ids sorted so (a,b) and (b,a) collapse.
pub fn conflict_key(a: &MeetingPayload, b: &MeetingPayload) -> String {
    let (x, y) = ordered(a, b);
    format!("conflict:{}:{}", x.event_id, y.event_id)
}

/// Fingerprint over both events' times: moving either end re-arms the pair.
pub fn conflict_fingerprint(a: &MeetingPayload, b: &MeetingPayload) -> String {
    let (x, y) = ordered(a, b);
    format!(
        "{}|{}|{}|{}",
        x.start.to_rfc3339(),
        x.end.to_rfc3339(),
        y.start.to_rfc3339(),
        y.end.to_rfc3339()
    )
}

fn ordered<'a>(
    a: &'a MeetingPayload,
    b: &'a MeetingPayload,
) -> (&'a MeetingPayload, &'a MeetingPayload) {
    if a.event_id <= b.event_id {
        (a, b)
    } else {
        (b, a)
    }
}

pub fn agenda_key(local_date: NaiveDate) -> String {
    format!("agenda:{local_date}")
}

/// Events still ahead today (local time), sorted by start. All-day and
/// declined events are excluded; transparent ones stay (an agenda is a
/// summary, not a busy check).
pub fn agenda_items<'a>(
    cands: &'a [AlertCandidate],
    now: DateTime<Utc>,
) -> Vec<&'a AlertCandidate> {
    let today = now.with_timezone(&Local).date_naive();
    let mut items: Vec<&AlertCandidate> = cands
        .iter()
        .filter(|c| !c.all_day && !c.declined_by_self)
        .filter(|c| c.payload.start >= now)
        .filter(|c| c.payload.start.with_timezone(&Local).date_naive() == today)
        .collect();
    items.sort_by_key(|c| c.payload.start);
    items
}

fn fmt_local_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M").to_string()
}

/// "📅 **Q3 planning** starts in 23 min (14:00–14:45) — with Sarah · google_meet"
pub fn render_upcoming(p: &MeetingPayload, now: DateTime<Utc>) -> String {
    let mins = (p.start - now).num_minutes().max(0);
    let mut out = format!(
        "📅 **{}** starts in {} min ({}–{})",
        p.summary,
        mins,
        fmt_local_time(p.start),
        fmt_local_time(p.end)
    );
    let others: Vec<String> = p
        .attendees
        .iter()
        .filter(|a| !a.is_self && !a.is_resource)
        .map(|a| a.display_name.clone().unwrap_or_else(|| a.email.clone()))
        .collect();
    if !others.is_empty() {
        out.push_str(&format!(" — with {}", others.join(", ")));
    }
    if let Some(kind) = &p.conference_kind {
        out.push_str(&format!(" · {kind}"));
    }
    out
}

pub fn render_conflict(a: &MeetingPayload, b: &MeetingPayload) -> String {
    format!(
        "⚠️ **Schedule conflict**: \"{}\" ({}–{}) overlaps \"{}\" ({}–{})",
        a.summary,
        fmt_local_time(a.start),
        fmt_local_time(a.end),
        b.summary,
        fmt_local_time(b.start),
        fmt_local_time(b.end)
    )
}

pub fn render_agenda(items: &[&AlertCandidate], now: DateTime<Utc>) -> String {
    let date = now.with_timezone(&Local).format("%A %Y-%m-%d");
    let mut out = format!(
        "🗓️ **Agenda for {date}** — {} meeting{}:\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    );
    for c in items {
        let p = &c.payload;
        out.push_str(&format!(
            "• {}–{}  {}\n",
            fmt_local_time(p.start),
            fmt_local_time(p.end),
            p.summary
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EventTime, RawAttendee};

    fn timed_event(id: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            status: Some("confirmed".into()),
            summary: Some(format!("Meeting {id}")),
            start: Some(EventTime {
                date_time: Some(start.to_rfc3339()),
                ..Default::default()
            }),
            end: Some(EventTime {
                date_time: Some(end.to_rfc3339()),
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
                    email: Some("other@y.com".into()),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }
    }

    fn cand(ev: &CalendarEvent) -> AlertCandidate {
        AlertCandidate::from_event(ev, "ent", "primary").expect("candidate")
    }

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 7, 8, h, m, 0).unwrap()
    }

    #[test]
    fn candidate_drops_cancelled_and_special_types() {
        let mut ev = timed_event("e1", t(10, 0), t(11, 0));
        ev.status = Some("cancelled".into());
        assert!(AlertCandidate::from_event(&ev, "ent", "primary").is_none());

        let mut ev = timed_event("e2", t(10, 0), t(11, 0));
        ev.event_type = Some("focusTime".into());
        assert!(AlertCandidate::from_event(&ev, "ent", "primary").is_none());
    }

    #[test]
    fn candidate_flags_all_day_transparent_declined() {
        let mut ev = timed_event("e1", t(10, 0), t(11, 0));
        ev.start = Some(EventTime {
            date: Some("2026-07-08".into()),
            ..Default::default()
        });
        assert!(cand(&ev).all_day);

        let mut ev = timed_event("e2", t(10, 0), t(11, 0));
        ev.transparency = Some("transparent".into());
        assert!(cand(&ev).transparent);

        let mut ev = timed_event("e3", t(10, 0), t(11, 0));
        ev.attendees.as_mut().unwrap()[0].response_status = Some("declined".into());
        assert!(cand(&ev).declined_by_self);
    }

    #[test]
    fn upcoming_window_edges() {
        let now = t(9, 0);
        let lead = Duration::minutes(60);
        // 30 min out: alert.
        assert!(is_upcoming(&cand(&timed_event("a", t(9, 30), t(10, 0))), now, lead));
        // Exactly at the lead boundary: alert (<=).
        assert!(is_upcoming(&cand(&timed_event("b", t(10, 0), t(11, 0))), now, lead));
        // Beyond lead: no alert.
        assert!(!is_upcoming(&cand(&timed_event("c", t(10, 1), t(11, 0))), now, lead));
        // Already started: no alert.
        assert!(!is_upcoming(&cand(&timed_event("d", t(9, 0), t(10, 0))), now, lead));
        // Declined: no alert.
        let mut ev = timed_event("e", t(9, 30), t(10, 0));
        ev.attendees.as_mut().unwrap()[0].response_status = Some("declined".into());
        assert!(!is_upcoming(&cand(&ev), now, lead));
    }

    #[test]
    fn conflict_pairs_finds_overlap_not_adjacency() {
        let cands = vec![
            cand(&timed_event("a", t(10, 0), t(11, 0))),
            cand(&timed_event("b", t(10, 30), t(11, 30))), // overlaps a
            cand(&timed_event("c", t(11, 0), t(12, 0))),   // back-to-back with a: no
        ];
        let pairs = conflict_pairs(&cands);
        assert_eq!(pairs.len(), 2); // (a,b) and (b,c)
        let keys: Vec<String> = pairs
            .iter()
            .map(|&(x, y)| conflict_key(&cands[x].payload, &cands[y].payload))
            .collect();
        assert!(keys.contains(&"conflict:a:b".to_string()));
        assert!(keys.contains(&"conflict:b:c".to_string()));
        assert!(!keys.contains(&"conflict:a:c".to_string()));
    }

    #[test]
    fn conflict_pairs_ignore_free_declined_allday() {
        let mut free = timed_event("free", t(10, 0), t(11, 0));
        free.transparency = Some("transparent".into());
        let mut declined = timed_event("dec", t(10, 0), t(11, 0));
        declined.attendees.as_mut().unwrap()[0].response_status = Some("declined".into());
        let mut all_day = timed_event("day", t(10, 0), t(11, 0));
        all_day.start = Some(EventTime {
            date: Some("2026-07-08".into()),
            ..Default::default()
        });
        let cands = vec![
            cand(&timed_event("a", t(10, 0), t(11, 0))),
            cand(&free),
            cand(&declined),
            cand(&all_day),
        ];
        assert!(conflict_pairs(&cands).is_empty());
    }

    #[test]
    fn conflict_key_and_fingerprint_are_order_stable() {
        let a = cand(&timed_event("a", t(10, 0), t(11, 0))).payload;
        let b = cand(&timed_event("b", t(10, 30), t(11, 30))).payload;
        assert_eq!(conflict_key(&a, &b), conflict_key(&b, &a));
        assert_eq!(conflict_fingerprint(&a, &b), conflict_fingerprint(&b, &a));
        // Moving one event changes the fingerprint (re-arm).
        let b2 = cand(&timed_event("b", t(10, 45), t(11, 30))).payload;
        assert_ne!(conflict_fingerprint(&a, &b), conflict_fingerprint(&a, &b2));
    }

    #[test]
    fn reschedule_changes_upcoming_fingerprint() {
        let p1 = cand(&timed_event("a", t(10, 0), t(11, 0))).payload;
        let p2 = cand(&timed_event("a", t(12, 0), t(13, 0))).payload;
        assert_eq!(upcoming_key("a"), "upcoming:a");
        assert_ne!(upcoming_fingerprint(&p1), upcoming_fingerprint(&p2));
    }

    #[test]
    fn render_upcoming_mentions_title_and_attendee() {
        let p = cand(&timed_event("a", t(10, 0), t(11, 0))).payload;
        let text = render_upcoming(&p, t(9, 40));
        assert!(text.contains("Meeting a"), "{text}");
        assert!(text.contains("starts in 20 min"), "{text}");
        assert!(text.contains("other@y.com"), "{text}");
    }
}
