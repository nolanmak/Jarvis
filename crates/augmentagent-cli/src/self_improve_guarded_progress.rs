//! S progress adapter over the original verifier's protected captures.
use super::*;

impl OriginalExecution<'_> {
    /// Called by the executor after its resource outcome and verified teardown,
    /// never by read-only capture inspection or at command admission.
    pub(in crate::self_improve) fn record_completed_progress(&self, verifier: &str) -> Result<()> {
        let Some(observation) = self.read_captured_observation(verifier)? else {
            return Ok(());
        };
        if observation.interrupted
            || !observation.output_complete
            || observation.exit_code.is_none()
        {
            return Ok(());
        }
        // Deliberately excludes attempt/session IDs, manifests and timestamps.
        // New actual command output or a changed recipe can establish progress;
        // replaying identical bytes under another nonce cannot.
        let semantic = amendment::digest(&serde_json::to_vec(&(
            "guarded-original-step-v1",
            &self.contract,
            &self.files,
            &self.environment,
            verifier,
            observation.exit_code,
            &observation.stdout,
            &observation.stderr,
        ))?);
        let attempt = self.command_sources.get(verifier).unwrap_or(&self.attempt);
        self.workflow
            .record_guarded_progress(&command_key(&self.plan, attempt, verifier), &semantic)
    }
}
