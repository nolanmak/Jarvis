//! #501 — deterministic send-time resolution for the schedule surfaces.
//!
//! Three entry points share this module: the card's Schedule `StringSelect`
//! (symbolic tokens, resolved at CLICK time — cards sit for hours/days, so
//! render-time resolution would drift), the custom-time modal (free text),
//! and — via the `augmentagent-channel-core::timeparse` re-export — the
//! query-mode `--send-at` flag (#502). **No LLM**: an LLM computing a future
//! UTC offset across a DST transition silently shifts the send by an hour;
//! this parser does date math on `NaiveDate` and resolves wall time through
//! the timezone at the end.
//!
//! This file lives here (not in channel-core, where the epic drafted it)
//! because the workspace dependency edge runs channel-core → approval-discord
//! (`engagement.rs::ApprovalBroker`): the Discord select/modal arms in
//! `event_handler.rs` need these functions, so the shared implementation must
//! sit downstream. channel-core re-exports the whole module for #502.
//!
//! DST policy (locked by #499): wall times that occur twice (fall-back
//! overlap) resolve to the EARLIEST reading; wall times that never occur
//! (spring-forward gap) shift forward one hour. "tomorrow 9am" across a
//! transition is a calendar-day computation, never `now + 24h`.
//!
//! The core functions are generic over `chrono::TimeZone` so tests can pin
//! `chrono_tz::America::New_York` fixtures — `chrono::Local` reads the `TZ`
//! env var once at process start, which makes env-based test pinning
//! unreliable. Production callers use the thin `Local` wrappers.

use chrono::{
    DateTime, Datelike, Days, Duration, LocalResult, Local, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Weekday,
};

/// Minimum lead time for a scheduled send: reject anything ≤ now + 2 minutes.
/// Part of the central guard ([`validate_send_at`]) every entry point shares.
pub const MIN_LEAD_MS: i64 = 2 * 60 * 1000;

/// Maximum schedule horizon: reject anything > now + 60 days. A send armed
/// months out is far more likely a typo'd year than intent.
pub const MAX_HORIZON_MS: i64 = 60 * 24 * 60 * 60 * 1000;

/// Grace for parse-to-validate latency (#501 review): "in 2m" resolves to
/// exactly now+[`MIN_LEAD_MS`] at parse time and reaches the guard (a store
/// round-trip and a fresh clock read later) strictly after — without this
/// allowance the advertised 2-minute minimum could never actually validate.
pub const SKEW_MS: i64 = 15_000;

/// The central time guard (#501): one validation shared by the select tokens,
/// the custom modal, and (later) `--send-at`, enforced at the CAS layer in
/// `ApprovalActionHandler::schedule` — not per-parser, so no entry point can
/// forget it.
pub fn validate_send_at(at_ms: i64, now_ms: i64) -> Result<(), String> {
    if at_ms < now_ms + MIN_LEAD_MS - SKEW_MS {
        return Err(
            "that time is too soon — schedule at least 2 minutes out".to_string(),
        );
    }
    if at_ms > now_ms + MAX_HORIZON_MS {
        return Err(
            "that time is too far out — schedules cap at 60 days ahead".to_string(),
        );
    }
    Ok(())
}

/// Resolve a symbolic Schedule-select token to epoch-ms. Thin `Local` wrapper
/// over [`resolve_token_in`] for production callers (the Discord select arm).
pub fn resolve_token(token: &str, now: DateTime<Local>) -> Result<i64, String> {
    resolve_token_in(token, now)
}

/// Parse a free-text send time (custom modal, `--send-at`) to epoch-ms. Thin
/// `Local` wrapper over [`parse_send_at_in`] for production callers.
pub fn parse_send_at(input: &str, now: DateTime<Local>) -> Result<i64, String> {
    parse_send_at_in(input, now)
}

