//! #1833 C3 (D14) — calendar candidates for a trusted contact's message.
//!
//! The daemon, not the model, looks the meeting up: [`find_candidate_events`]
//! searches **every** Google-connected owner calendar account over the direct
//! client, inside a window parsed deterministically from the inbound text in
//! the owner's time zone, ranks by title-token overlap with that text and
//! returns at most [`MAX_CANDIDATES`]. Each candidate carries an opaque
//! `cand_N` id; the `(account, calendar_id, event_id)` it stands for stays
//! daemon-side in [`CandidateSet::resolve`], so a planner can only ever point
//! at an event this lookup actually returned.
//!
//! Any account error fails the whole lookup: a silently partial list would
//! read as "the meeting isn't there" (D7 treats `Err` as
//! `evidence_incomplete`).
//!
//! [`find_candidate_events`]: crate::google_direct::GoogleCalendar::find_candidate_events

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::Serialize;

use crate::gcal::CalendarError;
use crate::google_direct::GoogleCalendar;
use crate::types::{CalendarEvent, EventTime};

/// Most candidates one lookup returns (D14).
pub const MAX_CANDIDATES: usize = 8;
/// Lookahead when the text names no day (D14: now..+7d).
pub const DEFAULT_LOOKAHEAD_DAYS: i64 = 7;
/// The calendar searched on each account. v1 searches each connected
/// account's primary calendar.
pub const CANDIDATE_CALENDAR_ID: &str = "primary";

/// Where a [`CandidateWindow`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSource {
    /// Day phrases found in the text, in the order they appeared
    /// (`today`, `tomorrow`, `next monday`, `friday`, …).
    Parsed(Vec<String>),
    /// Nothing parsed: now .. now + [`DEFAULT_LOOKAHEAD_DAYS`].
    Default,
}

/// The time ranges a lookup searches, resolved in the owner's zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateWindow {
    /// Sorted, non-overlapping, half-open `[start, end)` ranges. One per
    /// local day the text named, or a single now..+7d range.
    pub ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    pub source: WindowSource,
    /// The owner's zone the ranges were resolved in. All-day events are
    /// placed in this zone too.
    pub tz: Tz,
}

/// One event a planner may point at, by `id` only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// Opaque `cand_N` (1-based, in rank order).
    pub id: String,
    /// Owner mailbox the event was found on.
    pub account: String,
    /// The account is the event's organizer (v1 can only add guests then).
    pub is_organizer: bool,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// A date-only event; `start`/`end` are local midnights in the window's
    /// zone.
    pub all_day: bool,
    pub title: String,
    /// `hangoutLink`, else the first `video` conference entry point.
    pub join_link: Option<String>,
    /// Current attendee emails, lowercased.
    pub attendees: Vec<String>,
}

/// What a `cand_N` id stands for. Never shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRef {
    pub account: String,
    pub calendar_id: String,
    pub event_id: String,
}

/// The ranked candidates plus the daemon-side id map.
#[derive(Debug, Clone)]
pub struct CandidateSet {
    pub candidates: Vec<Candidate>,
    pub window: CandidateWindow,
    refs: BTreeMap<String, EventRef>,
}

impl CandidateSet {
    /// The event a candidate id stands for; `None` for any id this lookup
    /// did not hand out.
    pub fn resolve(&self, id: &str) -> Option<&EventRef> {
        self.refs.get(id)
    }

    pub fn get(&self, id: &str) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.id == id)
    }
}

