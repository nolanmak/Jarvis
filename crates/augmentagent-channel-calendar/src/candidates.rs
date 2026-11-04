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
    /// A set from already-ranked `(candidate, event)` pairs, keeping each
    /// candidate's id (fakes and replays; a live lookup builds its own).
    pub fn from_parts(window: CandidateWindow, entries: Vec<(Candidate, EventRef)>) -> Self {
        let mut candidates = Vec::with_capacity(entries.len());
        let mut refs = BTreeMap::new();
        for (c, r) in entries {
            refs.insert(c.id.clone(), r);
            candidates.push(c);
        }
        Self { candidates, window, refs }
    }

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
        let today = now.with_timezone(&tz).date_naive();
        let words = words(text);
        let mut phrases: Vec<String> = Vec::new();
        let mut days: Vec<NaiveDate> = Vec::new();
        let mut push = |phrase: String, day: NaiveDate| {
            if !phrases.contains(&phrase) {
                phrases.push(phrase);
            }
            if !days.contains(&day) {
                days.push(day);
            }
        };
        let mut i = 0;
        while i < words.len() {
            let w = words[i].as_str();
            let next = words.get(i + 1).map(String::as_str);
            match w {
                "today" | "tonight" => push(w.to_string(), today),
                "this" if matches!(next, Some("morning" | "afternoon" | "evening")) => {
                    push(format!("this {}", next.unwrap_or_default()), today);
                    i += 1;
                }
                "tomorrow" | "tmrw" => push("tomorrow".into(), today + Duration::days(1)),
                "next" => {
                    if let Some(wd) = next.and_then(weekday) {
                        push(
                            format!("next {}", weekday_name(wd)),
                            strictly_ahead(today, wd),
                        );
                        i += 1;
                    }
                }
                _ => {
                    if let Some(wd) = weekday(w) {
                        push(weekday_name(wd).to_string(), strictly_ahead(today, wd));
                    }
                }
            }
            i += 1;
        }
        if days.is_empty() {
            return Self {
                ranges: vec![(now, now + Duration::days(DEFAULT_LOOKAHEAD_DAYS))],
                source: WindowSource::Default,
                tz,
            };
        }
        days.sort();
        let ranges = days
            .into_iter()
            .map(|d| {
                (
                    local_midnight(tz, d),
                    local_midnight(tz, d + Duration::days(1)),
                )
            })
            .collect();
        Self {
            ranges,
            source: WindowSource::Parsed(phrases),
            tz,
        }
    }

    /// Whether `[start, end)` overlaps any range. A zero-length event counts
    /// when its instant falls inside a range.
    fn overlaps(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        self.ranges
            .iter()
            .any(|(lo, hi)| start < *hi && (end > *lo || (end == start && start >= *lo)))
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
        if accounts.is_empty() {
            return Err(CalendarError::Invalid(
                "no Google-connected calendar account to search".into(),
            ));
        }
        let (min, max) = window
            .span()
            .ok_or_else(|| CalendarError::Invalid("empty candidate window".into()))?;
        let hint_tokens = title_tokens(hint);
        let mut found: Vec<(usize, Candidate, EventRef)> = Vec::new();
        for account in accounts {
            let events = self
                .list_events(account, CANDIDATE_CALENDAR_ID, min, max)
                .await?;
            for ev in events {
                let Some(c) = candidate_of(&ev, account, window) else {
                    continue;
                };
                let score = title_tokens(&c.title)
                    .iter()
                    .filter(|t| hint_tokens.contains(*t))
                    .count();
                let r = EventRef {
                    account: account.clone(),
                    calendar_id: CANDIDATE_CALENDAR_ID.to_string(),
                    event_id: ev.id.clone(),
                };
                found.push((score, c, r));
            }
        }
        // Most overlap first, then soonest, then a stable tiebreak.
        found.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.start.cmp(&b.1.start))
                .then(a.2.account.cmp(&b.2.account))
                .then(a.2.event_id.cmp(&b.2.event_id))
        });
        found.truncate(MAX_CANDIDATES);
        let mut candidates = Vec::with_capacity(found.len());
        let mut refs = BTreeMap::new();
        for (n, (_, mut c, r)) in found.into_iter().enumerate() {
            c.id = format!("cand_{}", n + 1);
            refs.insert(c.id.clone(), r);
            candidates.push(c);
        }
        Ok(CandidateSet {
            candidates,
            window: window.clone(),
            refs,
        })
    }
}