/// Generic-timezone core of [`resolve_token`]. Click-time resolution rules
/// (spec'd in #501, tested against `America/New_York` fixtures):
///
/// - `in1h` / `in3h` — pure instant math (`now + offset`), DST-immune.
/// - `tonight-1900` — today 19:00 local; if that is ≤ now + 2min the token is
///   an ERROR steering the owner to a tomorrow time — never a silent roll
///   forward, because the owner was told "tonight".
/// - `tomorrow-0900` / `tomorrow-1400` — next CALENDAR day (always future).
/// - `next-monday-0900` — strictly 1..=7 days ahead (a Monday click → +7d).
pub fn resolve_token_in<Tz: TimeZone>(token: &str, now: DateTime<Tz>) -> Result<i64, String> {
    let tz = now.timezone();
    let today = now.date_naive();
    let now_ms = now.timestamp_millis();
    match token {
        "in1h" => Ok(now_ms + 3_600_000),
        "in3h" => Ok(now_ms + 3 * 3_600_000),
        "tonight-1900" => {
            let at = resolve_wall_time(&tz, today.and_time(at_hm(19, 0)));
            if at <= now_ms + MIN_LEAD_MS {
                return Err(
                    "tonight 7pm is already past — use Custom… and enter \
                     \"tomorrow 7pm\" instead"
                        .to_string(),
                );
            }
            Ok(at)
        }
        "tomorrow-0900" => Ok(resolve_wall_time(
            &tz,
            add_days(today, 1).and_time(at_hm(9, 0)),
        )),
        "tomorrow-1400" => Ok(resolve_wall_time(
            &tz,
            add_days(today, 1).and_time(at_hm(14, 0)),
        )),
        "next-monday-0900" => {
            // Strictly 1..=7 days ahead: Mon→7, Tue→6, …, Sun→1. "Next
            // Monday" clicked ON a Monday means the following week, not
            // "in two minutes".
            let ahead = 7 - u64::from(today.weekday().num_days_from_monday());
            Ok(resolve_wall_time(
                &tz,
                add_days(today, ahead).and_time(at_hm(9, 0)),
            ))
        }
        other => Err(format!("unknown schedule option \"{other}\"")),
    }
}

/// Generic-timezone core of [`parse_send_at`]. Accepted forms:
///
/// - RFC3339 with offset (`2026-09-01T09:00:00-04:00`) — absolute instant.
/// - `YYYY-MM-DD HH:MM` — owner-local naive datetime.
/// - `in Nm` / `in Nh` / `in Nd` — pure offsets, DST-immune.
/// - `tomorrow [time]` — next calendar day, default 09:00.
/// - weekday name `[time]` (`fri 14:30`, `monday 9am`) — next occurrence,
///   strictly 1..=7 days ahead, default 09:00.
/// - bare time (`HH:MM`, `7pm`, `7:30pm`) — today, or tomorrow if past.
///
/// Rejections return an error message listing the accepted formats so the
/// ephemeral Discord error is self-serve.
pub fn parse_send_at_in<Tz: TimeZone>(input: &str, now: DateTime<Tz>) -> Result<i64, String> {
    parse_core(input, now).map(|(at, _)| at)
}

