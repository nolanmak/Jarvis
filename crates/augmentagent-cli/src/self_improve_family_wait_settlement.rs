//! Post-pipeline settlement of durable family waits; reporting grants no dispatch.
use super::*;

impl CapacityAccounting {
    pub(in crate::self_improve) async fn settle_family_wait(&self) -> Result<Option<RunReport>> {
        family_budget_hold::report(self)
    }
}