/// Owner mailboxes on the direct Google client (`google:<email>` entities),
/// i.e. the accounts a candidate lookup must search.
pub fn connected_google_calendar_accounts(
    store: &augmentagent_store::Store,
) -> augmentagent_store::StoreResult<Vec<String>> {
    let mut out: Vec<String> = store
        .get_active_gmail_accounts()?
        .into_iter()
        .filter_map(|a| {
            augmentagent_channel_email::google::email_of(&a.entity_id)
                .map(|e| e.trim().to_ascii_lowercase())
        })
        .filter(|e| !e.is_empty())
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

impl CandidateWindow {
    /// Parse the day phrases in `text` against `now` in `tz`:
    /// `today` / `tonight` / `this morning|afternoon|evening` → today,
    /// `tomorrow` → tomorrow, a weekday name or `next <weekday>` → that
    /// weekday strictly 1..=7 days ahead (the same rule as the scheduled-send
    /// parser). Every phrase found contributes its local day; none → now..+7d.
    pub fn parse(text: &str, now: DateTime<Utc>, tz: Tz) -> Self {
        let _ = (text, now, tz);
        todo!("C3 window parser")
    }

    /// [`Self::parse`] at the current time in the owner's configured zone
    /// (`AUGMENTAGENT_TIMEZONE`, else `TZ`, else the host zone; see
    /// `augmentagent_approval_discord::timeparse::owner_zone`).
    pub fn for_owner(text: &str) -> Self {
        Self::parse(
            text,
            Utc::now(),
            augmentagent_approval_discord::timeparse::owner_zone(),
        )
    }

    /// Earliest start and latest end over all ranges.
    pub fn span(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let start = self.ranges.iter().map(|r| r.0).min()?;
        let end = self.ranges.iter().map(|r| r.1).max()?;
        Some((start, end))
    }
}

impl GoogleCalendar {
    /// D14: search every account in `accounts` (owner mailboxes on this
    /// direct client) for events overlapping `window`, rank by title-token
    /// overlap with `hint` (the inbound text), cap at [`MAX_CANDIDATES`].
    /// Any account failing — including an empty `accounts` — is `Err`, never
    /// a partial list.
    pub async fn find_candidate_events(
        &self,
        accounts: &[String],
        window: &CandidateWindow,
        hint: &str,
    ) -> Result<CandidateSet, CalendarError> {
        let _ = (accounts, window, hint);
        todo!("C3 candidate lookup")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono_tz::America::New_York;
    use mockito::{Matcher, Server};
    use std::sync::Arc;

    const PRIMARY: &str = "owner@example.test";
    const SECONDARY: &str = "owner.work@example.test";

    struct Tokens;
    #[async_trait]
    impl crate::google_direct::GoogleTokens for Tokens {
        async fn access_token(&self, email: &str) -> Result<String, String> {
            Ok(format!("at-{email}"))
        }
    }

    fn client(server: &Server) -> GoogleCalendar {
        GoogleCalendar::new(Arc::new(Tokens)).with_base_url(server.url())
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn ny_midnight(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        New_York
            .with_ymd_and_hms(y, m, d, 0, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn accounts() -> Vec<String> {
        vec![PRIMARY.to_string(), SECONDARY.to_string()]
    }

    async fn list_mock(
        server: &mut Server,
        account: &str,
        items: serde_json::Value,
    ) -> mockito::Mock {
        server
            .mock("GET", "/calendars/primary/events")
            .match_header("authorization", format!("Bearer at-{account}").as_str())
            .match_query(Matcher::Any)
            .with_body(serde_json::json!({ "items": items }).to_string())
            .create_async()
            .await
    }

    fn timed(id: &str, title: &str, start: &str, end: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "status": "confirmed", "summary": title,
            "start": {"dateTime": start}, "end": {"dateTime": end},
        })
    }

    #[tokio::test]
    async fn event_on_secondary_account_found() {
        let mut server = Server::new_async().await;
        // 2026-11-03 10:00 New York.
        let now = utc("2026-11-03T15:00:00Z");
        let window = CandidateWindow::parse(
            "not seeing an invite for this meeting today",
            now,
            New_York,
        );
        let primary = list_mock(&mut server, PRIMARY, serde_json::json!([])).await;
        let mut event = timed(
            "ev-sec-1",
            "Design review",
            "2026-11-03T13:00:00-05:00",
            "2026-11-03T13:30:00-05:00",
        );
        event["organizer"] = serde_json::json!({"email": SECONDARY, "self": true});
        event["hangoutLink"] = serde_json::json!("https://meet.example.test/abc-defg-hij");
        event["attendees"] = serde_json::json!([
            {"email": SECONDARY, "self": true, "organizer": true, "responseStatus": "accepted"},
            {"email": "Colleague@Example.test", "responseStatus": "needsAction"},
        ]);
        let secondary = list_mock(&mut server, SECONDARY, serde_json::json!([event])).await;

        let set = client(&server)
            .find_candidate_events(&accounts(), &window, "not seeing an invite for this meeting today")
            .await
            .unwrap();
        primary.assert_async().await;
        secondary.assert_async().await;
        assert_eq!(set.candidates.len(), 1);
        let c = &set.candidates[0];
        assert_eq!(c.id, "cand_1");
        assert_eq!(c.account, SECONDARY);
        assert!(c.is_organizer);
        assert_eq!(c.title, "Design review");
        assert_eq!(c.start, utc("2026-11-03T18:00:00Z"));
        assert_eq!(c.end, utc("2026-11-03T18:30:00Z"));
        assert!(!c.all_day);
        assert_eq!(c.join_link.as_deref(), Some("https://meet.example.test/abc-defg-hij"));
        assert_eq!(c.attendees, vec![SECONDARY.to_string(), "colleague@example.test".to_string()]);
        assert_eq!(
            set.resolve("cand_1"),
            Some(&EventRef {
                account: SECONDARY.into(),
                calendar_id: "primary".into(),
                event_id: "ev-sec-1".into(),
            })
        );
        assert_eq!(set.resolve("cand_2"), None);
        assert_eq!(set.resolve("ev-sec-1"), None, "raw event ids never resolve");
    }

    #[tokio::test]
    async fn today_resolves_in_owner_tz_not_utc() {
        // 21:30 on Mon 2 Nov in New York is already Tue 3 Nov in UTC.
        let now = utc("2026-11-03T02:30:00Z");
        let window = CandidateWindow::parse("is the meeting today still on?", now, New_York);
        assert_eq!(window.source, WindowSource::Parsed(vec!["today".into()]));
        assert_eq!(window.ranges, vec![(ny_midnight(2026, 11, 2), ny_midnight(2026, 11, 3))]);
        assert_eq!(window.ranges[0].0, utc("2026-11-02T05:00:00Z"));

        // The query Google sees is the local day, and an event later on the
        // UTC "today" (local tomorrow) is not a candidate.
        let mut server = Server::new_async().await;
        let list = server
            .mock("GET", "/calendars/primary/events")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("timeMin".into(), "2026-11-02T05:00:00+00:00".into()),
                Matcher::UrlEncoded("timeMax".into(), "2026-11-03T05:00:00+00:00".into()),
            ]))
            .with_body(
                serde_json::json!({"items": [
                    timed("local-today", "Standup", "2026-11-02T16:00:00-05:00", "2026-11-02T16:15:00-05:00"),
                    timed("local-tomorrow", "Standup", "2026-11-03T09:00:00-05:00", "2026-11-03T09:15:00-05:00"),
                ]})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let set = client(&server)
            .find_candidate_events(&[PRIMARY.to_string()], &window, "is the meeting today still on?")
            .await
            .unwrap();
        list.assert_async().await;
        let ids: Vec<_> = set.candidates.iter().map(|c| set.resolve(&c.id).unwrap().event_id.clone()).collect();
        assert_eq!(ids, vec!["local-today".to_string()]);

        // No day phrase at all: now .. now + 7 days.
        let fallback = CandidateWindow::parse("can you send me the link", now, New_York);
        assert_eq!(fallback.source, WindowSource::Default);
        assert_eq!(fallback.ranges, vec![(now, now + Duration::days(7))]);
        // "tomorrow" is the next local day, not the next UTC day.
        let tomorrow = CandidateWindow::parse("Tomorrow's sync?", now, New_York);
        assert_eq!(tomorrow.ranges, vec![(ny_midnight(2026, 11, 3), ny_midnight(2026, 11, 4))]);
    }

    #[tokio::test]
    async fn next_monday_window() {
        // Tue 3 Nov 2026, 10:00 New York.
        let now = utc("2026-11-03T15:00:00Z");
        let w = CandidateWindow::parse("only got one for next Monday morning", now, New_York);
        assert_eq!(w.source, WindowSource::Parsed(vec!["next monday".into()]));
        assert_eq!(w.ranges, vec![(ny_midnight(2026, 11, 9), ny_midnight(2026, 11, 10))]);

        // On a Monday, "next monday" is a week out, never today.
        let monday = utc("2026-11-09T15:00:00Z");
        let w = CandidateWindow::parse("next monday works", monday, New_York);
        assert_eq!(w.ranges, vec![(ny_midnight(2026, 11, 16), ny_midnight(2026, 11, 17))]);

        // A bare weekday name follows the same strictly-ahead rule.
        let w = CandidateWindow::parse("see you thursday", now, New_York);
        assert_eq!(w.ranges, vec![(ny_midnight(2026, 11, 5), ny_midnight(2026, 11, 6))]);

        // The incident text names both days: both are searched, in order.
        let w = CandidateWindow::parse(
            "not seeing an invite for this meeting today, only for next Monday morning",
            now,
            New_York,
        );
        assert_eq!(
            w.source,
            WindowSource::Parsed(vec!["today".into(), "next monday".into()])
        );
        assert_eq!(
            w.ranges,
            vec![
                (ny_midnight(2026, 11, 3), ny_midnight(2026, 11, 4)),
                (ny_midnight(2026, 11, 9), ny_midnight(2026, 11, 10)),
            ]
        );

        // Across the spring-forward change the local day is 23 hours long.
        let before_dst = utc("2027-03-10T15:00:00Z"); // Wed 10 Mar 2027
        let w = CandidateWindow::parse("next sunday", before_dst, New_York);
        assert_eq!(w.ranges, vec![(ny_midnight(2027, 3, 14), ny_midnight(2027, 3, 15))]);
        assert_eq!(w.ranges[0].1 - w.ranges[0].0, Duration::hours(23));
    }

    #[tokio::test]
    async fn all_day_and_timed_events() {
        let mut server = Server::new_async().await;
        let now = utc("2026-11-03T15:00:00Z");
        let window = CandidateWindow::parse("today", now, New_York);
        let mut cancelled = timed("gone", "Offsite planning", "2026-11-03T11:00:00-05:00", "2026-11-03T12:00:00-05:00");
        cancelled["status"] = serde_json::json!("cancelled");
        let mut ooo = timed("ooo", "Out of office", "2026-11-03T08:00:00-05:00", "2026-11-03T18:00:00-05:00");
        ooo["eventType"] = serde_json::json!("outOfOffice");
        let mut video = timed("timed", "Offsite planning", "2026-11-03T14:00:00-05:00", "2026-11-03T15:00:00-05:00");
        video["conferenceData"] = serde_json::json!({"entryPoints": [
            {"entryPointType": "phone", "uri": "tel:+1-555-0100"},
            {"entryPointType": "video", "uri": "https://video.example.test/j/123"},
        ]});
        let all_day = serde_json::json!({
            "id": "allday", "status": "confirmed", "summary": "Offsite",
            "start": {"date": "2026-11-03"}, "end": {"date": "2026-11-04"},
        });
        let _m = list_mock(&mut server, PRIMARY, serde_json::json!([cancelled, ooo, video, all_day])).await;

        let set = client(&server)
            .find_candidate_events(&[PRIMARY.to_string()], &window, "today")
            .await
            .unwrap();
        let by_event: BTreeMap<String, &Candidate> = set
            .candidates
            .iter()
            .map(|c| (set.resolve(&c.id).unwrap().event_id.clone(), c))
            .collect();
        assert_eq!(by_event.keys().cloned().collect::<Vec<_>>(), vec!["allday", "timed"], "cancelled and out-of-office are not meetings");

        let d = by_event["allday"];
        assert!(d.all_day);
        assert_eq!(d.start, ny_midnight(2026, 11, 3), "all-day dates are local midnights, not UTC");
        assert_eq!(d.end, ny_midnight(2026, 11, 4));
        assert_eq!(d.join_link, None);
        assert!(!d.is_organizer, "no organizer block ⇒ not organizer");

        let t = by_event["timed"];
        assert!(!t.all_day);
        assert_eq!(t.start, utc("2026-11-03T19:00:00Z"));
        assert_eq!(t.join_link.as_deref(), Some("https://video.example.test/j/123"));
    }

    #[tokio::test]
    async fn one_account_error_is_err_not_partial() {
        let mut server = Server::new_async().await;
        let now = utc("2026-11-03T15:00:00Z");
        let window = CandidateWindow::parse("today", now, New_York);
        let _ok = list_mock(
            &mut server,
            PRIMARY,
            serde_json::json!([timed("e1", "Standup", "2026-11-03T13:00:00-05:00", "2026-11-03T13:15:00-05:00")]),
        )
        .await;
        let _denied = server
            .mock("GET", "/calendars/primary/events")
            .match_header("authorization", format!("Bearer at-{SECONDARY}").as_str())
            .match_query(Matcher::Any)
            .with_status(403)
            .with_body(r#"{"error":{"message":"Request had insufficient authentication scopes."}}"#)
            .create_async()
            .await;
        let err = client(&server)
            .find_candidate_events(&accounts(), &window, "today")
            .await
            .unwrap_err();
        assert!(matches!(err, CalendarError::Forbidden { .. }), "{err}");

        // A token failure on either account is an error too.
        struct HalfRevoked;
        #[async_trait]
        impl crate::google_direct::GoogleTokens for HalfRevoked {
            async fn access_token(&self, email: &str) -> Result<String, String> {
                if email == SECONDARY {
                    Err("grant revoked".into())
                } else {
                    Ok(format!("at-{email}"))
                }
            }
        }
        let c = GoogleCalendar::new(Arc::new(HalfRevoked)).with_base_url(server.url());
        let err = c.find_candidate_events(&accounts(), &window, "today").await.unwrap_err();
        assert!(err.to_string().contains(SECONDARY), "{err}");

        // No connected account at all is not "nothing found".
        let err = client(&server).find_candidate_events(&[], &window, "today").await.unwrap_err();
        assert!(matches!(err, CalendarError::Invalid(_)), "{err}");
    }

    #[tokio::test]
    async fn candidates_capped_and_ranked() {
        let mut server = Server::new_async().await;
        let now = utc("2026-11-03T15:00:00Z");
        let window = CandidateWindow::parse("today", now, New_York);
        let text = "not seeing the meeting invite for the Quarterly Budget review today";
        // Ten events on the primary, two relevant ones on the secondary.
        let mut primary_items = Vec::new();
        for h in 0..10u32 {
            primary_items.push(timed(
                &format!("p{h}"),
                &format!("Filler meeting {h}"),
                &format!("2026-11-03T{:02}:00:00-05:00", 8 + h),
                &format!("2026-11-03T{:02}:30:00-05:00", 8 + h),
            ));
        }
        let _p = list_mock(&mut server, PRIMARY, serde_json::Value::Array(primary_items)).await;
        let _s = list_mock(
            &mut server,
            SECONDARY,
            serde_json::json!([
                timed("budget-sync", "Budget sync", "2026-11-03T09:00:00-05:00", "2026-11-03T09:30:00-05:00"),
                timed("qbr", "Quarterly budget review", "2026-11-03T16:00:00-05:00", "2026-11-03T17:00:00-05:00"),
            ]),
        )
        .await;
        let set = client(&server)
            .find_candidate_events(&accounts(), &window, text)
            .await
            .unwrap();
        assert_eq!(set.candidates.len(), MAX_CANDIDATES);
        let ids: Vec<_> = set.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, (1..=MAX_CANDIDATES).map(|n| format!("cand_{n}")).collect::<Vec<_>>());
        let events: Vec<_> = set
            .candidates
            .iter()
            .map(|c| set.resolve(&c.id).unwrap().event_id.clone())
            .collect();
        // Most title-token overlap first ("quarterly", "budget", "review"),
        // then "budget"; the rest tie at zero and keep start-time order.
        // "meeting" is a generic word, not a title match.
        assert_eq!(&events[..2], &["qbr".to_string(), "budget-sync".to_string()]);
        assert_eq!(&events[2..], &["p0", "p1", "p2", "p3", "p4", "p5"].map(String::from));
        assert!(set.get("cand_9").is_none() && set.resolve("cand_9").is_none());
    }
}
