//! Heartbeat configuration from `AUGMENTAGENT_HEARTBEAT_*` env vars.
//!
//! Every knob degrades to a safe default with a warning rather than failing
//! the daemon: a typo in the active-hours window means "always on", the same
//! permissive choice OpenClaw makes, and never "silently never runs".

use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_DAILY_CAP: u32 = 6;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// A daily window in minutes since local midnight. `start` is inclusive,
/// `end` exclusive; `end < start` crosses midnight and `start == end` is
/// never active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHours {
    pub start_min: u32,
    pub end_min: u32,
}

impl ActiveHours {
    /// Parse `HH:MM-HH:MM`; `24:00` is allowed as the end.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let bad = || format!("expected HH:MM-HH:MM, got `{raw}`");
        let (start, end) = raw.trim().split_once('-').ok_or_else(bad)?;
        let start_min = clock_minutes(start)
            .filter(|m| *m < 24 * 60)
            .ok_or_else(bad)?;
        let end_min = clock_minutes(end).ok_or_else(bad)?;
        Ok(Self { start_min, end_min })
    }

    pub fn contains(&self, minute_of_day: u32) -> bool {
        let (start, end) = (self.start_min, self.end_min);
        if start < end {
            (start..end).contains(&minute_of_day)
        } else if start > end {
            minute_of_day >= start || minute_of_day < end
        } else {
            false
        }
    }
}

/// `HH:MM` to minutes since midnight; `24:00` is the only value past 23:59.
fn clock_minutes(raw: &str) -> Option<u32> {
    let (h, m) = raw.trim().split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    match (h, m) {
        (24, 0) => Some(24 * 60),
        (0..=23, 0..=59) => Some(h * 60 + m),
        _ => None,
    }
}

impl std::fmt::Display for ActiveHours {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hm = |m: u32| format!("{:02}:{:02}", m / 60, m % 60);
        write!(f, "{}-{}", hm(self.start_min), hm(self.end_min))
    }
}

#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    pub interval: Duration,
    /// `None` = always active.
    pub active_hours: Option<ActiveHours>,
    /// `None` = the host's local zone.
    pub tz: Option<Tz>,
    /// Maximum notices delivered per rolling 24h.
    pub daily_cap: u32,
    /// Wall-clock budget for one model call.
    pub timeout: Duration,
    /// Human-readable problems found while parsing; logged at startup and
    /// shown by `heartbeat status`.
    pub warnings: Vec<String>,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: DEFAULT_INTERVAL,
            active_hours: None,
            tz: None,
            daily_cap: DEFAULT_DAILY_CAP,
            timeout: DEFAULT_TIMEOUT,
            warnings: Vec::new(),
        }
    }
}

