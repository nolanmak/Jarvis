//! Opt-in real build-VM smoke. No provider calls, publication or merge authority.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Behavioral cache replay through the production adapter and actual managed
/// image. This grants no workflow, review, publication or merge authority.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_cache_freshness_replay() -> Result<()> {
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()?==evidence,"evidence must be literal");
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-cache-freshness",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm",Some(&audit))?;
    let target=crate::build_storage::effective_target(&root);
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let fixture=tempfile::Builder::new().prefix("freshness-source-fixture-").tempdir_in(&target)?;
    let repo=fixture.path();std::fs::create_dir(repo.join("src"))?;
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname = \"freshness-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n")?;
    std::fs::write(repo.join("Cargo.lock"),"version = 4\n[[package]]\nname = \"freshness-fixture\"\nversion = \"0.0.0\"\n")?;
    git(repo,&["-c","init.templateDir=","init","-q"]).await?;
    async fn commit(repo:&Path,source:&str)->Result<String> {
        std::fs::write(repo.join("src/main.rs"),source)?;
        git(repo,&["add","."]).await?;
        let result=Command::new("/usr/bin/git").args(["-c","core.hooksPath=/dev/null","-c","commit.gpgsign=false",
            "-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","synthetic source"])
            .current_dir(repo).env_clear().env("PATH","/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
            .env("GIT_AUTHOR_DATE","2000-01-01T00:00:00Z").env("GIT_COMMITTER_DATE","2000-01-01T00:00:00Z")
            .output().await?;
        ensure!(result.status.success(),"fixture commit failed");
        Ok(git(repo,&["rev-parse","HEAD"]).await?.trim().into())
    }
    let original="fn main() { println!(\"first\"); }\n#[test] fn regression() { assert_eq!(2+2,4); }\n";
    let first=commit(repo,original).await?;
    git(repo,&["update-ref","refs/remotes/origin/main",&first]).await?;
    let session=vm::Session::prepare(&root,&target)?;
    let a=binding(repo).await?;let source=snapshot(repo,&a,&target).await?;
    let first_generation=source.generation().to_owned();
    let stamp=std::fs::metadata(source.path().join("src/main.rs"))?.modified()?;
    let command=r#"set -e
cat > target/rustc-probe <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> /workspace/target/compiler-calls
exec "$@"
SH
chmod 700 target/rustc-probe
RUSTC_WRAPPER=/workspace/target/rustc-probe cargo run --offline --locked --quiet
python3 -c 'from pathlib import Path; print("UNITS="+str(sum("--crate-name freshness_fixture " in line for line in Path("target/compiler-calls").read_text().splitlines())))'
"#;
    let timeout=std::time::Duration::from_secs(180);
    let mut reclaimed=0;
    for _ in 0..2 {
        let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some(command),&storage,&target,&audit,timeout).await?;
        reclaimed+=outcome.cache_reclaimed.len();
        ensure!(outcome.status=="passed" && outcome.stdout.trim()=="first\nUNITS=1",
            "same-generation output/reuse mismatch: {} {}",outcome.stdout,outcome.stderr);
    }
    let tamper=r#"python3 -c 'import errno; from pathlib import Path
p=Path("src/main.rs")
try: p.write_text("fn main() {}")
except OSError as error:
 assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM); print("TRACKED_READ_ONLY")
