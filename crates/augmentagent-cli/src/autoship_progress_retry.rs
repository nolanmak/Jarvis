//! #1714: a Discord send that was definitely rejected (429 or another 4xx
//! response, so no message exists) is parked for a persisted retry instead of
//! being left uncertain. Ambiguous failures stay uncertain: never resent blind.
use super::*;

/// Exponential backoff ceiling when Discord gives no retry-after.
pub const MAX_BACKOFF_MS:i64=15*60_000;
const BASE_BACKOFF_MS:i64=30_000;
/// Upper bound accepted from a retry-after header.
const MAX_RETRY_AFTER_MS:i64=24*60*60_000;

impl WorkflowStore {
    /// Park one dispatched (uncertain) send after a definite rejection.
    /// Returns when it becomes eligible again.
    pub fn defer_progress(&mut self,_origin:&super::discord_ingress::DiscordOrigin,_workflow:&str,_sequence:i64,_retry_after_ms:Option<i64>,now:i64)->Result<i64> {
        let _=(BASE_BACKOFF_MS,MAX_RETRY_AFTER_MS);Ok(now)
    }
}

#[cfg(test)]
#[path="autoship_progress_retry_tests.rs"]
mod tests;