/// [`parse_send_at_in`] plus how DST adjusted the wall time (#1291). One
/// grammar for both, so the confirmation path can never resolve a different
/// instant than the Discord modal and `--send-at`.
fn parse_core<Tz: TimeZone>(
    input: &str,
    now: DateTime<Tz>,
) -> Result<(i64, Option<DstAdjustment>), String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(format_error());
    }
    // Absolute instant with an explicit offset — timezone-independent, parsed
    // before any case-folding.
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok((dt.timestamp_millis(), None));
    }

    let s = raw.to_ascii_lowercase();
    let tz = now.timezone();
    let today = now.date_naive();
    let now_ms = now.timestamp_millis();

    // "YYYY-MM-DD HH:MM" — owner-local wall time.
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M") {
        return Ok(resolve_wall_time_adj(&tz, ndt));
    }

    // "in Nm/Nh/Nd" — instant math. `parse_offset` caps the multiply, but a
    // pathological N can still push the epoch sum past i64 — overflow is a
    // parse error, never a panic (#501 review).
    if let Some(rest) = s.strip_prefix("in ") {
        let delta = parse_offset(rest.trim()).ok_or_else(format_error)?;
        return now_ms
            .checked_add(delta)
            .map(|at| (at, None))
            .ok_or_else(format_error);
    }

    // "tomorrow [time]" — calendar day, default 09:00.
    if let Some(rest) = s.strip_prefix("tomorrow") {
        let rest = rest.trim();
        let time = if rest.is_empty() {
            at_hm(9, 0)
        } else {
            parse_time_of_day(rest).ok_or_else(format_error)?
        };
        return Ok(resolve_wall_time_adj(
            &tz,
            add_days(today, 1).and_time(time),
        ));
    }

    // Weekday name [time] — next occurrence, strictly 1..=7 days ahead (a
    // "friday" typed on a Friday means next week, matching the select token).
    if let Some((weekday, rest)) = parse_weekday_prefix(&s) {
        let time = if rest.is_empty() {
            at_hm(9, 0)
        } else {
            parse_time_of_day(rest).ok_or_else(format_error)?
        };
        let target = i64::from(weekday.num_days_from_monday());
        let current = i64::from(today.weekday().num_days_from_monday());
        let diff = (target - current).rem_euclid(7);
        let ahead = if diff == 0 { 7 } else { diff as u64 };
        return Ok(resolve_wall_time_adj(
            &tz,
            add_days(today, ahead).and_time(time),
        ));
    }

    // Bare time — today, or tomorrow if that instant is already past.
    if let Some(time) = parse_time_of_day(&s) {
        let at = resolve_wall_time_adj(&tz, today.and_time(time));
        if at.0 <= now_ms {
            return Ok(resolve_wall_time_adj(
                &tz,
                add_days(today, 1).and_time(time),
            ));
        }
        return Ok(at);
    }

    Err(format_error())
}

/// Resolve a naive local wall time to epoch-ms under `tz`, applying the
/// locked DST policy: fall-back overlap → earliest reading; spring-forward
/// gap → shift forward one hour (the gap is one hour in every IANA zone the
/// owner plausibly lives in).
fn resolve_wall_time<Tz: TimeZone>(tz: &Tz, ndt: NaiveDateTime) -> i64 {
    resolve_wall_time_adj(tz, ndt).0
}

/// [`resolve_wall_time`] plus the adjustment it made, if any.
fn resolve_wall_time_adj<Tz: TimeZone>(
    tz: &Tz,
    ndt: NaiveDateTime,
) -> (i64, Option<DstAdjustment>) {
    match tz.from_local_datetime(&ndt) {
        LocalResult::Single(dt) => (dt.timestamp_millis(), None),
        LocalResult::Ambiguous(earliest, latest) => (
            earliest.timestamp_millis(),
            Some(DstAdjustment::Repeated {
                later_ms: latest.timestamp_millis(),
            }),
        ),
        LocalResult::None => match tz.from_local_datetime(&(ndt + Duration::hours(1))) {
            LocalResult::Single(dt) => (dt.timestamp_millis(), Some(DstAdjustment::Skipped)),
            LocalResult::Ambiguous(earliest, _) => {
                (earliest.timestamp_millis(), Some(DstAdjustment::Skipped))
            }
            // Unreachable with real tz data (gaps are one hour); fall back to
            // reading the wall time as UTC rather than failing the schedule.
            LocalResult::None => (
                chrono::Utc.from_utc_datetime(&ndt).timestamp_millis(),
                Some(DstAdjustment::Skipped),
            ),
        },
    }
}

/// "3h" / "45m" / "2d" → milliseconds. `None` on anything else (zero
/// included — "in 0h" is a typo, not a schedule). The unit is taken by
/// `char_indices`, not a byte split: a multi-byte trailing char ("in 3ч")
/// must parse-fail, not panic on a char boundary (#501 review).
fn parse_offset(s: &str) -> Option<i64> {
    let (last_idx, unit) = s.char_indices().last()?;
    let n: i64 = s[..last_idx].trim().parse().ok()?;
    if n < 1 {
        return None;
    }
    let ms = match unit {
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        _ => return None,
    };
    n.checked_mul(ms)
}

