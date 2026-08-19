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

/// The central time guard (#501): one validation shared by the select tokens,
/// the custom modal, and (later) `--send-at`, enforced at the CAS layer in
/// `ApprovalActionHandler::schedule` — not per-parser, so no entry point can
/// forget it.
pub fn validate_send_at(at_ms: i64, now_ms: i64) -> Result<(), String> {
    if at_ms <= now_ms + MIN_LEAD_MS {
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
    let raw = input.trim();
    if raw.is_empty() {
        return Err(format_error());
    }
    // Absolute instant with an explicit offset — timezone-independent, parsed
    // before any case-folding.
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.timestamp_millis());
    }

    let s = raw.to_ascii_lowercase();
    let tz = now.timezone();
    let today = now.date_naive();
    let now_ms = now.timestamp_millis();

    // "YYYY-MM-DD HH:MM" — owner-local wall time.
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M") {
        return Ok(resolve_wall_time(&tz, ndt));
    }

    // "in Nm/Nh/Nd" — instant math.
    if let Some(rest) = s.strip_prefix("in ") {
        let delta = parse_offset(rest.trim()).ok_or_else(format_error)?;
        return Ok(now_ms + delta);
    }

    // "tomorrow [time]" — calendar day, default 09:00.
    if let Some(rest) = s.strip_prefix("tomorrow") {
        let rest = rest.trim();
        let time = if rest.is_empty() {
            at_hm(9, 0)
        } else {
            parse_time_of_day(rest).ok_or_else(format_error)?
        };
        return Ok(resolve_wall_time(&tz, add_days(today, 1).and_time(time)));
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
        return Ok(resolve_wall_time(&tz, add_days(today, ahead).and_time(time)));
    }

    // Bare time — today, or tomorrow if that instant is already past.
    if let Some(time) = parse_time_of_day(&s) {
        let at = resolve_wall_time(&tz, today.and_time(time));
        if at <= now_ms {
            return Ok(resolve_wall_time(&tz, add_days(today, 1).and_time(time)));
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
    match tz.from_local_datetime(&ndt) {
        LocalResult::Single(dt) => dt.timestamp_millis(),
        LocalResult::Ambiguous(earliest, _) => earliest.timestamp_millis(),
        LocalResult::None => match tz.from_local_datetime(&(ndt + Duration::hours(1))) {
            LocalResult::Single(dt) => dt.timestamp_millis(),
            LocalResult::Ambiguous(earliest, _) => earliest.timestamp_millis(),
            // Unreachable with real tz data (gaps are one hour); fall back to
            // reading the wall time as UTC rather than failing the schedule.
            LocalResult::None => chrono::Utc.from_utc_datetime(&ndt).timestamp_millis(),
        },
    }
}

/// "3h" / "45m" / "2d" → milliseconds. `None` on anything else (zero
/// included — "in 0h" is a typo, not a schedule).
fn parse_offset(s: &str) -> Option<i64> {
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: i64 = num.trim().parse().ok()?;
    if n < 1 {
        return None;
    }
    let ms = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
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