impl HeartbeatConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from an env-like lookup so tests never touch the process env.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut c = Self {
            enabled: get("AUGMENTAGENT_HEARTBEAT_ENABLED").is_some_and(|v| {
                matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
            }),
            ..Self::default()
        };

        if let Some(raw) = get("AUGMENTAGENT_HEARTBEAT_INTERVAL") {
            match augmentagent_approval_discord::parse_interval(&raw).filter(|s| *s > 0) {
                Some(secs) if Duration::from_secs(secs as u64) < MIN_INTERVAL => {
                    c.interval = MIN_INTERVAL;
                    c.warnings.push(format!(
                        "AUGMENTAGENT_HEARTBEAT_INTERVAL `{raw}` is below the 5m floor; using 5m"
                    ));
                }
                Some(secs) => c.interval = Duration::from_secs(secs as u64),
                None => c.warnings.push(format!(
                    "AUGMENTAGENT_HEARTBEAT_INTERVAL `{raw}` is not an interval; using 30m"
                )),
            }
        }

        if let Some(raw) = get("AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS") {
            match ActiveHours::parse(&raw) {
                Ok(window) => c.active_hours = Some(window),
                Err(e) => c.warnings.push(format!(
                    "AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS: {e}; running at all hours"
                )),
            }
        }

        if let Some(raw) = get("AUGMENTAGENT_HEARTBEAT_TZ") {
            match raw.parse::<Tz>() {
                Ok(tz) => c.tz = Some(tz),
                Err(_) => c.warnings.push(format!(
                    "AUGMENTAGENT_HEARTBEAT_TZ `{raw}` is not an IANA zone; using host local time"
                )),
            }
        }

        if let Some(raw) = get("AUGMENTAGENT_HEARTBEAT_DAILY_CAP") {
            match raw.parse() {
                Ok(cap) => c.daily_cap = cap,
                Err(_) => c.warnings.push(format!("AUGMENTAGENT_HEARTBEAT_DAILY_CAP `{raw}` is not a count; using {DEFAULT_DAILY_CAP}")),
            }
        }

        if let Some(raw) = get("AUGMENTAGENT_HEARTBEAT_TIMEOUT_SECS") {
            match raw.parse::<u64>() {
                Ok(secs) if secs > 0 => c.timeout = Duration::from_secs(secs),
                _ => c.warnings.push(format!("AUGMENTAGENT_HEARTBEAT_TIMEOUT_SECS `{raw}` is not a positive number; using 300")),
            }
        }
        c
    }

    /// Whether `now` falls inside the active window in the configured zone.
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        let Some(window) = self.active_hours else {
            return true;
        };
        let (hour, minute) = match self.tz {
            Some(tz) => {
                let t = now.with_timezone(&tz);
                (t.hour(), t.minute())
            }
            None => {
                let t = now.with_timezone(&chrono::Local);
                (t.hour(), t.minute())
            }
        };
        window.contains(hour * 60 + minute)
    }

    /// Liveness probe for `heartbeat status --check`: enabled, and no
    /// attempt within three intervals while the window has been open for
    /// at least that long. A heartbeat that has never run is stale too.
    pub fn is_stale(&self, last_attempt_ms: Option<i64>, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        let grace = chrono::Duration::from_std(self.interval * 3).unwrap_or(chrono::Duration::MAX);
        if !self.is_active_at(now) || !self.is_active_at(now - grace) {
            return false;
        }
        last_attempt_ms.is_none_or(|last| now.timestamp_millis() - last > grace.num_milliseconds())
    }

    /// `2026-09-29 08:15 EDT (Tuesday)` in the configured zone, for prompts.
    pub fn local_time_label(&self, now: DateTime<Utc>) -> String {
        const FORMAT: &str = "%Y-%m-%d %H:%M %Z (%A)";
        match self.tz {
            Some(tz) => now.with_timezone(&tz).format(FORMAT).to_string(),
            None => now.with_timezone(&chrono::Local).format(FORMAT).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg(pairs: &[(&str, &str)]) -> HeartbeatConfig {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        HeartbeatConfig::from_lookup(|k| map.get(k).cloned())
    }

    fn min(h: u32, m: u32) -> u32 {
        h * 60 + m
    }

    #[test]
    fn daytime_window_is_start_inclusive_end_exclusive() {
        let w = ActiveHours::parse("08:00-22:00").unwrap();
        assert!(w.contains(min(8, 0)));
        assert!(w.contains(min(21, 59)));
        assert!(!w.contains(min(22, 0)));
        assert!(!w.contains(min(7, 59)));
    }

    #[test]
    fn window_can_cross_midnight() {
        let w = ActiveHours::parse("22:00-06:00").unwrap();
        assert!(w.contains(min(23, 30)));
        assert!(w.contains(min(5, 59)));
        assert!(!w.contains(min(6, 0)));
        assert!(!w.contains(min(12, 0)));
    }

    #[test]
    fn zero_width_is_never_and_full_day_is_always() {
        let never = ActiveHours::parse("09:00-09:00").unwrap();
        assert!((0..24 * 60).all(|m| !never.contains(m)));
        let always = ActiveHours::parse("00:00-24:00").unwrap();
        assert!((0..24 * 60).all(|m| always.contains(m)));
    }

    #[test]
    fn malformed_windows_are_rejected() {
        for raw in [
            "8-10pm",
            "25:00-26:00",
            "08:00",
            "08:60-09:00",
            "24:00-08:00",
            "",
        ] {
            assert!(ActiveHours::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn defaults_when_nothing_is_set() {
        let c = cfg(&[]);
        assert!(!c.enabled);
        assert_eq!(c.interval, DEFAULT_INTERVAL);
        assert_eq!(c.active_hours, None);
        assert_eq!(c.tz, None);
        assert_eq!(c.daily_cap, DEFAULT_DAILY_CAP);
        assert_eq!(c.timeout, DEFAULT_TIMEOUT);
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn enabled_flag_accepts_common_truthy_spellings_only() {
        for v in ["1", "true", "TRUE", "yes", "On"] {
            assert!(cfg(&[("AUGMENTAGENT_HEARTBEAT_ENABLED", v)]).enabled, "{v}");
        }
        for v in ["0", "false", "off", "", "enable"] {
            assert!(
                !cfg(&[("AUGMENTAGENT_HEARTBEAT_ENABLED", v)]).enabled,
                "{v}"
            );
        }
    }

    #[test]
    fn interval_parses_clamps_and_falls_back() {
        let c = cfg(&[("AUGMENTAGENT_HEARTBEAT_INTERVAL", "2h")]);
        assert_eq!(c.interval, Duration::from_secs(7200));
        let c = cfg(&[("AUGMENTAGENT_HEARTBEAT_INTERVAL", "1m")]);
        assert_eq!(c.interval, MIN_INTERVAL);
        assert_eq!(c.warnings.len(), 1);
        let c = cfg(&[("AUGMENTAGENT_HEARTBEAT_INTERVAL", "soon")]);
        assert_eq!(c.interval, DEFAULT_INTERVAL);
        assert_eq!(c.warnings.len(), 1);
    }

    #[test]
    fn bad_window_and_tz_fall_back_with_warnings() {
        let c = cfg(&[
            ("AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS", "8-10pm"),
            ("AUGMENTAGENT_HEARTBEAT_TZ", "Mars/Olympus"),
        ]);
        assert_eq!(c.active_hours, None);
        assert_eq!(c.tz, None);
        assert_eq!(c.warnings.len(), 2);
    }

    #[test]
    fn cap_and_timeout_parse() {
        let c = cfg(&[
            ("AUGMENTAGENT_HEARTBEAT_DAILY_CAP", "2"),
            ("AUGMENTAGENT_HEARTBEAT_TIMEOUT_SECS", "90"),
        ]);
        assert_eq!(c.daily_cap, 2);
        assert_eq!(c.timeout, Duration::from_secs(90));
        let c = cfg(&[("AUGMENTAGENT_HEARTBEAT_TIMEOUT_SECS", "0")]);
        assert_eq!(c.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn window_is_evaluated_in_the_configured_zone() {
        let c = cfg(&[
            ("AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS", "08:00-22:00"),
            ("AUGMENTAGENT_HEARTBEAT_TZ", "America/New_York"),
        ]);
        // 13:00Z = 09:00 EDT, inside; 03:00Z = 23:00 EDT the previous day, outside.
        assert!(c.is_active_at(Utc.with_ymd_and_hms(2026, 9, 29, 13, 0, 0).unwrap()));
        assert!(!c.is_active_at(Utc.with_ymd_and_hms(2026, 9, 29, 3, 0, 0).unwrap()));
        assert_eq!(
            c.local_time_label(Utc.with_ymd_and_hms(2026, 9, 29, 13, 5, 0).unwrap()),
            "2026-09-29 09:05 EDT (Tuesday)"
        );
    }

    #[test]
    fn staleness_needs_enabled_three_missed_intervals_and_an_open_window() {
        let now = Utc.with_ymd_and_hms(2026, 9, 29, 15, 0, 0).unwrap();
        let ms = |h: i64| now.timestamp_millis() - h * 60 * 60_000;
        let mut c = cfg(&[("AUGMENTAGENT_HEARTBEAT_TZ", "UTC")]);
        assert!(!c.is_stale(None, now), "disabled is never stale");

        c.enabled = true;
        assert!(c.is_stale(None, now), "enabled but never run");
        assert!(!c.is_stale(Some(ms(1)), now), "1h < 3 x 30m");
        assert!(c.is_stale(Some(ms(2)), now), "2h > 3 x 30m");

        // Window opened at 14:00; 3 intervals back (13:30) was still quiet.
        c.active_hours = Some(ActiveHours::parse("14:00-22:00").unwrap());
        assert!(!c.is_stale(Some(ms(12)), now));
        c.active_hours = Some(ActiveHours::parse("08:00-22:00").unwrap());
        assert!(c.is_stale(Some(ms(12)), now));
        c.active_hours = Some(ActiveHours::parse("09:00-09:00").unwrap());
        assert!(!c.is_stale(None, now), "never-active window never alarms");
    }

    #[test]
    fn no_window_is_always_active() {
        let c = cfg(&[]);
        assert!(c.is_active_at(Utc.with_ymd_and_hms(2026, 9, 29, 3, 0, 0).unwrap()));
    }
}
