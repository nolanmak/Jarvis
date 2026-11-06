//! D1 — the master switch, read once at startup.
//!
//! Only `on` (case-insensitive, trimmed) enables. Anything else — unset,
//! empty, `off`, `true`, a typo — is off; a set-but-unrecognised value logs a
//! warning so a typo doesn't silently look like a decision.

/// `AUGMENTAGENT_AUTONOMY`.
pub const AUTONOMY_ENV: &str = "AUGMENTAGENT_AUTONOMY";
/// `AUGMENTAGENT_HEARTBEAT_OUTREACH` (D16; also needs the master switch).
pub const HEARTBEAT_OUTREACH_ENV: &str = "AUGMENTAGENT_HEARTBEAT_OUTREACH";

/// How a switch value was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchValue {
    On,
    /// Unset, empty or `off`.
    Off,
    /// Set to something other than `on`/`off`: treated as off, warned about.
    Unrecognised,
}

impl SwitchValue {
    pub fn enabled(self) -> bool {
        self == SwitchValue::On
    }
}

pub fn parse_switch(raw: Option<&str>) -> SwitchValue {
    match raw.map(str::trim) {
        None | Some("") => SwitchValue::Off,
        Some(v) if v.eq_ignore_ascii_case("on") => SwitchValue::On,
        Some(v) if v.eq_ignore_ascii_case("off") => SwitchValue::Off,
        Some(_) => SwitchValue::Unrecognised,
    }
}

fn switch_from_env(key: &str) -> bool {
    let raw = std::env::var(key).ok();
    let value = parse_switch(raw.as_deref());
    if value == SwitchValue::Unrecognised {
        tracing::warn!(
            key,
            value = raw.as_deref().unwrap_or_default(),
            "unrecognised switch value; only `on` enables it, so it stays off"
        );
    }
    value.enabled()
}

/// D1 master switch. Call once at startup.
pub fn master_switch_from_env() -> bool {
    switch_from_env(AUTONOMY_ENV)
}

/// D16 heartbeat outreach: on only when its own switch *and* the master
/// switch are `on`.
pub fn heartbeat_outreach_from_env() -> bool {
    switch_from_env(HEARTBEAT_OUTREACH_ENV) && master_switch_from_env()
}