/// Parse a time-of-day: `HH:MM` (24h), or `7pm` / `7:30pm` / `7 am`
/// (12h with meridiem). Bare hours WITHOUT am/pm are rejected — "tomorrow 9"
/// is ambiguous where "tomorrow 9am" is not.
fn parse_time_of_day(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let (core, pm) = if let Some(rest) = s.strip_suffix("pm") {
        (rest.trim_end(), Some(true))
    } else if let Some(rest) = s.strip_suffix("am") {
        (rest.trim_end(), Some(false))
    } else {
        (s, None)
    };
    let (h_str, m_str) = match core.split_once(':') {
        Some((h, m)) => (h, Some(m)),
        None => (core, None),
    };
    let h: u32 = h_str.trim().parse().ok()?;
    let m: u32 = match m_str {
        Some(m) => m.trim().parse().ok()?,
        // Without a meridiem, a bare hour is ambiguous — require HH:MM.
        None => {
            pm?;
            0
        }
    };
    if m > 59 {
        return None;
    }
    match pm {
        Some(is_pm) => {
            if !(1..=12).contains(&h) {
                return None;
            }
            let h24 = match (h, is_pm) {
                (12, false) => 0,  // 12am = midnight
                (12, true) => 12,  // 12pm = noon
                (h, false) => h,
                (h, true) => h + 12,
            };
            NaiveTime::from_hms_opt(h24, m, 0)
        }
        None => NaiveTime::from_hms_opt(h, m, 0),
    }
}

/// Match a leading weekday name (full or common abbreviation) and return it
/// with the remaining (trimmed) text.
fn parse_weekday_prefix(s: &str) -> Option<(Weekday, &str)> {
    let (word, rest) = match s.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (s, ""),
    };
    let wd = match word {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    };
    Some((wd, rest))
}

/// Infallible `NaiveTime` for the constant times this module deals in.
fn at_hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap_or(NaiveTime::MIN)
}

/// Infallible calendar-day addition (saturates at the date range edge, which
/// is unreachable for the ≤ 60-day horizon this module serves).
fn add_days(date: NaiveDate, days: u64) -> NaiveDate {
    date.checked_add_days(Days::new(days)).unwrap_or(date)
}

/// The rejection message every parse failure returns — lists the accepted
/// formats so the ephemeral Discord error is actionable without docs.
fn format_error() -> String {
    "couldn't parse that time — accepted formats: \"tomorrow 9am\", \
     \"fri 14:30\", \"in 3h\", \"7pm\", \"2026-09-01 09:00\", or RFC3339 \
     with offset"
        .to_string()
}

// ---------------------------------------------------------------------------
// #1291 — the owner's zone, confirmation-grade resolution, display.
// ---------------------------------------------------------------------------

/// The IANA zone type every surface that shows a send time uses. Zone rules
/// come from the database compiled into `chrono-tz`, never from the host's
/// zoneinfo files, so macOS and Linux resolve the same wall time to the same
/// instant.
pub use chrono_tz::Tz as Zone;

/// The preset send times every surface offers, label → the symbolic token
/// [`resolve_token_in`] resolves at click time (Discord's Schedule select
/// uses the same tokens).
pub const SCHEDULE_PRESETS: &[(&str, &str)] = &[
    ("In 1 hour", "in1h"),
    ("In 3 hours", "in3h"),
    ("Tonight 7pm", "tonight-1900"),
    ("Tomorrow 9am", "tomorrow-0900"),
    ("Tomorrow 2pm", "tomorrow-1400"),
    ("Next Monday 9am", "next-monday-0900"),
];

/// Names the owner's zone (`America/New_York`). Wins over the host zone.
pub const TIMEZONE_ENV: &str = "AUGMENTAGENT_TIMEZONE";

/// How a wall time was adjusted for daylight saving (#499 policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DstAdjustment {
    /// The wall time happens twice (clocks fall back); the first reading is
    /// used. `later_ms` is the second one.
    Repeated { later_ms: i64 },
    /// The wall time never happens (clocks spring forward); it moved one
    /// hour later.
    Skipped,
}

/// A send time ready to be confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSendAt {
    pub at_ms: i64,
    pub dst: Option<DstAdjustment>,
}

/// Why a send time could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendAtError {
    /// The text could mean more than one time; the message asks which.
    Ambiguous(String),
    /// Unparseable, already past, too soon or too far out.
    Invalid(String),
}

impl SendAtError {
    pub fn message(&self) -> &str {
        match self {
            Self::Ambiguous(m) | Self::Invalid(m) => m,
        }
    }
}

