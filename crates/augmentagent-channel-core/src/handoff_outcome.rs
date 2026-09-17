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
//! Content ending (a content-class `TurnFailure`, which includes claude's
//! empty output) leaves completed operations and no uncertain ones in the
//! journal, `FallbackReasoner` records a verdict file next to the journal and
//! returns [`CompletedWithoutSummary`]. Every later dispatch that addresses
//! the same journal gets that verdict back and spawns nothing.
//!
//! What is deliberately not a verdict:
//! - A provider-side interruption (quota, timeout, outage). The turn did not
//!   finish, so the next provider resumes from the receipts under the #1021
//!   handoff contract, and the journal itself stops a completed operation
//!   from repeating.
//! - An unrecognised ending, such as a provider killed mid-turn. Nothing
//!   shows the request finished, so a retry resumes from the receipts.
//! - A journal with an uncertain (`started`) row. A verdict is permanent;
//!   the request stays on the journal's own reconciliation gate instead.
//!
//! **Scope.** "The same journal" means the same turn identity
//! (`ReasonerOpts::session_id`). Only callers with a stable per-turn id get
//! retry protection: channel queries through the Discord broker and `/loop`
//! runs. Callers without one (wiki ingest, digests, the auto-PR builder) get
//! a fresh journal per call, so a retry from them is a new request.
//!
//! This module only reads the journal. `handoff.rs` owns the layout and the
//! bridge owns the rows.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
}

/// What a content-level ending left in the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settlement {
    /// No journal, or no completed operation: nothing to protect.
    Nothing,
    /// Some operations are uncertain. No verdict; reconciliation decides.
    Uncertain { uncertain: usize },
    /// Recorded (or already recorded) and returned.
    Completed(CompletedWithoutSummary),
}

/// Same bound the journal writer and `handoff::resume_message` enforce.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Beside the journal, under the name the journal sweep recognises (#1068).
fn verdict_path(journal: &Path) -> PathBuf {
    journal.with_file_name(crate::handoff::VERDICT_FILE)
}

/// Read a private regular file owned by this user, without following a
/// symlink. `None` when it does not exist; an error when it is not trusted.
fn read_private(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file() && metadata.permissions().mode() & 0o077 == 0
        && metadata.uid() == unsafe { libc::geteuid() } && metadata.len() <= MAX_BYTES,
        "handoff state is not a private bounded file");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "handoff state is not a private bounded file");
    Ok(Some(bytes))
}

/// The verdict already recorded for this request, if any.
///
/// An untrusted verdict file (a link, not private, another owner) is an
/// error, and the dispatcher does not dispatch on an error. An empty or
/// damaged one reads as absent and is reported: only a crash or a bad disk
/// produces one (writes are atomic), and treating it as permanent would wedge
/// the request forever. The next content-level ending writes it again.
pub(crate) fn recorded(journal: &Path) -> anyhow::Result<Option<CompletedWithoutSummary>> {
    let path = verdict_path(journal);
    let Some(bytes) = read_private(&path)? else { return Ok(None) };
    let completed = serde_json::from_slice::<Value>(&bytes).ok()
        .filter(|value| value["version"] == 1)
        .and_then(|value| value["completed"].as_u64())
        .and_then(|count| usize::try_from(count).ok())
        .filter(|count| *count > 0);
    match completed {
        Some(completed) => Ok(Some(CompletedWithoutSummary { completed })),
        None => {
            tracing::warn!(path = %path.display(),
                "completed-without-summary verdict is empty or damaged; ignoring it until it is written again");
            Ok(None)
        }
    }
}

/// Call this after a Content ending. If the journal holds completed
/// operations and no uncertain ones, record the verdict and return it.
pub(crate) fn settle(journal: &Path) -> anyhow::Result<Settlement> {
    let Some(bytes) = read_private(journal)? else { return Ok(Settlement::Nothing) };
    let state: Value = serde_json::from_slice(&bytes)?;
    let operations = match (state["version"].as_u64(), state["operations"].as_array()) {
        (Some(1), Some(operations)) => operations,
        _ => anyhow::bail!("invalid handoff state"),
    };
    let with_status = |status: &str| operations.iter().filter(|row| row["status"] == status).count();
    let (completed, uncertain) = (with_status("completed"), with_status("started"));
    if uncertain > 0 {
        return Ok(Settlement::Uncertain { uncertain });
    }
    if completed == 0 {
        return Ok(Settlement::Nothing);
    }
    let verdict = CompletedWithoutSummary { completed };
    // The verdict still applies to this call if the file cannot be written.
    // Only the refusal of later retries depends on the file.
    if let Err(error) = persist(journal, &verdict) {
        tracing::warn!("could not record the completed-without-summary verdict ({error:#}); \
            a retry of this request would resume from its receipts instead");
    }
    Ok(Settlement::Completed(verdict))
}

