//! Opt-in real build-VM smoke. No provider calls, publication or merge authority.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_internal_links() -> Result<()> {
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()?==evidence,"evidence path must be literal");
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-link-smoke",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm",Some(&audit))?;
    let before=binding(&root).await?;let target=crate::build_storage::effective_target(&root);
    // This opt-in test runs from the already-built binary after cargo-managed
    // exits; it obtains its own managed CI lease rather than nesting admission.
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let session=vm::Session::prepare(&root,&target)?;
    let source=snapshot(&root,&before,&target).await?;
    let fixture=source.path().join("autoship-internal-link-fixture");std::fs::create_dir(&fixture)?;
    std::fs::write(fixture.join("addon.node"),"owned build artifact")?;
    std::fs::hard_link(fixture.join("addon.node"),fixture.join("alias.node"))?;
    let command=r#"python3 -c 'import json; from pathlib import Path; a=Path("autoship-internal-link-fixture/addon.node"); b=a.with_name("alias.node"); print(json.dumps({"same_inode":a.stat().st_ino==b.stat().st_ino,"link_count":a.stat().st_nlink,"value":b.read_text()}))'"#;
    let outcome=session.run("repository",source.path(),&root,&before,None,&[],&[],&[],Some(command),&storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let observed:serde_json::Value=serde_json::from_str(outcome.stdout.trim())?;
    ensure!(observed==serde_json::json!({"same_inode":true,"link_count":2,"value":"owned build artifact"}),"guest did not observe the unchanged internal aliases");
    // POSIX hard links require one filesystem. Keep this external fixture in
    // the admitted target, outside the exported snapshot, rather than /tmp.
    let external=tempfile::NamedTempFile::new_in(&target)?;std::fs::write(external.path(),"private host fixture")?;
    std::fs::hard_link(external.path(),fixture.join("external-alias"))?;
    let refused=session.run("repository",source.path(),&root,&before,None,&[],&[],&[],Some("touch autoship-internal-link-fixture/negative-dispatched"),&storage,&target,&audit,std::time::Duration::from_secs(180)).await;
    ensure!(refused.is_err(),"external host alias was admitted");
    ensure!(!fixture.join("negative-dispatched").exists() && std::fs::read_to_string(external.path())?=="private host fixture","negative probe reached execution or changed host data");
    ensure!(binding(&root).await?==before,"diagnostic changed source binding");audit.protected_health()?;
    let receipt=serde_json::json!({"version":1,"kind":"real_shared_ci_internal_link_smoke","binding":before,
        "adapter_authority":session.authority_sha256,"guest_internal_aliases":observed,"external_alias_refused":true,
        "negative_dispatch_absent":true,"diagnostic_selection_only":true,"full_ci_pass":false,"shipping_authority":false});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_vm_smoke() -> Result<()> {
    let root = PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")
        .context("explicit trusted acceptance repository required")?).canonicalize()?;
    let evidence = PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")
        .context("explicit new private evidence directory required")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root), "evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence, std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()? == evidence, "evidence path must be literal");
    let audit = workflow::workflow_audit(&evidence.join("workflow.db"), "shared-ci-vm-smoke", Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm", Some(&audit))?;
    let before = binding(&root).await?;
    let target = crate::build_storage::effective_target(&root);
    // Invoke the already-built test binary directly, after cargo-managed exits:
    // nesting this admission inside its global lease would deadlock.
    let storage = crate::build_storage::admit_ci(&root, &target).await?;
    let inherited = storage.inherited_files()?;
    ensure!(inherited.len() >= 2, "global and target lease descriptors required");
    let identities: Vec<_> = inherited.iter().map(|file| {
        let meta = file.metadata()?;
        Ok::<_, std::io::Error>(serde_json::json!({"device":meta.dev(),"inode":meta.ino()}))
    }).collect::<std::result::Result<_, _>>()?;
    let session = vm::Session::prepare(&root, &target)?;
    let source = snapshot(&root, &before, &target).await?;
    // This is explicit diagnostic selection, not a claim about actual changed
    // paths or complete applicability on this branch.
    let diagnostic_paths = vec!["scripts/tests/model_router_install_test.py".to_owned()];
    let discovered = session.run("plan", source.path(), &root, &before, None,
        &diagnostic_paths, &[], &[], None, &storage, &target, &audit,
        std::time::Duration::from_secs(180)).await?;
    ensure!(discovered.status == "planned", "real guest planning did not succeed");
    let plan: Plan = serde_json::from_value(discovered.plan.context("guest plan missing")?)?;
    plan.validate(&before)?;
    let check = plan.checks.iter().find(|check|
        check.id == "model-router/installer/3 @{\"os\":\"ubuntu-latest\"}")
        .context("representative pinned Python check missing")?;
    ensure!(check.status == "ready", "representative check is not locally ready: {}", check.reason);
    // Production recreates the snapshot after prerequisite probes, too.
    drop(source);
    let source = snapshot(&root, &before, &target).await?;
    let outcome = session.run("step", source.path(), &root, &before, Some(check),
        &[], &[], &[], None, &storage, &target, &audit,
        check.remaining(std::time::Duration::ZERO)?).await?;
    ensure!(outcome.status == "passed", "real guest Python check failed: {}", outcome.stderr);
    ensure!(outcome.stderr.contains("Ran ") && outcome.stderr.contains("OK"),
        "Python fixture did not report executed tests");
    ensure!(binding(&root).await? == before, "source binding changed during diagnostic");
    audit.protected_health()?;
    let receipt = serde_json::json!({"version":1,"kind":"real_shared_ci_vm_smoke",
        "binding":before,"adapter_authority":session.authority_sha256,
        "check":check.id,"inherited_storage":identities,"guest_plan":"planned","guest_step":"passed",
        "stdout_sha256":digest(outcome.stdout.as_bytes()),"stderr_sha256":digest(outcome.stderr.as_bytes()),
        "diagnostic_selection_only":true,"full_ci_pass":false,"shipping_authority":false});
    audit.record_controller(receipt.clone())?;
    audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"), serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}