/// Event types that are not meetings anyone can be invited to.
const NOT_MEETINGS: &[&str] = &["outOfOffice", "focusTime", "workingLocation", "birthday"];

/// Words too generic to say which meeting a message means.
const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "this",
    "that",
    "these",
    "those",
    "from",
    "you",
    "your",
    "our",
    "are",
    "was",
    "not",
    "but",
    "can",
    "could",
    "would",
    "will",
    "just",
    "only",
    "about",
    "have",
    "has",
    "had",
    "get",
    "got",
    "any",
    "all",
    "its",
    "it's",
    "into",
    "out",
    "meeting",
    "meetings",
    "invite",
    "invites",
    "invitation",
    "calendar",
    "event",
    "call",
    "seeing",
    "see",
    "send",
    "link",
    "today",
    "tonight",
    "tomorrow",
    "next",
    "morning",
    "afternoon",
    "evening",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "please",
    "thanks",
    "there",
    "what",
    "when",
    "where",
    "which",
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn title_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = words(text)
        .into_iter()
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn weekday(w: &str) -> Option<Weekday> {
    Some(match w {
        "monday" => Weekday::Mon,
        "tuesday" | "tues" => Weekday::Tue,
        "wednesday" | "weds" => Weekday::Wed,
        "thursday" | "thurs" => Weekday::Thu,
        "friday" => Weekday::Fri,
        "saturday" => Weekday::Sat,
        "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn weekday_name(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "monday",
        Weekday::Tue => "tuesday",
        Weekday::Wed => "wednesday",
        Weekday::Thu => "thursday",
        Weekday::Fri => "friday",
        Weekday::Sat => "saturday",
        Weekday::Sun => "sunday",
    }
}

/// The next `wd` strictly 1..=7 days after `today` (scheduled-send rule).
fn strictly_ahead(today: NaiveDate, wd: Weekday) -> NaiveDate {
    let diff =
        (7 + wd.num_days_from_monday() as i64 - today.weekday().num_days_from_monday() as i64) % 7;
    today + Duration::days(if diff == 0 { 7 } else { diff })
}

/// Start of local day `d` in `tz`. Midnight can be skipped by a DST change
/// in some zones; then the first instant of the day is an hour later.
fn local_midnight(tz: Tz, d: NaiveDate) -> DateTime<Utc> {
    let midnight = d.and_hms_opt(0, 0, 0).expect("midnight");
    tz.from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(midnight + Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&midnight))
}

/// `(instant, all_day)` for an event boundary; date-only values are local
/// midnights in `tz`.
fn boundary(t: &EventTime, tz: Tz) -> Option<(DateTime<Utc>, bool)> {
    if let Some(dt) = &t.date_time {
        return DateTime::parse_from_rfc3339(dt)
            .ok()
            .map(|d| (d.with_timezone(&Utc), false));
    }
    let date = NaiveDate::parse_from_str(t.date.as_deref()?, "%Y-%m-%d").ok()?;
    Some((local_midnight(tz, date), true))
}