/// `name` as an IANA zone (`America/New_York`, or POSIX `TZ` style
/// `:America/New_York`).
pub fn zone_named(name: &str) -> Option<Zone> {
    let name = name.trim();
    let name = name.strip_prefix(':').unwrap_or(name).trim();
    if name.is_empty() {
        return None;
    }
    name.parse::<Zone>().ok()
}

/// The owner's zone: [`TIMEZONE_ENV`], else the `TZ` variable when it names
/// an IANA zone, else the host's configured zone name, else UTC. Only the
/// *name* comes from the host; its rules come from the compiled-in database.
pub fn owner_zone() -> Zone {
    let configured = std::env::var(TIMEZONE_ENV).ok();
    if let Some(bad) = configured.as_deref().filter(|c| zone_named(c).is_none()) {
        tracing::warn!("{TIMEZONE_ENV}=`{bad}` is not an IANA zone name; using the host zone");
    }
    // The host zone's *name*: CoreFoundation on macOS, /etc/localtime's
    // link (or /etc/timezone) on Linux. Its rules still come from chrono-tz.
    let host = iana_time_zone::get_timezone().ok();
    let zone = owner_zone_from(
        configured.as_deref(),
        std::env::var("TZ").ok().as_deref(),
        host.as_deref(),
    );
    if zone == chrono_tz::UTC && host.as_deref().and_then(zone_named).is_none() {
        tracing::warn!(
            "could not tell the owner's time zone; send times use UTC. Set {TIMEZONE_ENV} \
             (for example America/New_York)"
        );
    }
    zone
}

/// [`owner_zone`] over explicit inputs (tests).
pub fn owner_zone_from(configured: Option<&str>, tz_env: Option<&str>, host: Option<&str>) -> Zone {
    [configured, tz_env, host]
        .into_iter()
        .flatten()
        .find_map(zone_named)
        .unwrap_or(chrono_tz::UTC)
}

/// Epoch-ms as a moment in `zone`.
pub fn at_in(ms: i64, zone: &Zone) -> DateTime<Zone> {
    zone.timestamp_millis_opt(ms).single().unwrap_or_else(|| {
        chrono::Utc
            .timestamp_millis_opt(0)
            .unwrap()
            .with_timezone(zone)
    })
}

/// A send time the way every confirmation and notice shows it:
/// `Wed Sep 30, 9:00 AM EDT (America/New_York)`.
pub fn describe_send_time(at_ms: i64, zone: &Zone) -> String {
    format!(
        "{} ({})",
        at_in(at_ms, zone).format("%a %b %-d, %-I:%M %p %Z"),
        zone.name()
    )
}

/// The sentence a confirmation adds when daylight saving moved or doubled
/// the requested wall time. `None` when it did not.
pub fn describe_dst(resolved: &ResolvedSendAt, zone: &Zone) -> Option<String> {
    let at = at_in(resolved.at_ms, zone);
    match resolved.dst? {
        DstAdjustment::Skipped => {
            // The wall time asked for: one hour before the shifted reading.
            let asked = at.naive_local() - Duration::hours(1);
            Some(format!(
                "Clocks spring forward that night: {} does not exist, so it sends an hour later, at {}.",
                asked.format("%-I:%M %p"),
                at.format("%-I:%M %p %Z")
            ))
        }
        DstAdjustment::Repeated { later_ms } => {
            let later = at_in(later_ms, zone);
            Some(format!(
                "Clocks fall back that night, so {} happens twice. This is the first ({}); for \
                 the second ({}) give the time with its offset, e.g. `{}`.",
                at.format("%-I:%M %p"),
                at.format("%-I:%M %p %Z"),
                later.format("%-I:%M %p %Z"),
                later.format("%Y-%m-%dT%H:%M:%S%:z")
            ))
        }
    }
}