/// Write the verdict atomically: a private temp file in the request directory
/// (named like the bridge's own atomic saves, so a leftover is recognised),
/// fsync, rename over the verdict, fsync the directory.
fn persist(journal: &Path, verdict: &CompletedWithoutSummary) -> anyhow::Result<()> {
    // First valid verdict wins. It already refuses every later dispatch.
    if recorded(journal)?.is_some() {
        return Ok(());
    }
    let path = verdict_path(journal);
    let directory = path.parent().ok_or_else(|| anyhow::anyhow!("verdict path has no directory"))?;
    let mut file = tempfile::Builder::new().prefix(".handoff-").tempfile_in(directory)?;
    file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(serde_json::json!({"version": 1, "completed": verdict.completed}).to_string().as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(&path)?;
    std::fs::File::open(directory)?.sync_all()?;
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
            {"tool": "mcp__fixture__delete", "arguments": {}, "status": "not_applied",
             "reconciliation": {"outcome": "not_applied", "evidence": "synthetic"}},
        ]));
        assert_eq!(recorded(&journal).unwrap(), None);
        let verdict = CompletedWithoutSummary { completed: 2 };
        assert_eq!(settle(&journal).unwrap(), Settlement::Completed(verdict.clone()));
        assert_eq!(recorded(&journal).unwrap(), Some(verdict));
        let mode = std::fs::metadata(verdict_path(&journal)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the verdict is private task state");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.')).collect();
        assert!(leftovers.is_empty(), "the atomic write leaves no temp file: {leftovers:?}");
    }

    /// #1069 review M1(d): a verdict is permanent, an uncertain row is not.
    /// While any row is `started`, nothing is recorded and the request stays
    /// on the journal's reconciliation gate.
    #[test]
    fn uncertain_rows_never_settle_into_a_verdict() {
        let dir = private_dir();
        let journal = dir.path().join("operations.json");
        write_journal(&journal, serde_json::json!([
            {"tool": "mcp__fixture__create", "arguments": {}, "status": "completed", "result": {"content": []}},
            {"tool": "mcp__fixture__update", "arguments": {}, "status": "started"},
        ]));
        assert_eq!(settle(&journal).unwrap(), Settlement::Uncertain { uncertain: 1 });
        assert_eq!(recorded(&journal).unwrap(), None);
    }

    #[test]
    fn nothing_to_protect_records_nothing() {
        let dir = private_dir();
        let journal = dir.path().join("operations.json");
        assert_eq!(settle(&journal).unwrap(), Settlement::Nothing, "no journal");
        write_journal(&journal, serde_json::json!([]));
        assert_eq!(settle(&journal).unwrap(), Settlement::Nothing, "no rows");
        assert_eq!(recorded(&journal).unwrap(), None);
    }

    /// #1069 review L2: a verdict left empty or damaged (a crash, a bad disk)
    /// must not wedge the request forever. It reads as absent, and the next
    /// content-level ending with completed rows writes it again.
    #[test]
    fn a_torn_verdict_reads_as_absent_and_is_repaired() {
        let dir = private_dir();
        let journal = dir.path().join("operations.json");
        write_journal(&journal, serde_json::json!([
            {"tool": "mcp__fixture__create", "arguments": {}, "status": "completed", "result": {"content": []}},
        ]));
        for damaged in [&b""[..], &b"{\"version\":1}"[..], &b"{not json"[..]] {
            let verdict = verdict_path(&journal);
            std::fs::write(&verdict, damaged).unwrap();
            std::fs::set_permissions(&verdict, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(recorded(&journal).unwrap(), None, "{damaged:?}");
            assert!(matches!(settle(&journal).unwrap(), Settlement::Completed(_)));
            assert!(recorded(&journal).unwrap().is_some(), "{damaged:?}: repaired");
            std::fs::remove_file(&verdict).unwrap();
        }
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
        std::os::unix::fs::symlink(dir.path().join("elsewhere.json"), &verdict).unwrap();
        assert!(recorded(&journal).is_err(), "a linked verdict must stop dispatch, not vanish");
        std::fs::remove_file(&verdict).unwrap();
        std::fs::write(&verdict, b"{\"version\":1,\"completed\":1}").unwrap();
        std::fs::set_permissions(&verdict, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(recorded(&journal).is_err(), "a non-private verdict must stop dispatch, not vanish");
    }
}