fn candidate_of(ev: &CalendarEvent, account: &str, window: &CandidateWindow) -> Option<Candidate> {
    if ev.id.trim().is_empty()
        || ev.status.as_deref() == Some("cancelled")
        || ev
            .event_type
            .as_deref()
            .is_some_and(|t| NOT_MEETINGS.contains(&t))
    {
        return None;
    }
    let (start, all_day) = boundary(ev.start.as_ref()?, window.tz)?;
    let end = ev
        .end
        .as_ref()
        .and_then(|e| boundary(e, window.tz))
        .map(|(e, _)| e)
        .filter(|e| *e >= start)
        .unwrap_or(if all_day {
            start + Duration::days(1)
        } else {
            start
        });
    if !window.overlaps(start, end) {
        return None;
    }
    let is_organizer = ev.organizer.as_ref().is_some_and(|o| {
        o.self_ == Some(true)
            || o.email
                .as_deref()
                .is_some_and(|e| e.trim().eq_ignore_ascii_case(account.trim()))
    });
    let join_link = ev
        .hangout_link
        .clone()
        .filter(|l| !l.trim().is_empty())
        .or_else(|| {
            ev.conference_data
                .as_ref()?
                .entry_points
                .as_ref()?
                .iter()
                .find(|p| p.entry_point_type.as_deref() == Some("video"))
                .and_then(|p| p.uri.clone())
        });
    let attendees = ev
        .attendees
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a.email.as_deref())
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    let title = ev
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("(no title)")
        .to_string();
    Some(Candidate {
        id: String::new(),
        account: account.to_string(),
        is_organizer,
        start,
        end,
        all_day,
        title,
        join_link,
        attendees,
    })
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
        let window =
            CandidateWindow::parse("not seeing an invite for this meeting today", now, New_York);
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
            .find_candidate_events(
                &accounts(),
                &window,
                "not seeing an invite for this meeting today",
            )
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
        assert_eq!(
            c.join_link.as_deref(),
            Some("https://meet.example.test/abc-defg-hij")
        );
        assert_eq!(
            c.attendees,
            vec![SECONDARY.to_string(), "colleague@example.test".to_string()]
        );
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
        assert_eq!(
            window.ranges,
            vec![(ny_midnight(2026, 11, 2), ny_midnight(2026, 11, 3))]
        );
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
            .find_candidate_events(
                &[PRIMARY.to_string()],
                &window,
                "is the meeting today still on?",
            )
            .await
            .unwrap();
        list.assert_async().await;
        let ids: Vec<_> = set
            .candidates
            .iter()
            .map(|c| set.resolve(&c.id).unwrap().event_id.clone())
            .collect();
        assert_eq!(ids, vec!["local-today".to_string()]);

        // No day phrase at all: now .. now + 7 days.
        let fallback = CandidateWindow::parse("can you send me the link", now, New_York);
        assert_eq!(fallback.source, WindowSource::Default);
        assert_eq!(fallback.ranges, vec![(now, now + Duration::days(7))]);
        // "tomorrow" is the next local day, not the next UTC day.
        let tomorrow = CandidateWindow::parse("Tomorrow's sync?", now, New_York);
        assert_eq!(
            tomorrow.ranges,
            vec![(ny_midnight(2026, 11, 3), ny_midnight(2026, 11, 4))]
        );
    }

    #[tokio::test]
    async fn next_monday_window() {
        // Tue 3 Nov 2026, 10:00 New York.
        let now = utc("2026-11-03T15:00:00Z");
        let w = CandidateWindow::parse("only got one for next Monday morning", now, New_York);
        assert_eq!(w.source, WindowSource::Parsed(vec!["next monday".into()]));
        assert_eq!(
            w.ranges,
            vec![(ny_midnight(2026, 11, 9), ny_midnight(2026, 11, 10))]
        );

        // On a Monday, "next monday" is a week out, never today.
        let monday = utc("2026-11-09T15:00:00Z");
        let w = CandidateWindow::parse("next monday works", monday, New_York);
        assert_eq!(
            w.ranges,
            vec![(ny_midnight(2026, 11, 16), ny_midnight(2026, 11, 17))]
        );

        // A bare weekday name follows the same strictly-ahead rule.
        let w = CandidateWindow::parse("see you thursday", now, New_York);
        assert_eq!(
            w.ranges,
            vec![(ny_midnight(2026, 11, 5), ny_midnight(2026, 11, 6))]
        );

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
        assert_eq!(
            w.ranges,
            vec![(ny_midnight(2027, 3, 14), ny_midnight(2027, 3, 15))]
        );
        assert_eq!(w.ranges[0].1 - w.ranges[0].0, Duration::hours(23));
    }

    #[tokio::test]
    async fn all_day_and_timed_events() {
        let mut server = Server::new_async().await;
        let now = utc("2026-11-03T15:00:00Z");
        let window = CandidateWindow::parse("today", now, New_York);
        let mut cancelled = timed(
            "gone",
            "Offsite planning",
            "2026-11-03T11:00:00-05:00",
            "2026-11-03T12:00:00-05:00",
        );
        cancelled["status"] = serde_json::json!("cancelled");
        let mut ooo = timed(
            "ooo",
            "Out of office",
            "2026-11-03T08:00:00-05:00",
            "2026-11-03T18:00:00-05:00",
        );
        ooo["eventType"] = serde_json::json!("outOfOffice");
        let mut video = timed(
            "timed",
            "Offsite planning",
            "2026-11-03T14:00:00-05:00",
            "2026-11-03T15:00:00-05:00",
        );
        video["conferenceData"] = serde_json::json!({"entryPoints": [
            {"entryPointType": "phone", "uri": "tel:+1-555-0100"},
            {"entryPointType": "video", "uri": "https://video.example.test/j/123"},
        ]});
        let all_day = serde_json::json!({
            "id": "allday", "status": "confirmed", "summary": "Offsite",
            "start": {"date": "2026-11-03"}, "end": {"date": "2026-11-04"},
        });
        let _m = list_mock(
            &mut server,
            PRIMARY,
            serde_json::json!([cancelled, ooo, video, all_day]),
        )
        .await;

        let set = client(&server)
            .find_candidate_events(&[PRIMARY.to_string()], &window, "today")
            .await
            .unwrap();
        let by_event: BTreeMap<String, &Candidate> = set
            .candidates
            .iter()
            .map(|c| (set.resolve(&c.id).unwrap().event_id.clone(), c))
            .collect();
        assert_eq!(
            by_event.keys().cloned().collect::<Vec<_>>(),
            vec!["allday", "timed"],
            "cancelled and out-of-office are not meetings"
        );

        let d = by_event["allday"];
        assert!(d.all_day);
        assert_eq!(
            d.start,
            ny_midnight(2026, 11, 3),
            "all-day dates are local midnights, not UTC"
        );
        assert_eq!(d.end, ny_midnight(2026, 11, 4));
        assert_eq!(d.join_link, None);
        assert!(!d.is_organizer, "no organizer block ⇒ not organizer");

        let t = by_event["timed"];
        assert!(!t.all_day);
        assert_eq!(t.start, utc("2026-11-03T19:00:00Z"));
        assert_eq!(
            t.join_link.as_deref(),
            Some("https://video.example.test/j/123")
        );
    }

    #[tokio::test]
    async fn one_account_error_is_err_not_partial() {
        let mut server = Server::new_async().await;
        let now = utc("2026-11-03T15:00:00Z");
        let window = CandidateWindow::parse("today", now, New_York);
        let _ok = list_mock(
            &mut server,
            PRIMARY,
            serde_json::json!([timed(
                "e1",
                "Standup",
                "2026-11-03T13:00:00-05:00",
                "2026-11-03T13:15:00-05:00"
            )]),
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
        let err = c
            .find_candidate_events(&accounts(), &window, "today")
            .await
            .unwrap_err();
        assert!(err.to_string().contains(SECONDARY), "{err}");

        // No connected account at all is not "nothing found".
        let err = client(&server)
            .find_candidate_events(&[], &window, "today")
            .await
            .unwrap_err();
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
        let _p = list_mock(
            &mut server,
            PRIMARY,
            serde_json::Value::Array(primary_items),
        )
        .await;
        let _s = list_mock(
            &mut server,
            SECONDARY,
            serde_json::json!([
                timed(
                    "budget-sync",
                    "Budget sync",
                    "2026-11-03T09:00:00-05:00",
                    "2026-11-03T09:30:00-05:00"
                ),
                timed(
                    "qbr",
                    "Quarterly budget review",
                    "2026-11-03T16:00:00-05:00",
                    "2026-11-03T17:00:00-05:00"
                ),
            ]),
        )
        .await;
        let set = client(&server)
            .find_candidate_events(&accounts(), &window, text)
            .await
            .unwrap();
        assert_eq!(set.candidates.len(), MAX_CANDIDATES);
        let ids: Vec<_> = set.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            ids,
            (1..=MAX_CANDIDATES)
                .map(|n| format!("cand_{n}"))
                .collect::<Vec<_>>()
        );
        let events: Vec<_> = set
            .candidates
            .iter()
            .map(|c| set.resolve(&c.id).unwrap().event_id.clone())
            .collect();
        // Most title-token overlap first ("quarterly", "budget", "review"),
        // then "budget"; the rest tie at zero and keep start-time order.
        // "meeting" is a generic word, not a title match.
        assert_eq!(
            &events[..2],
            &["qbr".to_string(), "budget-sync".to_string()]
        );
        assert_eq!(
            &events[2..],
            &["p0", "p1", "p2", "p3", "p4", "p5"].map(String::from)
        );
        assert!(set.get("cand_9").is_none() && set.resolve("cand_9").is_none());
    }

    /// Opt-in live, READ-ONLY check against the owner's real calendars:
    /// runs the lookup over every listed Google-connected account and prints
    /// counts only (no titles, no addresses). Never writes. Needs
    /// GOOGLE_OAUTH_CLIENT_ID/SECRET in the env and the mailboxes' stored
    /// grants; accounts are comma-separated in
    /// JARVIS_GCAL_CANDIDATES_QA_ACCOUNTS.
    #[tokio::test]
    #[ignore = "live read-only Google Calendar lookup; set JARVIS_GCAL_CANDIDATES_QA_ACCOUNTS"]
    async fn live_find_candidate_events_read_only() {
        let accounts: Vec<String> = std::env::var("JARVIS_GCAL_CANDIDATES_QA_ACCOUNTS")
            .expect("QA accounts")
            .split(',')
            .map(|a| a.trim().to_ascii_lowercase())
            .filter(|a| !a.is_empty())
            .collect();
        let google =
            augmentagent_channel_email::google::GoogleGmail::from_env().expect("OAuth client env");
        let cal = GoogleCalendar::new(Arc::new(google));
        for text in ["is the meeting today still on?", "can you send me the link"] {
            let window = CandidateWindow::for_owner(text);
            let set = cal
                .find_candidate_events(&accounts, &window, text)
                .await
                .expect("lookup");
            let per_account = accounts
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let n = set.candidates.iter().filter(|c| &c.account == a).count();
                    format!("account#{i}={n}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            println!(
                "{:?} tz={} ranges={} candidates={} organizer={} with_link={} all_day={} [{per_account}]",
                window.source,
                window.tz,
                window.ranges.len(),
                set.candidates.len(),
                set.candidates.iter().filter(|c| c.is_organizer).count(),
                set.candidates.iter().filter(|c| c.join_link.is_some()).count(),
                set.candidates.iter().filter(|c| c.all_day).count(),
            );
            assert!(set.candidates.len() <= MAX_CANDIDATES);
            for c in &set.candidates {
                assert!(set.resolve(&c.id).is_some());
            }
        }
    }
}
