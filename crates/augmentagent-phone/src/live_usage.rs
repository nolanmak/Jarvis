//! Final usage and transport outcomes are independent evidence. No transcript,
//! credential or raw provider error is stored in these operator receipts.
use crate::{
    live_config::LiveConfig,
    live_session::{CloseReason, LiveError, LiveInput},
    Error, PhoneService,
};
use augmentagent_store::phone::{LiveUsageFinal, LiveUsagePhase};

impl LiveError {
    pub fn code(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Authentication => "authentication",
            Self::Access => "access",
            Self::RateLimit => "rate_limit",
            Self::Unavailable => "unavailable",
            Self::Configuration => "configuration",
            Self::Protocol => "protocol",
            Self::Capacity => "capacity",
            Self::StartupTimeout => "startup_timeout",
            Self::CloseTimeout => "close_timeout",
            Self::ConnectionLost => "connection_lost",
        }
    }
}
impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "close_requested",
            Self::Expired => "expired",
            Self::Content => "content",
            Self::Remote => "remote_hangup",
            Self::ConnectionLost => "connection_lost",
        }
    }
}
impl PhoneService {
    pub(crate) fn save_live_usage(
        &self,
        call_id: &str,
        phase: LiveUsagePhase,
        trigger: &str,
        error: Option<LiveError>,
        finalization: Result<LiveInput, LiveError>,
    ) -> Result<(), Error> {
        let (reason, seconds, finalization_error) = match finalization {
            Ok(LiveInput::Closed { reason, seconds }) => (Some(reason.as_str()), seconds, None),
            Ok(_) => (None, None, Some(LiveError::Protocol.code())),
            Err(error) => (None, None, Some(error.code())),
        };
        self.store.phone_live_usage_finish(
            call_id,
            phase,
            &LiveUsageFinal {
                trigger,
                reason,
                seconds,
                error: error.map(LiveError::code),
                finalization_error,
            },
        )?;
        Ok(())
    }
    pub(crate) async fn live_preflight(
        &self,
        live: &LiveConfig,
        call_id: &str,
    ) -> Result<(), Error> {
        let phase = LiveUsagePhase::Preflight;
        self.store.phone_live_usage_start(call_id, phase)?;
        let connection = match live
            .connect("Connection readiness check. Remain silent and do not delegate any work.")
            .await
        {
            Ok(connection) => connection,
            Err(error) => {
                self.store.phone_live_usage_finish(
                    call_id,
                    phase,
                    &LiveUsageFinal {
                        trigger: "preflight",
                        reason: None,
                        seconds: None,
                        error: Some(error.code()),
                        finalization_error: None,
                    },
                )?;
                return Err(error.into());
            }
        };
        let session_id = connection
            .session_id()
            .ok_or(LiveError::Protocol)?
            .to_owned();
        if let Err(error) = self
            .store
            .phone_live_usage_bind(call_id, phase, &session_id)
        {
            let _ = connection.finish().await;
            return Err(error.into());
        }
        let finalization = connection.finish().await;
        let error = match &finalization {
            Ok(LiveInput::Closed {
                reason: CloseReason::Requested,
                seconds: Some(_),
            }) => None,
            Ok(_) => Some(LiveError::Protocol),
            Err(error) => Some(*error),
        };
        self.save_live_usage(call_id, phase, "preflight", error, finalization)?;
        match error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }
}

/// Operator aggregate only: no transcript, destination, call or provider IDs.
/// The estimate uses the published GPT-Live 1 rate checked 2026-10-26; it is
/// not an invoice and excludes Twilio and independently executed backend work.
pub fn render_usage(rows: &[augmentagent_store::phone::PhoneLiveUsage]) -> String {
    let mut output = String::from("Live voice usage (final provider reports; USD estimate at $0.05/min, checked 2026-10-26; excludes Twilio/backend)\n");
    for phase in ["preflight", "conversation"] {
        let attempts = rows.iter().filter(|row| row.phase == phase).count();
        let confirmed: f64 = rows
            .iter()
            .filter(|row| row.phase == phase)
            .filter_map(|row| row.seconds)
            .fold(0.0, |total, seconds| total + seconds);
        let unknown = rows
            .iter()
            .filter(|row| row.phase == phase && row.seconds.is_none())
            .count();
        output.push_str(&format!(
            "{phase}: attempts={attempts} confirmed_seconds={confirmed:.3} unknown={unknown}\n"
        ));
    }
    let estimate = rows.iter().filter_map(|row| row.seconds).fold(0.0, |total, seconds| total + seconds) * (0.05 / 60.);
    let total = if rows.iter().any(|row| row.seconds.is_none()) {
        "unknown".into()
    } else {
        format!("{estimate:.6}")
    };
    output.push_str(&format!(
        "confirmed_usage_estimate_usd={estimate:.6} total_estimated_usd={total}\n"
    ));
    output
}