/// `"tomorrow 9"` / `"fri 9"` / `"9"`: the hour a bare number names, when
/// the text is otherwise a time the parser would accept with am/pm.
fn bare_hour(s: &str) -> Option<(String, u32)> {
    let s = s.trim().to_ascii_lowercase();
    let (prefix, rest) = if let Some(rest) = s.strip_prefix("tomorrow") {
        ("tomorrow ".to_string(), rest.trim().to_string())
    } else if let Some((_, rest)) = parse_weekday_prefix(&s) {
        let word = s.split_whitespace().next().unwrap_or_default();
        (format!("{word} "), rest.to_string())
    } else {
        (String::new(), s.clone())
    };
    let h: u32 = rest.parse().ok()?;
    (1..=12).contains(&h).then_some((prefix, h))
}

/// Resolve free text to a send time the owner can confirm: the
/// [`parse_send_at_in`] grammar, plus the checks a confirmation needs. A
/// bare hour without am/pm asks which; a time already past, too soon or
/// beyond the horizon is refused ([`validate_send_at`]). DST follows the
/// #499 policy and is reported in [`ResolvedSendAt::dst`].
pub fn resolve_send_at_in<Tz: TimeZone>(
    input: &str,
    now: DateTime<Tz>,
) -> Result<ResolvedSendAt, SendAtError> {
    if let Some((prefix, h)) = bare_hour(input) {
        return Err(SendAtError::Ambiguous(format!(
            "“{}” could be {h}am or {h}pm — say which, e.g. `{prefix}{h}am` or `{prefix}{h}pm` \
             (or 24-hour `{prefix}{h:02}:00`).",
            input.trim()
        )));
    }
    let now_ms = now.timestamp_millis();
    let (at_ms, dst) = parse_core(input, now).map_err(SendAtError::Invalid)?;
    if at_ms <= now_ms {
        return Err(SendAtError::Invalid(
            "that time has already passed — give a time in the future".into(),
        ));
    }
    validate_send_at(at_ms, now_ms).map_err(SendAtError::Invalid)?;
    Ok(ResolvedSendAt { at_ms, dst })
}

#[cfg(test)]
mod confirm_tests {
    use super::*;
    use chrono_tz::America::New_York;