else: raise SystemExit("tracked source was writable")'"#;
    let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some(tamper),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="TRACKED_READ_ONLY","tracked-source substitution was possible");
    let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some("cargo test --offline --locked --quiet"),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.contains("1 passed"),"original assertion did not execute");
    drop(source);
    commit(repo,"fn main() { println!(\"other\"); }\n").await?;
    let b=binding(repo).await?;let source=snapshot(repo,&b,&target).await?;
    ensure!(std::fs::metadata(source.path().join("src/main.rs"))?.modified()?==stamp,"fixture timestamps differ");
    let outcome=session.run("repository",&source,repo,&b,None,&[],&[],&[],Some(command),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="other\nUNITS=1","changed source ran stale output: {} {}",outcome.stdout,outcome.stderr);
    drop(source);
    commit(repo,"fn main() { missing_function(); }\n").await?;
    let bad=binding(repo).await?;let source=snapshot(repo,&bad,&target).await?;
    let outcome=session.run("repository",&source,repo,&bad,None,&[],&[],&[],Some("cargo build --offline --locked --quiet"),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="failed" && outcome.stderr.contains("E0425"),"compile error was hidden by stale success");
    drop(source);
    commit(repo,"fn main() {}\n#[test] fn regression() { assert_eq!(2+2,5); }\n").await?;
    let bad=binding(repo).await?;let source=snapshot(repo,&bad,&target).await?;
    let outcome=session.run("repository",&source,repo,&bad,None,&[],&[],&[],Some("cargo test --offline --locked --quiet"),&storage,&target,&audit,timeout).await?;
    reclaimed+=outcome.cache_reclaimed.len();
    ensure!(outcome.status=="failed" && outcome.stdout.contains("1 failed"),"failing assertion was hidden by stale success");
    drop(source);
    git(repo,&["reset","--hard",&first]).await?;
    let restored=binding(repo).await?;let source=snapshot(repo,&restored,&target).await?;
    ensure!(source.generation()!=first_generation,"same-HEAD reconstruction inherited its generation");
    let outcome=session.run("repository",&source,repo,&restored,None,&[],&[],&[],Some(command),&storage,&target,&audit,timeout).await?;
    reclaimed+=outcome.cache_reclaimed.len();
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="first\nUNITS=1","same-HEAD reconstruction inherited changed artifacts");
    ensure!(reclaimed>0,"bounded reclamation did not retire any prior quiescent generation");
    audit.protected_health()?;
    let receipt=serde_json::json!({"version":1,"kind":"real_shared_ci_cache_freshness_replay",
        "adapter_authority":session.authority_sha256,"equal_old_timestamps":true,"changed_output":true,
        "compile_error_observed":"E0425","failing_assertion_observed":true,"same_head_reconstruction":true,
        "same_generation_compatible_units_recompiled":0,"tracked_source_read_only":true,
        "reclaimed_generations_observed":reclaimed,"active_generation_preserved":true,
        "trusted_external_seed_tested":false,"full_ci_pass":false,"shipping_authority":false,
        "additional_model_calls":0,"autonomous_merges_claimed":0});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

