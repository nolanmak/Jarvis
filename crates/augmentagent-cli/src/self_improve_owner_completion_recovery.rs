//! Reconstruct original launches and project lost completion callbacks under an
//! existing authenticated metadata claim. No scheduler/model/body authority.
use super::*;

pub(super) struct CompletionProjection {pub(super) observed:usize,pub(super) appended:usize}

/// Inspect protected original completions without first spending a recovery
/// claim. Only an actually changed outcome may atomically import and claim.
/// This supports the original active namespace; historical closed sources keep
/// their separate proof requirement and cannot be relabelled as active.
pub(super) fn recover_blocked_native_completions(state:&Path,worktree:Option<&Path>,
    authorization:&super::super::targeted::AuthorizedOwnerRecovery<'_>,
)->Result<Option<Arc<durable::owner_publication_recovery::RecoveryLease>>> {
    let shared=Arc::new(Mutex::new(WorkflowStore::open(state)?));
    let Some(observation)=shared.lock().map_err(|_|anyhow::anyhow!("completion observation store poisoned"))?
        .begin_owner_completion_observation(authorization,millis())? else {return Ok(None)};
    let facts=shared.lock().map_err(|_|anyhow::anyhow!("completion observation store poisoned"))?
        .owner_completion_observation_native_facts(&observation,authorization,millis())?;
    let launches=facts.completion_launches();
    anyhow::ensure!(launches.len()<=128,"completion observation census exceeds bound");
    for launch in &launches {
        anyhow::ensure!(launch.source_owner==facts.namespace().owner
            && launch.source_audit_path==facts.namespace().audit_path,
            "completion observation requires original active launch namespace");
    }
    let audit=existing_native_audit(state,facts.namespace(),worktree,facts.original_binding())?;
    audit.restrict_to_metadata(durable::owner_publication_recovery::completion_observation_fence(
        shared.clone(),observation.clone(),authorization,millis())?)?;
    let mut writer=WorkflowStore::open(state)?;
    let current=||shared.lock().map_err(|_|anyhow::anyhow!("completion observation store poisoned"))?
        .validate_owner_completion_observation(&observation,authorization,millis());
    for launch in &launches {
        current()?;
        let receipt=launch.intent.controller_receipt.as_ref().context("completion observation launch receipt missing")?;
        let mut row=audit.read_append_receipt(receipt)?;
        anyhow::ensure!(row["actor"]=="controller" && row["kind"]=="native_launch_intent","completion observation needs protected controller launch");
        let object=row.as_object_mut().context("completion observation launch is not an object")?;
        for name in ["audit_chain","actor","kind"] {object.remove(name);}
        let prepared=augmentagent_channel_core::native_vm::PreparedLaunchRecord {
            intent:serde_json::from_value(row)?,controller_receipt:receipt.clone(),
        };
        launch.require_prepared_identity(&prepared)?;
        let mut claimed=None;
        augmentagent_channel_core::native_vm::ownership::with_discovered_metadata_native_completion(
            &audit,&prepared,&current,&mut |proof| {
                claimed=writer.claim_with_owner_native_completion(&observation,authorization,proof,
                    "autoship-owner-completion-observer",millis(),300_000)?;
                Ok(())
            })?;
        if claimed.is_some() {return Ok(claimed);}
    }
    current()?;
    Ok(None)
}

/// Each imported completion commits atomically. A later error can leave earlier
/// metadata observations committed, so callers must re-read history on retry.
/// Counts describe observations/writes only; zero is never absence/ready proof.
pub(super) fn recover_known_native_completions(state:&Path,worktree:Option<&Path>,
    recovery:Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization:&super::super::targeted::AuthorizedOwnerRecovery<'_>,
)->Result<CompletionProjection> {
    let shared=Arc::new(Mutex::new(WorkflowStore::open(state)?));
    let read_facts=||shared.lock().map_err(|_|anyhow::anyhow!("completion recovery store poisoned"))?
        .owner_publication_recovery_native_facts(&recovery,authorization,millis());
    let original=read_facts()?;let launches=original.completion_launches();
    anyhow::ensure!(launches.len()<=128,"completion recovery runtime census exceeds bound");
    for launch in &launches {
        anyhow::ensure!(launch.source_owner==original.namespace().owner
            && launch.source_audit_path==original.namespace().audit_path,
            "completion recovery requires the original active launch namespace");
    }
    let audit=existing_native_audit(state,original.namespace(),worktree,original.original_binding())?;
    audit.restrict_to_metadata(durable::owner_publication_recovery::native_metadata_fence(shared.clone(),recovery.clone(),authorization,millis())?)?;
    // Open before native locks; the metadata fence uses a different connection
    // and its mutex remains free during the SQL append visitor.
    let mut writer=WorkflowStore::open(state)?;
    let mut result=CompletionProjection {observed:0,appended:0};
    let unchanged=|fresh:&durable::OwnerPublicationNativeFacts|->Result<()> {
        anyhow::ensure!(fresh.namespace()==original.namespace() && fresh.original_binding()==original.original_binding()
            && fresh.completion_launches()==launches,"completion recovery namespace or launch census changed");Ok(())
    };
    for launch in &launches {
        let basis=read_facts()?;unchanged(&basis)?;let fingerprint=basis.fingerprint()?;
        let current=||->Result<()> {let fresh=read_facts()?;unchanged(&fresh)?;
            anyhow::ensure!(fresh.fingerprint()?==fingerprint,"completion recovery original inputs changed");Ok(())};
        current()?;
        let receipt=launch.intent.controller_receipt.as_ref().context("completion recovery original launch receipt missing")?;
        let mut row=audit.read_append_receipt(receipt)?;
        anyhow::ensure!(row["actor"]=="controller" && row["kind"]=="native_launch_intent","completion recovery requires a protected controller launch");
        let object=row.as_object_mut().context("completion recovery launch is not an object")?;
        for name in ["audit_chain","actor","kind"] {object.remove(name);}
        let prepared=augmentagent_channel_core::native_vm::PreparedLaunchRecord {
            intent:serde_json::from_value(row)?,controller_receipt:receipt.clone(),
        };
        launch.require_prepared_identity(&prepared)?;
        // The fresh native proof and SQL writer independently join this parsed
        // descriptor back to original binding, registration, launch and claim.
        let observed=augmentagent_channel_core::native_vm::ownership::with_discovered_metadata_native_completion(
            &audit,&prepared,&current,&mut |proof| {
                let (_,appended)=writer.record_owner_recovery_native_completion_progress(&recovery,authorization,proof,millis())?;
                result.appended+=usize::from(appended);Ok(())
            })?;
        result.observed+=usize::from(observed);
    }
    unchanged(&read_facts()?)?;
    Ok(result)
}
