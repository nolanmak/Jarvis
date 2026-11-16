//! Native provider-hold checkpoint boundary. Extracted existing scope behavior.
use super::*;
impl WorkflowGuard {
    pub(in crate::self_improve) async fn hold_provider(&mut self, lane: &augmentagent_channel_core::LaneAvailability) -> Result<()> {
        anyhow::ensure!(!matches!(lane, augmentagent_channel_core::LaneAvailability::Available), "available provider is not a hold");
        self.transition(Stage::Scope, "scope_deferred", "retry_scope", millis()+300_000).await
    }
}