/// Observe the exact Rust build check that fails in the deployed main gate.
/// Persist bounded private output even on failure; no production verdict is set.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_rust_build_diagnostic() -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()?==evidence,"evidence path must be literal");
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-rust-build-diagnostic",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm",Some(&audit))?;
    let before=binding(&root).await?;let target=crate::build_storage::effective_target(&root);
    // Run the already-built opt-in binary after the managed build exits.
    // This admission owns the actual storage lease throughout the guest step.
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let session=vm::Session::prepare(&root,&target)?;
    let source=snapshot(&root,&before,&target).await?;
    let discovered=session.run("plan",&source,&root,&before,None,&["Cargo.toml".into()],&[],&[],None,
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    ensure!(discovered.status=="planned","guest planning unavailable");
    let plan:Plan=serde_json::from_value(discovered.plan.context("guest plan missing")?)?;plan.validate(&before)?;
    let check=plan.checks.iter().find(|check|check.id==r#"platform/platform/3 @{"os":"ubuntu-latest"}"#).context("normal Rust CLI build check missing")?;
    ensure!(check.status=="ready","normal Rust CLI build check is not ready: {}",check.reason);
    let mut onnx_cache_probe=None;
    let mut onnx_probe_error=None;
    if std::env::var("AUTOSHIP_CI_ACCEPTANCE_ORT_CACHE_PROBE").as_deref()==Ok("1") {
        let command=r#"python3 -c 'import json,os; from pathlib import Path
cache=Path(os.environ["ORT_CACHE_DIR"])
library=next(cache.glob("dfbin/*/*/libonnxruntime.a"))
writable=False
try:
 fd=os.open(library,os.O_WRONLY); os.close(fd); writable=True
except OSError: pass
print(json.dumps({"cache":str(cache),"library_bytes":library.stat().st_size,"writable":writable,"interfaces":sorted(p.name for p in Path("/sys/class/net").iterdir()),"home":os.environ["HOME"]}))'"#;
        let probe=session.run("repository",&source,&root,&before,None,&[],&[],&[],Some(command),
            &storage,&target,&audit,std::time::Duration::from_secs(180)).await;
        let mut diagnostic=match &probe {
            Ok(outcome)=>serde_json::json!({"status":outcome.status,"stdout":outcome.stdout,"stderr":outcome.stderr}),
            Err(error)=>serde_json::json!({"status":"unavailable","error":format!("{error:#}")}),
        };
        let evaluated:Result<serde_json::Value>=(|| {
            let probe=probe?;ensure!(probe.status=="passed","ONNX cache guest probe failed");
            let observed:serde_json::Value=serde_json::from_str(probe.stdout.trim())?;
            ensure!(observed["cache"]=="/ort-cache" && observed["writable"]==false
                && observed["interfaces"]==serde_json::json!(["lo"]) && observed["home"]=="/home/worker"
                && observed["library_bytes"].as_u64().is_some_and(|bytes|bytes>0),"ONNX cache exposure mismatch");
            Ok(observed)
        })();
        match evaluated {
            Ok(observed)=>onnx_cache_probe=Some(observed),
            Err(error)=>{
                diagnostic["error"]=format!("{error:#}").into();
                onnx_cache_probe=Some(diagnostic);onnx_probe_error=Some(error);
            },
        }
    }
    // An optional probe failure still reaches the private receipt writer. Do
    // not start a Rust build after failed confinement/dependency observation.
    let rust_build_attempted=onnx_probe_error.is_none();
    let observed=if let Some(error)=onnx_probe_error {Err(error)} else {async {
        drop(source);let source=snapshot(&root,&before,&target).await?;
        session.run("step",&source,&root,&before,Some(check),&[],&[],&[],None,
            &storage,&target,&audit,check.remaining(std::time::Duration::ZERO)?).await
    }.await};
    let (status,stdout,stderr,error)=match observed {
        Ok(outcome)=>(outcome.status,outcome.stdout,outcome.stderr,None),
        Err(error)=>("unavailable".into(),String::new(),String::new(),Some(format!("{error:#}"))),
    };
    let receipt=serde_json::json!({"version":1,"kind":"real-shared-ci-rust-build-diagnostic","binding":before,
        "adapter_authority":session.authority_sha256,"check":check.id,"status":status,
        "stdout":stdout,"stderr":stderr,"error":error,"additional_model_calls":0,"onnx_cache_probe":onnx_cache_probe,"rust_build_attempted":rust_build_attempted,
        "diagnostic_selection_only":true,"full_ci_pass":false,"shipping_authority":false});
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(evidence.join("receipt.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&receipt)?)?;file.sync_all()?;
    audit.record_controller(serde_json::json!({"version":1,"kind":"shared_ci_rust_diagnostic_observed","check":check.id,
        "status":status,"stdout_sha256":digest(stdout.as_bytes()),"stderr_sha256":digest(stderr.as_bytes()),"shipping_authority":false}))?;
    audit.protected_health()?;ensure!(binding(&root).await?==before,"source changed during diagnostic");
    ensure!(status=="passed","isolated Rust build failed; inspect the private diagnostic receipt");Ok(())
}

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
    let outcome=session.run("repository",&source,&root,&before,None,&[],&[],&[],Some(command),&storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let observed:serde_json::Value=serde_json::from_str(outcome.stdout.trim())?;
    ensure!(observed==serde_json::json!({"same_inode":true,"link_count":2,"value":"owned build artifact"}),"guest did not observe the unchanged internal aliases");
    // POSIX hard links require one filesystem. Keep this external fixture in
    // the admitted target, outside the exported snapshot, rather than /tmp.
    let external=tempfile::NamedTempFile::new_in(&target)?;std::fs::write(external.path(),"private host fixture")?;
    std::fs::hard_link(external.path(),fixture.join("external-alias"))?;
    let refused=session.run("repository",&source,&root,&before,None,&[],&[],&[],Some("touch autoship-internal-link-fixture/negative-dispatched"),&storage,&target,&audit,std::time::Duration::from_secs(180)).await;
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
    let discovered = session.run("plan", &source, &root, &before, None,
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
    let outcome = session.run("step", &source, &root, &before, Some(check),
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
