//! "Completed without summary": the guard that stops a finished write from
//! being dispatched again (#1040).
//!
//! A write or agentic call can do its work (wiki pages written, a message
//! sent) and still end content-level: no final assistant text, or a turn
//! that failed on its own content. That ending is untyped, so the chain does
//! not advance. The request is still unanswered, though, and a caller that
//! retries it would dispatch the whole call again and repeat its effects.
//!
//! The request's operation journal is the evidence. Its rows are written by
//! `scripts/codex-tool-bridge.py` and the primary provider's hooks. When a
//! content-level ending leaves completed operations in it, `FallbackReasoner`
//! records a verdict file next to the journal and returns
//! [`CompletedWithoutSummary`]. Every later dispatch that uses the same
//! journal (the same turn identity) gets that verdict back and spawns
//! nothing.
//!
//! Provider-side interruptions (quota, timeout, outage) are deliberately not
//! verdicts. The turn did not finish, so the next provider resumes from the
//! receipts under the #1021 handoff contract, and the journal itself stops a
//! completed operation from repeating. To run a finished request's work again
//! on purpose, send a new request: it gets its own journal.
//!
//! This module only reads the journal. `handoff.rs` owns the layout and the
//! bridge owns the rows.

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The typed outcome for a request whose work finished without a final
/// summary. Not a `ReasonerError`: nothing latches and nothing fails over.
/// Callers can downcast it to report "done, but no summary" instead of
/// treating the call as a failure to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "request completed {completed} operation(s) but ended without a final summary; \
     not dispatching it again (a new request is needed to repeat its work)"
)]
pub struct CompletedWithoutSummary {
    /// Operations with a completed receipt.
    pub completed: usize,
    /// Operations started without a recorded outcome (need reconciliation).
    pub uncertain: usize,
}

/// Same bound the journal writer and `handoff::resume_message` enforce.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

fn verdict_path(journal: &Path) -> PathBuf {
    journal.with_extension("completed-without-summary")
}

/// Read a private regular file without following a symlink. `None` when it
/// does not exist.
fn read_private(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let mut file = match std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file() && metadata.permissions().mode() & 0o077 == 0
        && metadata.len() <= MAX_BYTES, "handoff state is not a private bounded file");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// The verdict already recorded for this request, if any. An unreadable
/// verdict is an error, and the dispatcher does not dispatch on an error.
pub(crate) fn recorded(journal: &Path) -> anyhow::Result<Option<CompletedWithoutSummary>> {
    let Some(bytes) = read_private(&verdict_path(journal))? else { return Ok(None) };
    let value: Value = serde_json::from_slice(&bytes)?;
    let count = |key: &str| value[key].as_u64().and_then(|n| usize::try_from(n).ok());
    match (value["version"].as_u64(), count("completed"), count("uncertain")) {
        (Some(1), Some(completed), Some(uncertain)) => Ok(Some(CompletedWithoutSummary { completed, uncertain })),
        _ => anyhow::bail!("invalid completed-without-summary verdict"),
    }
}

/// Call this after a content-level ending. If the journal holds completed
/// operations, record the verdict and return it. `Ok(None)` means the
/// request has no finished work to protect (no journal, or no completed
/// row), and the ending stays the provider's own error.
pub(crate) fn settle(journal: &Path) -> anyhow::Result<Option<CompletedWithoutSummary>> {
    let Some(bytes) = read_private(journal)? else { return Ok(None) };
    let state: Value = serde_json::from_slice(&bytes)?;
    let operations = match (state["version"].as_u64(), state["operations"].as_array()) {
        (Some(1), Some(operations)) => operations,
        _ => anyhow::bail!("invalid handoff state"),
    };
    let with_status = |status: &str| operations.iter().filter(|row| row["status"] == status).count();
    let verdict = CompletedWithoutSummary { completed: with_status("completed"), uncertain: with_status("started") };
    if verdict.completed == 0 {
        return Ok(None);
    }
    // The verdict still applies to this call if the file cannot be written.
    // Only the refusal of later retries depends on the file.
    if let Err(error) = persist(journal, &verdict) {
        tracing::warn!("could not record the completed-without-summary verdict ({error:#}); \
            a retry of this request would resume from its receipts instead");
    }
    Ok(Some(verdict))
}

fn persist(journal: &Path, verdict: &CompletedWithoutSummary) -> anyhow::Result<()> {
    let path = verdict_path(journal);
    let body = serde_json::json!({"version": 1, "completed": verdict.completed, "uncertain": verdict.uncertain});
    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
        Ok(file) => file,
        // First verdict wins. It already refuses every later dispatch.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    file.write_all(body.to_string().as_bytes())?;
    file.sync_all()?;
    if let Some(directory) = path.parent() {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn write_journal(path: &Path, operations: Value) {
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true)
            .mode(0o600).open(path).unwrap();
        file.write_all(serde_json::json!({"version": 1, "operations": operations}).to_string().as_bytes()).unwrap();
    }

    #[test]
    fn completed_rows_settle_into_a_durable_verdict() {
        let dir = private_dir();
        let journal = dir.path().join("operations.json");
        write_journal(&journal, serde_json::json!([
            {"tool": "mcp__fixture__create", "arguments": {}, "status": "completed", "result": {"content": []}},
            {"tool": "Write", "arguments": {}, "status": "completed", "result": {"content": []}},
            {"tool": "mcp__fixture__update", "arguments": {}, "status": "started"},
            {"tool": "mcp__fixture__delete", "arguments": {}, "status": "not_applied",
             "reconciliation": {"outcome": "not_applied", "evidence": "synthetic"}},
        ]));
        assert_eq!(recorded(&journal).unwrap(), None);
        let verdict = CompletedWithoutSummary { completed: 2, uncertain: 1 };
        assert_eq!(settle(&journal).unwrap(), Some(verdict.clone()));
        assert_eq!(recorded(&journal).unwrap(), Some(verdict));
        let mode = std::fs::metadata(verdict_path(&journal)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the verdict is private task state");
    }

    #[test]
    fn nothing_to_protect_records_nothing() {
        let dir = private_dir();
        let journal = dir.path().join("operations.json");
        assert_eq!(settle(&journal).unwrap(), None, "no journal");
        write_journal(&journal, serde_json::json!([{"tool": "mcp__fixture__update", "arguments": {}, "status": "started"}]));
        assert_eq!(settle(&journal).unwrap(), None, "only uncertain rows");
        assert_eq!(recorded(&journal).unwrap(), None);
    }

    #[test]
    fn untrusted_state_is_an_error_not_an_absence() {
        let dir = private_dir();
        let journal = dir.path().join("operations.json");
        std::fs::write(dir.path().join("elsewhere.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere.json"), &journal).unwrap();
        assert!(settle(&journal).is_err(), "a symlinked journal is not evidence");
        std::fs::remove_file(&journal).unwrap();
        write_journal(&journal, serde_json::json!({"not": "a list"}));
        assert!(settle(&journal).is_err());
        let verdict = verdict_path(&journal);
        std::fs::write(&verdict, b"{\"version\":1}").unwrap();
        std::fs::set_permissions(&verdict, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(recorded(&journal).is_err(), "a damaged verdict must still stop dispatch");
    }
}
