//! `augmentagent autonomy explain` — run the gate (and the planner and the
//! candidate lookup) for one stored action, with no side effects: nothing
//! is inserted, sent, approved or changed on a calendar.

use augmentagent_store::Store;

use crate::gate::AutonomyGate;

/// Explain the gate's decision for `action_id`. `notices_deliverable`
/// stands in for the live broker's capability.
pub async fn explain(gate: &AutonomyGate, store: &Store, action_id: &str, notices_deliverable: bool) -> anyhow::Result<String> {
    let _ = (gate, store, action_id, notices_deliverable);
    todo!("C7: explain")
}
