//! Unknown execution context is not a fabricated funding intent or usage total.
use super::*;
use observation::Reason;

#[derive(Debug)]
pub(crate) struct ContextRefused {
    pub reason: Reason,
}
impl std::fmt::Display for ContextRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason.as_str())
    }
}
impl std::error::Error for ContextRefused {}
pub(crate) fn refusal(reason: Reason) -> anyhow::Error {
    ContextRefused { reason }.into()
}
impl Wait {
    pub(crate) fn reason(&self) -> Result<Reason> {
        match (&self.observation, self.context_reason) {
            (Some(observation), None) => observation
                .blockers
                .first()
                .map(|b| b.reason_code)
                .context("missing family budget blocker"),
            (None, Some(reason)) => Ok(reason),
            _ => bail!("family wait must contain exactly one evidence variant"),
        }
    }
    pub(super) fn validate_evidence(&self, workflow: &str) -> Result<()> {
        match (&self.observation, self.context_reason) {
            (Some(observation), None) => ensure!(
                self.schema_version == 1
                    && observation.schema_version == 1
                    && observation.workflow == workflow
                    && !observation.ready()
                    && observation
                        .blockers
                        .iter()
                        .all(|b| b.workflow == workflow && b.next == b.reason_code.next()),
                "family budget wait evidence is invalid"
            ),
            (None, Some(reason)) => ensure!(
                self.schema_version == 2
                    && matches!(
                        reason,
                        Reason::FamilyAuthorityUnknown
                            | Reason::FamilyExecutionScopeUnknown
                            | Reason::FamilyMembershipPending
                    ),
                "family context wait evidence is invalid"
            ),
            _ => bail!("family wait must contain exactly one evidence variant"),
        }
        Ok(())
    }
}
