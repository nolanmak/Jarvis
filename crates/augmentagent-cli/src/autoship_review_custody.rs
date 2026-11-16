//! Writer custody is append-only even when a provider rotates conversations.
use super::*;
impl WorkflowStore {
    pub(crate) fn require_independent_native_session(
        &self,
        provider: &str,
        session: &str,
    ) -> Result<()> {
        ensure!(
            matches!(provider, "codex" | "claude") && !session.trim().is_empty(),
            "unknown native review provenance"
        );
        let writer:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_session_owner WHERE provider=?1 AND native_session=?2) OR EXISTS(SELECT 1 FROM workflow_builder_conversation WHERE provider=?1 AND native_session=?2)",params![provider,session],|r|r.get(0))?;
        ensure!(
            !writer,
            "native review reused a protected writer conversation or shared writer context"
        );
        Ok(())
    }
}
