//! Explicit local-operator recovery. Never exposed through model tools and never
//! clears uncertain provider creates, resets retry counters, dials, or requests hangup.
//! Unfinished Live preflights can be sealed and retired before any provider create.
use crate::{Clock, Error, FinalUsage, ProviderError, Telephony};
use augmentagent_store::{
    phone::{CallState, ProviderUsage},
    Store,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    Finalized,
    AlreadyFinalized,
    UnknownProviderId,
    UnverifiedProviderId,
    ProviderNotFinal,
    ContradictoryEvidence,
}

pub async fn reconcile(
    store: &Store,
    provider: &dyn Telephony,
    clock: &dyn Clock,
    call_id: &str,
) -> Result<RecoveryOutcome, Error> {
    let call = store.phone_provider_call(call_id)?;
    if store.phone_usage(call_id)?.is_some() || (call.state.terminal() && call.provider_sid.is_none()) {
        return Ok(RecoveryOutcome::AlreadyFinalized);
    }
    let Some(sid) = call.provider_sid else {
        if store.phone_recover_abandoned_preflight(call_id, clock.now_ms())? {
            return Ok(RecoveryOutcome::Finalized);
        }
        return Ok(RecoveryOutcome::UnknownProviderId);
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.final_usage(&sid),
    )
    .await
    .map_err(|_| ProviderError::Uncertain)??;
    // A signed callback may have supplied usage or answer evidence during I/O.
    let call = store.phone_provider_call(call_id)?;
    if store.phone_usage(call_id)?.is_some() {
        return Ok(RecoveryOutcome::AlreadyFinalized);
    }
    let now = clock.now_ms();
    let (terminal, usage) = match result {
        None => return Ok(RecoveryOutcome::ProviderNotFinal),
        Some(FinalUsage::NeverAnswered) if call.answered_ms.is_some() => {
            return Ok(RecoveryOutcome::ContradictoryEvidence)
        }
        Some(FinalUsage::NeverAnswered) => (
            CallState::Canceled,
            ProviderUsage {
                duration_seconds: 0,
                ended_ms: call.ended_ms.unwrap_or(now),
            },
        ),
        Some(FinalUsage::Completed(usage)) => (CallState::Completed, usage),
    };
    let terminal = if call.state.terminal() {
        call.state
    } else {
        terminal
    };
    let applied = if matches!(result, Some(FinalUsage::NeverAnswered)) {
        store.phone_confirm_never_answered(call_id, &sid, now)
    } else {
        store.phone_provider_event_with_usage(call_id, &sid, terminal, now, Some(usage))
    };
    match applied {
        Ok(()) => Ok(RecoveryOutcome::Finalized),
        // Immutable final usage from a concurrent signed callback wins.
        Err(_) if store.phone_usage(call_id)?.is_some() => Ok(RecoveryOutcome::AlreadyFinalized),
        Err(error) => Err(error.into()),
    }
}

/// Explicit operator-selected candidate. A terminal provider result and exact
/// callback-origin evidence are required before persisting its ID. Never dial.
pub async fn associate_and_reconcile(
    store: &Store,
    provider: &dyn Telephony,
    clock: &dyn Clock,
    call_id: &str,
    candidate: &str,
    public_base: &str,
) -> Result<RecoveryOutcome, Error> {
    let call = store.phone_provider_call(call_id)?;
    if let Some(existing) = call.provider_sid.as_deref() {
        if existing != candidate {
            return Err(Error::InvalidArguments);
        }
        return reconcile(store, provider, clock, call_id).await;
    }
    let final_usage = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.final_usage(candidate),
    )
    .await
    .map_err(|_| ProviderError::Uncertain)??;
    if final_usage.is_none() {
        return Ok(RecoveryOutcome::ProviderNotFinal);
    }
    if matches!(final_usage, Some(FinalUsage::NeverAnswered)) && call.answered_ms.is_some() {
        return Ok(RecoveryOutcome::ContradictoryEvidence);
    }
    let confirmed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.confirms_origin(candidate, call_id, public_base),
    )
    .await
    .map_err(|_| ProviderError::Uncertain)??;
    if !confirmed {
        return Ok(RecoveryOutcome::UnverifiedProviderId);
    }
    // The store atomically refuses replacement of a SID supplied by a racing
    // callback or reuse of another local call's provider SID.
    store.phone_bind_provider(call_id, candidate)?;
    reconcile(store, provider, clock, call_id).await
}
