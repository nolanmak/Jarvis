//! Unpaid worker observations before a workflow can exist. This does not grant
//! admission or retry paid work, and does not persist potentially private errors.
use super::*;

const OBSERVATION_MS: i64 = 300_000;
const REASON: &str = "execution-readiness-blocked";

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Observation {
    schema_version: u32,
    reason_code: String,
    next_observation_at_ms: i64,
}

pub(super) fn is_blocked(error: &anyhow::Error) -> bool {
    error.downcast_ref::<targeted::CommandFailure>()
        .is_some_and(|failure| failure.code == 3 && failure.reason == REASON)
}

/// Existing locked settlement transaction, including shutdown after a result.
/// Non-preflight results clear the schedule; unrelated errors keep their cadence
/// and accounting. Recovery cannot retain a stale readiness observation.
pub(super) fn record(counter: &mut DailyCounter, error: Option<&anyhow::Error>, now: i64) {
    counter.preflight_observation = error.filter(|error| is_blocked(error)).map(|_| Observation {
        schema_version: 1,
        reason_code: REASON.into(),
        next_observation_at_ms: now.saturating_add(OBSERVATION_MS),
    });
}

pub(super) fn delay(path: &Path, now: i64, ordinary: std::time::Duration) -> Result<std::time::Duration> {
    let counter = DailyCounter::load_checked(path)?;
    let Some(observation) = counter.preflight_observation.filter(|value|
        value.schema_version == 1 && value.reason_code == REASON) else { return Ok(ordinary); };
    // A backwards wall-clock adjustment must not postpone readiness forever.
    let remaining = observation.next_observation_at_ms.saturating_sub(now).clamp(0, OBSERVATION_MS);
    Ok(ordinary.min(std::time::Duration::from_millis(remaining as u64)))
}
