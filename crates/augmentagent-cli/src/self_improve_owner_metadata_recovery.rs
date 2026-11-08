//! Original protected publication and reserved-readiness orchestration.
//! These metadata transitions never grant coding or input-release authority.
use super::*;

/// Reopen the exact original private acceptance only inside its current native
/// metadata proof. No prepared object or receipt is supplied by the caller.
/// A validated duplicate returns false and cannot manufacture fresh progress.
pub(super) fn recover_owner_publication(
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
) -> Result<bool> {
    let mut writer = WorkflowStore::open(state)?;
    let mut appended = false;
    with_recovery_native_metadata(
        state,
        Some(worktree),
        recovery.clone(),
        authorization,
        &mut |proof| {
            let recovered = writer.with_owner_recovery_preparation(
                &recovery,
                authorization,
                proof,
                millis(),
                |access| {
                    acceptance::owner::publication::reopen_recovery_preparation(
                        access, proof, worktree,
                    )
                },
            )?;
            recovered.with_scope_transition(|transition| {
                appended = writer.complete_recovered_owner_publication_progress(
                    &recovery,
                    authorization,
                    &recovered,
                    transition,
                    millis(),
                )?;
                Ok(())
            })
        },
    )?;
    Ok(appended)
}

/// Settle only a reserved original body after its actual writer-revocation and
/// nonrelease proofs. The complete SQL snapshot is captured before native locks
/// and revalidated by the consuming transaction. Missing/uncertain history is
/// an error, never an unbound-body fallback or permission to repeat the body.
/// The readiness token stays inside the protected visitor's post-commit check.
pub(super) fn settle_reserved_owner_readiness(
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
) -> Result<()> {
    let mut writer = WorkflowStore::open(state)?;
    let facts =
        writer.owner_publication_recovery_native_facts(&recovery, authorization, millis())?;
    let snapshot = writer
        .read_reserved_owner_body(&facts.namespace().owner.workflow_id, recovery.request_id())?;
    with_recovery_native_metadata_outcome(
        state,
        Some(worktree),
        recovery.clone(),
        authorization,
        &mut |proof| {
            let recovered = writer.with_owner_recovery_preparation(
                &recovery,
                authorization,
                proof,
                millis(),
                |access| {
                    acceptance::owner::publication::reopen_recovery_preparation(
                        access, proof, worktree,
                    )
                },
            )?;
            recovered.with_scope_transition(|transition| {
                let scope = transition.register()?;
                proof.with_revoked_input_writers(|closed| {
                    closed.with_body_input_nonrelease(
                        recovery.request_id(),
                        snapshot.expected_attempts(),
                        |body| {
                            let settled = writer.prepare_reserved_owner_successor_ready(
                                &recovery,
                                authorization,
                                &recovered,
                                body,
                                &snapshot,
                                &scope,
                                millis(),
                            )?;
                            Ok(RecoveryMetadataOutcome::SuccessorReady(settled))
                        },
                    )
                })
            })
        },
    )
}
