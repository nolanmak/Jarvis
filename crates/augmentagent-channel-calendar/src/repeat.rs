//! #2042 — recurrence for `calendar create-event --repeat`.
//!
//! A small preset set maps to RFC 5545 RRULE lines Google accepts in
//! `events.insert`'s `recurrence` array. Weekly and monthly rules carry no
//! BYDAY/BYMONTHDAY, so Google follows the start's weekday / day of month.
//! A recurring event must also carry an IANA `timeZone` on start and end so
//! occurrences keep their local time across DST; [`build`] needs one.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;

/// `--repeat` presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Daily,
    Weekdays,
    Weekly,
    Biweekly,
    Monthly,
}

/// When the series stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatEnd {
    Never,
    /// Total occurrences, including the first.
    Count(u32),
    /// Last local date an occurrence may fall on (inclusive).
    Until(NaiveDate),
}

pub const MIN_COUNT: u32 = 2;
pub const MAX_COUNT: u32 = 365;

impl Repeat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "daily" => Some(Self::Daily),
            "weekdays" | "weekday" | "every-weekday" => Some(Self::Weekdays),
            "weekly" => Some(Self::Weekly),
            "biweekly" | "fortnightly" | "every-other-week" => Some(Self::Biweekly),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }

    fn rule(self) -> &'static str {
        match self {
            Self::Daily => "FREQ=DAILY",
            Self::Weekdays => "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR",
            Self::Weekly => "FREQ=WEEKLY",
            Self::Biweekly => "FREQ=WEEKLY;INTERVAL=2",
            Self::Monthly => "FREQ=MONTHLY",
        }
    }
}

/// Turn the CLI's `--repeat-count` / `--repeat-until` into a [`RepeatEnd`].
pub fn parse_end(count: Option<u32>, until: Option<&str>) -> Result<RepeatEnd, String> {
    match (count, until) {
        (Some(_), Some(_)) => Err("give --repeat-count or --repeat-until, not both".into()),
        (Some(n), None) if !(MIN_COUNT..=MAX_COUNT).contains(&n) => {
            Err(format!("--repeat-count must be 2-365 (got {n})"))
        }
        (Some(n), None) => Ok(RepeatEnd::Count(n)),
        (None, Some(d)) => NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d")
            .map(RepeatEnd::Until)
            .map_err(|_| format!("--repeat-until must be YYYY-MM-DD (got {d:?})")),
        (None, None) => Ok(RepeatEnd::Never),
    }
}

/// The RRULE line (`RRULE:FREQ=…`) for a series starting at `start` in `tz`.
pub fn build(
    repeat: Repeat,
    end: RepeatEnd,
    start: DateTime<FixedOffset>,
    tz: Tz,
) -> Result<String, String> {
    let mut rule = format!("RRULE:{}", repeat.rule());
    match end {
        RepeatEnd::Never => {}
        RepeatEnd::Count(n) => rule.push_str(&format!(";COUNT={n}")),
        RepeatEnd::Until(day) => {
            let first = start.with_timezone(&tz).date_naive();
            if day < first {
                return Err(format!(
                    "--repeat-until {day} is before the start date {first}"
                ));
            }
            // UNTIL must be UTC when DTSTART has a time: the last second of
            // that local day.
            let last = day
                .and_hms_opt(23, 59, 59)
                .and_then(|t| tz.from_local_datetime(&t).earliest())
                .ok_or_else(|| format!("--repeat-until {day} has no 23:59:59 in {tz}"))?;
            rule.push_str(&format!(
                ";UNTIL={}",
                last.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")
            ));
        }
    }
    Ok(rule)
}