    fn ny(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Zone> {
        New_York
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("unambiguous fixture")
    }

    fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::Utc
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn tomorrow_9am_resolves_in_the_owner_zone_and_is_shown_with_it() {
        let now = ny(2026, 9, 29, 10, 0);
        let r = resolve_send_at_in("tomorrow 9am", now).unwrap();
        assert_eq!(r.at_ms, utc_ms(2026, 9, 30, 13, 0));
        assert_eq!(r.dst, None);
        assert_eq!(
            describe_send_time(r.at_ms, &New_York),
            "Wed Sep 30, 9:00 AM EDT (America/New_York)"
        );
    }

    #[test]
    fn a_bare_hour_asks_whether_am_or_pm_instead_of_guessing() {
        let now = ny(2026, 9, 29, 10, 0);
        for text in ["tomorrow 9", "fri 9", "9"] {
            match resolve_send_at_in(text, now) {
                Err(SendAtError::Ambiguous(m)) => {
                    assert!(m.contains("9am") && m.contains("9pm"), "{text}: {m}")
                }
                other => panic!("{text}: expected a prompt, got {other:?}"),
            }
        }
        // Unparseable text is refused with the accepted formats, not a prompt.
        assert!(matches!(
            resolve_send_at_in("whenever", now),
            Err(SendAtError::Invalid(m)) if m.contains("tomorrow 9am")
        ));
    }

    #[test]
    fn a_time_already_past_is_refused_not_rolled_forward() {
        let now = ny(2026, 9, 29, 10, 0);
        match resolve_send_at_in("2026-09-28 09:00", now) {
            Err(SendAtError::Invalid(m)) => assert!(m.contains("already passed"), "{m}"),
            other => panic!("{other:?}"),
        }
        match resolve_send_at_in("2026-09-29T09:00:00-04:00", now) {
            Err(SendAtError::Invalid(m)) => assert!(m.contains("already passed"), "{m}"),
            other => panic!("{other:?}"),
        }
        // Too soon and too far keep the central guard's wording.
        assert!(matches!(
            resolve_send_at_in("in 1m", now),
            Err(SendAtError::Invalid(m)) if m.contains("too soon")
        ));
        assert!(matches!(
            resolve_send_at_in("in 90d", now),
            Err(SendAtError::Invalid(m)) if m.contains("too far")
        ));
    }

    #[test]
    fn a_spring_forward_gap_moves_one_hour_later_and_says_so() {
        // 2026-03-08 02:00 EST → 03:00 EDT in New York.
        let now = ny(2026, 3, 7, 12, 0);
        let r = resolve_send_at_in("tomorrow 2:30am", now).unwrap();
        assert_eq!(r.at_ms, utc_ms(2026, 3, 8, 7, 30));
        assert_eq!(r.dst, Some(DstAdjustment::Skipped));
        assert_eq!(
            describe_send_time(r.at_ms, &New_York),
            "Sun Mar 8, 3:30 AM EDT (America/New_York)"
        );
        let note = describe_dst(&r, &New_York).unwrap();
        assert!(note.contains("2:30 AM does not exist"), "{note}");
        // A calendar day, not now + 24h, across the transition.
        let nine = resolve_send_at_in("tomorrow 9am", now).unwrap();
        assert_eq!(nine.at_ms, utc_ms(2026, 3, 8, 13, 0));
        assert_eq!(nine.dst, None);
    }

    #[test]
    fn a_fall_back_overlap_uses_the_first_reading_and_names_the_second() {
        // 2026-11-01 02:00 EDT → 01:00 EST in New York: 1:30 happens twice.
        let now = ny(2026, 10, 31, 12, 0);
        let r = resolve_send_at_in("tomorrow 1:30am", now).unwrap();
        assert_eq!(r.at_ms, utc_ms(2026, 11, 1, 5, 30));
        assert_eq!(
            r.dst,
            Some(DstAdjustment::Repeated {
                later_ms: utc_ms(2026, 11, 1, 6, 30)
            })
        );
        assert_eq!(
            describe_send_time(r.at_ms, &New_York),
            "Sun Nov 1, 1:30 AM EDT (America/New_York)"
        );
        let note = describe_dst(&r, &New_York).unwrap();
        assert!(note.contains("happens twice"), "{note}");
        assert!(note.contains("1:30 AM EST"), "names the second: {note}");
        // The owner can pick the second reading with an explicit offset.
        let later = resolve_send_at_in("2026-11-01T01:30:00-05:00", now).unwrap();
        assert_eq!(later.at_ms, utc_ms(2026, 11, 1, 6, 30));
    }

    #[test]
    fn the_legacy_parser_is_unchanged_by_the_dst_reporting() {
        let now = ny(2026, 3, 7, 12, 0);
        assert_eq!(
            parse_send_at_in("tomorrow 2:30am", now).unwrap(),
            utc_ms(2026, 3, 8, 7, 30)
        );
        let fall = ny(2026, 10, 31, 12, 0);
        assert_eq!(
            parse_send_at_in("tomorrow 1:30am", fall).unwrap(),
            utc_ms(2026, 11, 1, 5, 30)
        );
    }

    #[test]
    fn the_owner_zone_is_named_not_read_from_host_zoneinfo_files() {
        // Configured wins; POSIX `:Zone` form is accepted.
        assert_eq!(
            owner_zone_from(Some("Europe/Berlin"), Some("Asia/Tokyo"), Some("UTC")),
            chrono_tz::Europe::Berlin
        );
        assert_eq!(
            owner_zone_from(None, Some(":Asia/Tokyo"), Some("UTC")),
            chrono_tz::Asia::Tokyo
        );
        // TZ that is not an IANA name (a POSIX rule string) falls through to
        // the host zone name.
        assert_eq!(
            owner_zone_from(
                None,
                Some("EST5EDT,M3.2.0,M11.1.0"),
                Some("America/Chicago")
            ),
            chrono_tz::America::Chicago
        );
        // A bad configured name falls through too; nothing left means UTC.
        assert_eq!(
            owner_zone_from(Some("Mars/Olympus"), None, None),
            chrono_tz::UTC
        );
        assert_eq!(zone_named(" America/New_York "), Some(New_York));
        assert_eq!(zone_named("nope"), None);
        // The same instant renders identically whatever the host is: the
        // rules are compiled in.
        assert_eq!(
            describe_send_time(utc_ms(2026, 1, 15, 14, 0), &New_York),
            "Thu Jan 15, 9:00 AM EST (America/New_York)"
        );
    }
}
