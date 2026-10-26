//! Provider-reported final usage, kept separate from transport termination.
use super::*;
#[derive(Clone, Copy)]
pub enum LiveUsagePhase {
    Preflight,
    Conversation,
}
impl LiveUsagePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Conversation => "conversation",
        }
    }
}
pub struct LiveUsageFinal<'a> {
    pub trigger: &'a str,
    pub reason: Option<&'a str>,
    pub seconds: Option<f64>,
    pub error: Option<&'a str>,
    pub finalization_error: Option<&'a str>,
}
// No Debug or Serialize: correlation IDs are for private operator inspection.
pub struct PhoneLiveUsage {
    pub call_id: String,
    pub phase: String,
    pub session_id: Option<String>,
    pub trigger: Option<String>,
    pub reason: Option<String>,
    pub seconds: Option<f64>,
    pub error: Option<String>,
    pub finalization_error: Option<String>,
}
fn usage_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PhoneLiveUsage> {
    Ok(PhoneLiveUsage {
        call_id: r.get(0)?,
        phase: r.get(1)?,
        session_id: r.get(2)?,
        trigger: r.get(3)?,
        reason: r.get(4)?,
        seconds: r.get(5)?,
        error: r.get(6)?,
        finalization_error: r.get(7)?,
    })
}
const FIELDS: &str =
    "call_id,phase,session_id,end_trigger,final_reason,seconds,error,finalization_error";
const ERRORS: &[&str] = &[
    "state",
    "authentication",
    "access",
    "rate_limit",
    "unavailable",
    "configuration",
    "protocol",
    "capacity",
    "startup_timeout",
    "close_timeout",
    "connection_lost",
];
impl Store {
    /// Persist intent before contacting OpenAI. A pending row after a crash has
    /// unknown usage, never zero; a second start cannot reset the receipt.
    pub fn phone_live_usage_start(&self, call_id: &str, phase: LiveUsagePhase) -> PhoneResult<()> {
        transaction(self, |tx| {
            call_by_id(tx, call_id)?;
            tx.execute(
                "INSERT INTO phone_live_usage(call_id,phase) VALUES(?1,?2)",
                params![call_id, phase.as_str()],
            )?;
            Ok(())
        })
    }
    pub fn phone_live_usage_bind(
        &self,
        call_id: &str,
        phase: LiveUsagePhase,
        session_id: &str,
    ) -> PhoneResult<()> {
        identifier(session_id)?;
        if session_id.len() > 256 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let changed=tx.execute("UPDATE phone_live_usage SET session_id=?3 WHERE call_id=?1 AND phase=?2 AND end_trigger IS NULL AND (session_id IS NULL OR session_id=?3)",params![call_id,phase.as_str(),session_id])?;
            if changed != 1 {
                return Err(PhoneError::Conflict);
            }
            Ok(())
        })
    }
    pub fn phone_live_usage_finish(
        &self,
        call_id: &str,
        phase: LiveUsagePhase,
        receipt: &LiveUsageFinal<'_>,
    ) -> PhoneResult<()> {
        if ![
            "preflight",
            "setup_failure",
            "app_stop",
            "duration_limit",
            "twilio_stop",
            "socket_close",
            "provider_closed",
            "error",
            "owner_goodbye",
        ]
        .contains(&receipt.trigger)
            || receipt.reason.is_some_and(|r| {
                ![
                    "close_requested",
                    "expired",
                    "content",
                    "remote_hangup",
                    "connection_lost",
                ]
                .contains(&r)
            })
            || receipt.seconds.is_some_and(|s| !s.is_finite() || s < 0.)
            || (receipt.seconds.is_some() && receipt.reason.is_none())
            || receipt.error.is_some_and(|s| !ERRORS.contains(&s))
            || receipt
                .finalization_error
                .is_some_and(|s| !ERRORS.contains(&s))
            || (receipt.reason.is_some() && receipt.finalization_error.is_some())
        {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let row = tx
                .query_row(
                    &format!("SELECT {FIELDS} FROM phone_live_usage WHERE call_id=?1 AND phase=?2"),
                    params![call_id, phase.as_str()],
                    usage_row,
                )
                .optional()?
                .ok_or(PhoneError::Conflict)?;
            if receipt.reason.is_some() && row.session_id.is_none() {
                return Err(PhoneError::Conflict);
            }
            if let Some(trigger) = row.trigger {
                return if trigger == receipt.trigger
                    && row.reason.as_deref() == receipt.reason
                    && row.seconds == receipt.seconds
                    && row.error.as_deref() == receipt.error
                    && row.finalization_error.as_deref() == receipt.finalization_error
                {
                    Ok(())
                } else {
                    Err(PhoneError::Conflict)
                };
            }
            tx.execute("UPDATE phone_live_usage SET end_trigger=?3,final_reason=?4,seconds=?5,error=?6,finalization_error=?7 WHERE call_id=?1 AND phase=?2",params![call_id,phase.as_str(),receipt.trigger,receipt.reason,receipt.seconds,receipt.error,receipt.finalization_error])?;
            Ok(())
        })
    }
    pub fn phone_live_usage(&self, last_calls: usize) -> PhoneResult<Vec<PhoneLiveUsage>> {
        if !(1..=1000).contains(&last_calls) {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let mut statement=tx.prepare(&format!("SELECT {FIELDS} FROM phone_live_usage WHERE call_id IN (SELECT id FROM phone_calls ORDER BY created_ms DESC,id DESC LIMIT ?1) ORDER BY call_id,phase"))?;
            let rows = statement
                .query_map([last_calls as i64], usage_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}