/// Plain words for the card: "weekly on Tue, 10 times".
pub fn describe(repeat: Repeat, end: RepeatEnd, start: DateTime<FixedOffset>, tz: Tz) -> String {
    let local = start.with_timezone(&tz);
    let weekday = local.format("%a");
    let what = match repeat {
        Repeat::Daily => "daily".to_string(),
        Repeat::Weekdays => "every weekday".to_string(),
        Repeat::Weekly => format!("weekly on {weekday}"),
        Repeat::Biweekly => format!("every 2 weeks on {weekday}"),
        Repeat::Monthly => format!("monthly on day {}", local.day()),
    };
    match end {
        RepeatEnd::Never => format!("{what}, no end date"),
        RepeatEnd::Count(n) => format!("{what}, {n} times"),
        RepeatEnd::Until(day) => format!("{what} until {day}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NY: Tz = chrono_tz::America::New_York;

    /// Tue 2026-11-17 10:00 New York.
    fn start() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-11-17T10:00:00-05:00").unwrap()
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    // AC1 — presets map to RRULEs.
    #[test]
    fn presets_map_to_rrules() {
        let cases = [
            ("daily", "RRULE:FREQ=DAILY"),
            ("weekdays", "RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
            ("weekly", "RRULE:FREQ=WEEKLY"),
            ("biweekly", "RRULE:FREQ=WEEKLY;INTERVAL=2"),
            ("monthly", "RRULE:FREQ=MONTHLY"),
        ];
        for (name, rule) in cases {
            let r = Repeat::parse(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(build(r, RepeatEnd::Never, start(), NY).unwrap(), rule);
        }
        assert_eq!(Repeat::parse("Weekly"), Some(Repeat::Weekly));
        assert_eq!(Repeat::parse("every-other-week"), Some(Repeat::Biweekly));
        assert_eq!(Repeat::parse("yearly"), None);
    }

    // AC2 — count and until.
    #[test]
    fn count_is_appended_and_bounded() {
        assert_eq!(
            build(Repeat::Weekly, RepeatEnd::Count(10), start(), NY).unwrap(),
            "RRULE:FREQ=WEEKLY;COUNT=10"
        );
        assert!(parse_end(Some(1), None).unwrap_err().contains("2-365"));
        assert!(parse_end(Some(366), None).unwrap_err().contains("2-365"));
        assert_eq!(parse_end(Some(2), None).unwrap(), RepeatEnd::Count(2));
        assert_eq!(parse_end(None, None).unwrap(), RepeatEnd::Never);
    }

    #[test]
    fn until_is_the_end_of_that_local_day_in_utc() {
        let end = parse_end(None, Some("2027-03-31")).unwrap();
        assert_eq!(end, RepeatEnd::Until(date("2027-03-31")));
        // 2027-03-31 23:59:59 EDT (-04:00) = 2027-04-01 03:59:59Z.
        assert_eq!(
            build(Repeat::Weekly, end, start(), NY).unwrap(),
            "RRULE:FREQ=WEEKLY;UNTIL=20270401T035959Z"
        );
    }

    #[test]
    fn until_must_not_precede_the_start_and_excludes_count() {
        let e = build(
            Repeat::Weekly,
            RepeatEnd::Until(date("2026-11-16")),
            start(),
            NY,
        )
        .unwrap_err();
        assert!(e.contains("before"), "{e}");
        assert!(parse_end(Some(3), Some("2027-01-01"))
            .unwrap_err()
            .contains("not both"));
        assert!(parse_end(None, Some("next march"))
            .unwrap_err()
            .contains("YYYY-MM-DD"));
    }

    // AC4 — the card's plain words.
    #[test]
    fn describes_the_series_in_plain_words() {
        assert_eq!(
            describe(Repeat::Weekly, RepeatEnd::Count(10), start(), NY),
            "weekly on Tue, 10 times"
        );
        assert_eq!(
            describe(
                Repeat::Weekdays,
                RepeatEnd::Until(date("2027-03-31")),
                start(),
                NY
            ),
            "every weekday until 2027-03-31"
        );
        assert_eq!(
            describe(Repeat::Biweekly, RepeatEnd::Never, start(), NY),
            "every 2 weeks on Tue, no end date"
        );
        assert_eq!(
            describe(Repeat::Monthly, RepeatEnd::Never, start(), NY),
            "monthly on day 17, no end date"
        );
        assert_eq!(
            describe(Repeat::Daily, RepeatEnd::Count(5), start(), NY),
            "daily, 5 times"
        );
    }
}
