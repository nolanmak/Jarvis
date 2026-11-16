//! S-owned activity from completed tools in a protected native model call.
use super::*;

#[derive(Serialize, Deserialize)]
struct NativeToolCompleted {
    version: u32,
    binding: Binding,
    provider: String,
    semantic_sha256: String,
}
fn semantic_key(binding: &Binding, digest: &str) -> Result<String> {
    ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid native tool artifact digest"
    );
    Ok(serde_json::to_string(&(binding, digest))?)
}
pub(super) fn event_key(payload: &str) -> Result<String> {
    let completed: NativeToolCompleted = serde_json::from_str(payload)?;
    ensure!(
        completed.version == 1 && matches!(completed.provider.as_str(), "codex" | "claude"),
        "invalid native tool completion"
    );
    completed.binding.validate()?;
    semantic_key(&completed.binding, &completed.semantic_sha256)
}

impl WorkflowStore {
    /// The protected audit observer supplies this digest after a native SDK
    /// tool result was durably recorded. It is not a model-text API.
    pub(crate) fn record_native_tool_progress(
        &mut self,
        lease: &Lease,
        provider: &str,
        semantic_sha256: &str,
        now: i64,
    ) -> Result<bool> {
        ensure!(
            matches!(provider, "codex" | "claude"),
            "unsupported native progress provider"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let admitted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_model_admitted' AND json_extract(payload,'$.provider')=?3)",params![lease.workflow,lease.generation,provider],|r|r.get(0))?;
        ensure!(admitted, "native tool has no matching model admission");
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        let key = semantic_key(&checkpoint.binding, semantic_sha256)?;
        let seen:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_activity_progress WHERE workflow=?1 AND kind='native_tool_completed' AND semantic_key=?2)",params![lease.workflow,key],|r|r.get(0))?;
        if seen {
            return Ok(false);
        }
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "native_tool_completed",
            &serde_json::to_string(&NativeToolCompleted {
                version: 1,
                binding: checkpoint.binding,
                provider: provider.into(),
                semantic_sha256: semantic_sha256.into(),
            })?,
            now,
        )?;
        tx.commit()?;
        Ok(true)
    }
}
