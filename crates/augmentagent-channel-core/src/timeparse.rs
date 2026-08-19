//! #501 — deterministic send-time parsing for scheduled email sends.
//!
//! This is the crate the epic (#499) names as the parser's shared home — the
//! query-mode `--send-at` flag (#502) consumes it from here. The
//! implementation itself lives in `augmentagent-approval-discord::timeparse`
//! and is re-exported wholesale: the Discord select/modal arms in that
//! crate's event handler need `resolve_token`/`parse_send_at` at click time,
//! and the workspace dependency edge runs THIS crate → approval-discord
//! (`engagement.rs::ApprovalBroker`), so the code cannot live here without a
//! cycle. Callers on this side see one canonical path:
//! `augmentagent_channel_core::timeparse::*`.
//!
//! The full parser table + DST fixture tests live in this file (pinned to
//! `chrono_tz::America::New_York` via the generic `*_in` entry points —
//! `chrono::Local` reads `TZ` once at process start, so env-pinning inside a
//! test process is unreliable).

pub use augmentagent_approval_discord::timeparse::{
    parse_send_at, parse_send_at_in, resolve_token, resolve_token_in, validate_send_at,
    MAX_HORIZON_MS, MIN_LEAD_MS, SKEW_MS,
};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone};
    use chrono_tz::America::New_York;
    use chrono_tz::Tz;

    /// Fixture "now" in America/New_York. Panics on invalid wall times —
    /// fixtures are hand-picked to be unambiguous.
    fn ny(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        New_York
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("unambiguous fixture time")
    }

    /// Expected epoch-ms of an unambiguous New_York wall time.
    fn ny_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        ny(y, mo, d, h, mi).timestamp_millis()
    }

    // ------------------------------------------------------------------
    // validate_send_at — the central guard.
    // ------------------------------------------------------------------

    #[test]
    fn guard_rejects_too_soon_and_too_far() {
        let now = 1_000_000_000;
        assert!(validate_send_at(now, now).is_err(), "now itself is too soon");
        // The lead bound carries a skew allowance (#501 review): the exact
        // minimum must validate (parse-to-validate latency), anything under
        // the allowance must not.
        assert!(validate_send_at(now + MIN_LEAD_MS, now).is_ok());
        assert!(validate_send_at(now + MIN_LEAD_MS - SKEW_MS, now).is_ok());
        let err =
            validate_send_at(now + MIN_LEAD_MS - SKEW_MS - 1, now).unwrap_err();
        assert!(err.contains("too soon"), "got: {err}");
        assert!(validate_send_at(now + MAX_HORIZON_MS, now).is_ok());
        let err = validate_send_at(now + MAX_HORIZON_MS + 1, now).unwrap_err();
        assert!(err.contains("too far"), "got: {err}");
    }

    #[test]
    fn advertised_minimum_lead_actually_validates() {
        // "in 2m" resolves to exactly now+MIN_LEAD at parse time and reaches
        // the guard a store round-trip later — the skew allowance is what
        // makes the advertised minimum usable (#501 review).
        let now = ny(2026, 7, 20, 10, 0);
        let now_ms = now.timestamp_millis();
        let at = parse_send_at_in("in 2m", now).unwrap();
        assert_eq!(at, now_ms + MIN_LEAD_MS);
        assert!(validate_send_at(at, now_ms).is_ok());
        assert!(validate_send_at(at, now_ms + 10_000).is_ok(), "within skew");
        assert!(
            validate_send_at(at, now_ms + SKEW_MS + 1_000).is_err(),
            "beyond the allowance the guard still bites"
        );
    }

    // ------------------------------------------------------------------
    // resolve_token — symbolic select values, click-time rules.
    // ------------------------------------------------------------------

    #[test]
    fn token_offsets_are_pure_instant_math() {
        let now = ny(2026, 7, 20, 10, 0);
        let now_ms = now.timestamp_millis();
        assert_eq!(resolve_token_in("in1h", now).unwrap(), now_ms + 3_600_000);
        assert_eq!(
            resolve_token_in("in3h", now).unwrap(),
            now_ms + 3 * 3_600_000
        );
    }

    #[test]
    fn tonight_resolves_to_1900_today() {
        let now = ny(2026, 7, 20, 10, 0);
        assert_eq!(
            resolve_token_in("tonight-1900", now).unwrap(),
            ny_ms(2026, 7, 20, 19, 0)
        );
    }

    #[test]
    fn tonight_errors_instead_of_rolling_forward() {
        // 19:01 — "tonight 7pm" is past; the owner was told "tonight", so
        // this must be an ERROR steering to tomorrow, never a silent +1d.
        let err = resolve_token_in("tonight-1900", ny(2026, 7, 20, 19, 1)).unwrap_err();
        assert!(err.contains("tomorrow 7pm"), "must suggest tomorrow: {err}");
        // 18:59 — inside the 2-minute lead window: same rejection.
        assert!(resolve_token_in("tonight-1900", ny(2026, 7, 20, 18, 59)).is_err());
    }

    #[test]
    fn tomorrow_tokens_are_next_calendar_day() {
        let now = ny(2026, 7, 20, 23, 50);
        assert_eq!(
            resolve_token_in("tomorrow-0900", now).unwrap(),
            ny_ms(2026, 7, 21, 9, 0)
        );
        assert_eq!(
            resolve_token_in("tomorrow-1400", now).unwrap(),
            ny_ms(2026, 7, 21, 14, 0)
        );
    }

    #[test]
    fn next_monday_is_strictly_one_to_seven_days_ahead() {
        // 2026-07-20 is a Monday: "next Monday" clicked ON a Monday → +7d.
        assert_eq!(
            resolve_token_in("next-monday-0900", ny(2026, 7, 20, 10, 0)).unwrap(),
            ny_ms(2026, 7, 27, 9, 0)
        );
        // Sunday 2026-07-26 → the very next day.
        assert_eq!(
            resolve_token_in("next-monday-0900", ny(2026, 7, 26, 10, 0)).unwrap(),
            ny_ms(2026, 7, 27, 9, 0)
        );
        // Tuesday 2026-07-21 → six days out.
        assert_eq!(
            resolve_token_in("next-monday-0900", ny(2026, 7, 21, 10, 0)).unwrap(),
            ny_ms(2026, 7, 27, 9, 0)
        );
    }

    #[test]
    fn unknown_token_is_rejected() {
        assert!(resolve_token_in("in5h", ny(2026, 7, 20, 10, 0)).is_err());
    }

    // ------------------------------------------------------------------
    // parse_send_at — free-text table.
    // ------------------------------------------------------------------

    #[test]
    fn parses_rfc3339_with_offset_as_absolute_instant() {
        let now = ny(2026, 7, 20, 10, 0);
        let got = parse_send_at_in("2026-09-01T09:00:00-04:00", now).unwrap();
        assert_eq!(got, ny_ms(2026, 9, 1, 9, 0));
        // A different offset is a different instant — offsets are honored,
        // not re-localized. 09:00-04:00 is four hours AFTER 09:00Z.
        let utc = parse_send_at_in("2026-09-01T09:00:00Z", now).unwrap();
        assert_eq!(got - utc, 4 * 3_600_000);
    }

    #[test]
    fn parses_naive_date_time_as_owner_local() {
        let now = ny(2026, 7, 20, 10, 0);
        assert_eq!(
            parse_send_at_in("2026-09-01 09:00", now).unwrap(),
            ny_ms(2026, 9, 1, 9, 0)
        );
    }

    #[test]
    fn parses_relative_offsets() {
        let now = ny(2026, 7, 20, 10, 0);
        let now_ms = now.timestamp_millis();
        assert_eq!(parse_send_at_in("in 45m", now).unwrap(), now_ms + 45 * 60_000);
        assert_eq!(
            parse_send_at_in("in 3h", now).unwrap(),
            now_ms + 3 * 3_600_000
        );
        assert_eq!(
            parse_send_at_in("in 2d", now).unwrap(),
            now_ms + 2 * 86_400_000
        );
        assert!(parse_send_at_in("in 0h", now).is_err(), "zero offset is a typo");
        assert!(parse_send_at_in("in 3w", now).is_err(), "unknown unit");
    }

    #[test]
    fn multibyte_offset_unit_is_rejected_not_a_panic() {
        // A multi-byte trailing char used to hit a byte-boundary split in
        // parse_offset and panic (#501 review) — it must parse-fail.
        let now = ny(2026, 7, 20, 10, 0);
        for bad in ["in 3ч", "in 3時", "in ч"] {
            let err = parse_send_at_in(bad, now.clone())
                .expect_err(&format!("{bad:?} must be rejected"));
            assert!(err.contains("accepted formats"), "got: {err}");
        }
    }

    #[test]
    fn absurd_offsets_error_instead_of_overflowing() {
        let now = ny(2026, 7, 20, 10, 0);
        // Multiply overflow inside parse_offset.
        assert!(parse_send_at_in("in 9223372036854775807m", now.clone()).is_err());
        // Multiply fits, epoch addition would overflow — checked_add path.
        assert!(parse_send_at_in("in 153722867280912m", now).is_err());
    }

    #[test]
    fn parses_tomorrow_with_default_and_explicit_time() {
        let now = ny(2026, 7, 20, 10, 0);
        assert_eq!(
            parse_send_at_in("tomorrow", now.clone()).unwrap(),
            ny_ms(2026, 7, 21, 9, 0),
            "bare tomorrow defaults to 09:00"
        );
        assert_eq!(
            parse_send_at_in("tomorrow 2pm", now.clone()).unwrap(),
            ny_ms(2026, 7, 21, 14, 0)
        );
        assert_eq!(
            parse_send_at_in("Tomorrow 14:30", now).unwrap(),
            ny_ms(2026, 7, 21, 14, 30),
            "case-insensitive"
        );
    }

    #[test]
    fn parses_bare_time_today_or_tomorrow_if_past() {
        let now = ny(2026, 7, 20, 10, 0);
        assert_eq!(
            parse_send_at_in("7pm", now.clone()).unwrap(),
            ny_ms(2026, 7, 20, 19, 0),
            "future today stays today"
        );
        assert_eq!(
            parse_send_at_in("7:30pm", now.clone()).unwrap(),
            ny_ms(2026, 7, 20, 19, 30)
        );
        assert_eq!(
            parse_send_at_in("14:30", now.clone()).unwrap(),
            ny_ms(2026, 7, 20, 14, 30)
        );
        assert_eq!(
            parse_send_at_in("7am", now).unwrap(),
            ny_ms(2026, 7, 21, 7, 0),
            "past today rolls to tomorrow"
        );
    }

    #[test]
    fn parses_weekday_names_next_occurrence() {
        // 2026-07-20 is a Monday.
        let now = ny(2026, 7, 20, 10, 0);
        assert_eq!(
            parse_send_at_in("fri 14:30", now.clone()).unwrap(),
            ny_ms(2026, 7, 24, 14, 30)
        );
        assert_eq!(
            parse_send_at_in("friday", now.clone()).unwrap(),
            ny_ms(2026, 7, 24, 9, 0),
            "weekday defaults to 09:00"
        );
        assert_eq!(
            parse_send_at_in("monday 9am", now).unwrap(),
            ny_ms(2026, 7, 27, 9, 0),
            "same weekday means NEXT week, never today"
        );
    }

    #[test]
    fn twelve_am_pm_edges() {
        let now = ny(2026, 7, 20, 10, 0);
        assert_eq!(
            parse_send_at_in("tomorrow 12pm", now.clone()).unwrap(),
            ny_ms(2026, 7, 21, 12, 0),
            "12pm is noon"
        );
        assert_eq!(
            parse_send_at_in("tomorrow 12am", now).unwrap(),
            ny_ms(2026, 7, 21, 0, 0),
            "12am is midnight"
        );
    }

    #[test]
    fn rejections_list_accepted_formats() {
        let now = ny(2026, 7, 20, 10, 0);
        for bad in ["", "whenever", "tomorrow 9", "13pm", "in h", "25:00"] {
            let err = parse_send_at_in(bad, now.clone())
                .expect_err(&format!("{bad:?} must be rejected"));
            assert!(
                err.contains("accepted formats"),
                "{bad:?} error must be self-serve: {err}"
            );
        }
    }

    // ------------------------------------------------------------------
    // DST fixtures — America/New_York, 2026 transitions:
    // spring forward Mar 8 (02:00→03:00), fall back Nov 1 (02:00→01:00).
    // ------------------------------------------------------------------

    #[test]
    fn spring_forward_gap_shifts_plus_one_hour() {
        // 02:30 on 2026-03-08 never exists; the locked policy is +1h.
        let now = ny(2026, 3, 7, 20, 0);
        let got = parse_send_at_in("2026-03-08 02:30", now).unwrap();
        assert_eq!(got, ny_ms(2026, 3, 8, 3, 30));
    }

    #[test]
    fn fall_back_overlap_resolves_to_earliest() {
        // 01:30 on 2026-11-01 happens twice (EDT then EST); take the FIRST.
        let now = ny(2026, 10, 31, 20, 0);
        let got = parse_send_at_in("2026-11-01 01:30", now).unwrap();
        let earliest = New_York
            .with_ymd_and_hms(2026, 11, 1, 1, 30, 0)
            .earliest()
            .expect("ambiguous time has an earliest reading")
            .timestamp_millis();
        assert_eq!(got, earliest);
        // And it precedes the latest reading by exactly the fold hour.
        let latest = New_York
            .with_ymd_and_hms(2026, 11, 1, 1, 30, 0)
            .latest()
            .expect("latest reading")
            .timestamp_millis();
        assert_eq!(latest - got, 3_600_000);
    }

    #[test]
    fn tomorrow_across_spring_forward_is_calendar_day_not_24h() {
        // Sat 2026-03-07 20:00 EST → "tomorrow 9am" must be Sun 2026-03-08
        // 09:00 EDT (12h earlier in absolute terms than a naive +24h-from-9am
        // would suggest — the night is one hour short).
        let now = ny(2026, 3, 7, 20, 0);
        let got = parse_send_at_in("tomorrow 9am", now.clone()).unwrap();
        assert_eq!(got, ny_ms(2026, 3, 8, 9, 0));
        // 13 nominal hours ahead, but only 12 absolute hours away.
        assert_eq!(got - now.timestamp_millis(), 12 * 3_600_000);
        // The select token agrees.
        assert_eq!(resolve_token_in("tomorrow-0900", now).unwrap(), got);
    }

    #[test]
    fn tomorrow_across_fall_back_is_calendar_day_not_24h() {
        // Sat 2026-10-31 20:00 EDT → "tomorrow 9am" = Sun 2026-11-01 09:00
        // EST: 13 nominal hours, 14 absolute (the night is one hour long).
        let now = ny(2026, 10, 31, 20, 0);
        let got = parse_send_at_in("tomorrow 9am", now.clone()).unwrap();
        assert_eq!(got, ny_ms(2026, 11, 1, 9, 0));
        assert_eq!(got - now.timestamp_millis(), 14 * 3_600_000);
    }

    #[test]
    fn local_wrappers_delegate_to_generic_core() {
        // Smoke-test the `DateTime<Local>` wrappers (whatever TZ the test
        // host runs in): instant-math tokens are timezone-independent.
        let now = chrono::Local::now();
        let now_ms = now.timestamp_millis();
        assert_eq!(resolve_token("in1h", now).unwrap(), now_ms + 3_600_000);
        let got = parse_send_at("in 3h", chrono::Local::now()).unwrap();
        assert!((got - (now_ms + 3 * 3_600_000)).abs() < 5_000);
    }
}
