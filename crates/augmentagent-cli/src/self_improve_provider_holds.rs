//! Persist native fallback facts without granting dispatch or changing cooldowns.
use super::*;

impl WorkflowGuard {
    pub(in crate::self_improve) async fn hold_provider(&mut self, lane: &augmentagent_channel_core::LaneAvailability) -> Result<()> {
        use augmentagent_channel_core::LaneAvailability;
        let now = millis();
        let observation = now.saturating_add(300_000);
        let (reason, action, eligible) = match lane {
            LaneAvailability::AllLatched(latched) => {
                // Each timestamp belongs to this lane's authoritative latch.
                // An unknown reset gets an unpaid observation, not a guessed
                // quota reset inferred from another provider or preset.
                let eligible = latched.iter().map(|(_, reset)| reset.as_ref()
                    .map(|at| at.timestamp_millis()).filter(|at| *at > now)
                    .unwrap_or(observation)).min().unwrap_or(observation);
                ("provider_quota_wait", "await_quota", eligible)
            }
            LaneAvailability::NoEligibleProvider =>
                ("provider_configuration_unavailable", "await_provider_configuration", observation),
            LaneAvailability::Available => anyhow::bail!("available provider is not a hold"),
        };
        let stage = self.store.lock().map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .snapshot(self.id())?.context("provider hold workflow missing")?.checkpoint.stage;
        // transition preserves evidence and enforces current owner/lease health.
        self.transition(stage, reason, action, eligible).await
    }
}
