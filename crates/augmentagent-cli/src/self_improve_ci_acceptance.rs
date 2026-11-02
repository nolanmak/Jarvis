//! Opt-in real build-VM smoke. No provider calls, publication or merge authority.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Complete a real compiled fixture plan, then prove a newer failing run
/// cannot return the previous run's protected local-completion capability.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_complete_coverage() -> Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-complete-coverage",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("ci-complete-fixture-").tempdir_in("/tmp")?;
    let root=fixture.path();
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    fixture::install(root)?;
    std::fs::write(root.join("arithmetic.py"),"def add(a, b):\n    return a + b\n")?;
    git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
        "-c","core.hooksPath=/dev/null","commit","-qm","working arithmetic"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    fixture::scope(root,async {
        let completed=verify_admitted(root,&audit,&storage,&target).await?;
        let record=completed.receipt().value();
        ensure!(record["kind"]=="shared_ci_local_complete" && record["reusable_passing_proof"]==false
            && record["shipping_authority"]==false && record["binding"]["verification_head"]==head,
            "local completion was misbound or claimed final freshness authority");
        let commands=record["command_receipts"].as_object().context("local coverage missing")?;
        ensure!(commands.len()==1,"fixture plan coverage was partial or overcounted");
        for (id,hash) in commands {
            let command=audit.controller_receipt(hash.as_str().context("command receipt hash missing")?)?;
            ensure!(command.value()["check"]==*id && command.value()["status"]=="passed",
                "local coverage used another command's receipt");
            ensure!(command.value()["guest_worker_quiescence"]["worker_free"]==true
                && command.value()["guest_worker_quiescence"]["scopes"].as_array().is_some_and(|scopes|!scopes.is_empty())
                && command.value()["input_write_observation"]["kind"]=="ci-input-write-observation"
                && command.value()["guest_worker_quiescence"]["scopes"].as_array().unwrap()
                    .contains(&command.value()["input_write_observation"]["guest_worker_quiescence"]),
                "input observation did not follow its own positive guest worker cleanup");
        }
        for (field,kind) in [("started_receipt_sha256","shared_ci_started"),("plan_receipt_sha256","shared_ci_plan")] {
            let prior=audit.controller_receipt(record[field].as_str().context("coverage lineage missing")?)?;
            ensure!(prior.value()["kind"]==kind && prior.value()["run_id"]==record["run_id"],
                "local completion borrowed another execution's plan or start");
        }
        let current_context=crate::autoship_workflow::Binding {head:head.clone(),
            contract:"isolated-ci-fixture-v1".into(),evidence_hash:digest(b"isolated CI fixture acceptance context")};
        let expected=proof::CiExpectation::current(root,"fixture/shared-ci#1",&current_context).await?;
        let refused=proof::publish_passing_ci(&audit,&completed,&expected).err()
            .context("preparation-only actual CI execution minted a final passing proof")?;
        ensure!(refused.downcast_ref::<crate::autoship_freshness::Hold>().is_some_and(|hold|
            hold.reason==crate::autoship_freshness::Reason::InputCensusUnknown),
            "preparation-only refusal was not the expected typed input prerequisite: {refused:#}");
        let old=completed.receipt().payload_sha256().to_owned();
        std::fs::write(root.join("arithmetic.py"),"def add(a, b):\n    return 0\n")?;
        git(root,&["add","arithmetic.py"]).await?;
        git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
            "-c","core.hooksPath=/dev/null","commit","-qm","intentional assertion failure"]).await?;
        let error=verify_admitted(root,&audit,&storage,&target).await.err()
            .context("failing newer CI run returned a completed capability")?;
        ensure!(format!("{error:#}").contains("shared CI check"),"new run failed before source assertion: {error:#}");
        ensure!(audit.controller_receipt(&old)?.value()["binding"]["verification_head"]==head,
            "failed retry corrupted historical completion evidence");
        std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "version":1,"kind":"real_shared_ci_complete_coverage","local_checks_covered":commands.len(),
            "protected_start_plan_and_command_lineage":true,"newer_failed_run_cannot_return_old_completion":true,
            "current_git_runtime_and_owned_generation_expectations":true,
            "guest_worker_quiescence_precedes_bound_input_capture":true,
            "preparation_only_actual_producer_refused_final_ci_authority":true,
            "historical_completion_preserved":true,"full_ci_pass":false,"shipping_authority":false,
            "autonomous_merges_claimed":0}))?)?;
        Ok::<_,anyhow::Error>(())
    }).await??;
    drop(storage);
    Ok(())
}

/// Publication requires a second, separately supervised VM after the original
/// producer has exited. This checks actual Cargo reuse and kernel permissions.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_dependency_seed_replay()->Result<()> {
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-dependency-seed",Some(&root))?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&root);
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let fixture=tempfile::Builder::new().prefix("dependency-seed-fixture-").tempdir_in(&target)?;
    let repo=fixture.path();std::fs::create_dir(repo.join("src"))?;
    let registry="registry+https://github.com/rust-lang/crates.io-index";
    let package_id="registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.18";
    let lock=format!("version = 4\n[[package]]\nname = \"seed-consumer\"\nversion = \"0.0.0\"\ndependencies = [\"itoa\"]\n\n[[package]]\nname = \"itoa\"\nversion = \"1.0.18\"\nsource = \"{registry}\"\nchecksum = \"8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682\"\n");
    std::fs::write(repo.join("Cargo.lock"),&lock)?;
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname = \"seed-consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n[dependencies]\nitoa = \"=1.0.18\"\n")?;
    std::fs::write(repo.join("src/main.rs"),"fn main() { println!(\"{}\", itoa::Buffer::new().format(42)); }\n")?;
    std::fs::write(repo.join("package.json"),"{\"name\":\"ci-generated-input-fixture\",\"private\":true}\n")?;
    git(repo,&["-c","init.templateDir=","init","-q"]).await?;
    git(repo,&["add","."]).await?;
    git(repo,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","seed consumer"]).await?;
    let head=git(repo,&["rev-parse","HEAD"]).await?;
    git(repo,&["update-ref","refs/remotes/origin/main",head.trim()]).await?;
    let binding=binding(repo).await?;
    let metadata=serde_json::json!({"version":1,"workspace_root":"/workspace","packages":[
        {"id":"root","name":"seed-consumer","version":"0.0.0","source":null,"manifest_path":"/workspace/Cargo.toml",
         "targets":[{"kind":["bin"],"src_path":"/workspace/src/main.rs"}]},
        {"id":package_id,"name":"itoa","version":"1.0.18","source":registry,
         "manifest_path":"/ci-cargo-home/registry/src/pinned/itoa-1.0.18/Cargo.toml",
         "targets":[{"kind":["lib"],"src_path":"/ci-cargo-home/registry/src/pinned/itoa-1.0.18/src/lib.rs"}]}],
         "resolve":{"nodes":[{"id":"root","dependencies":[package_id],"features":[]},
                                    {"id":package_id,"dependencies":[],"features":[]}]}});
    let session=vm::Session::prepare(&root,&target)?;
    let producer_source=session.dependency_source(&metadata,&lock,&binding,&target).await?;
    let producer=uuid::Uuid::new_v4().to_string();
    let mut action=serde_json::json!({"kind":"produce","producer":producer,
        "source_sha256":producer_source.source_sha256,"dependency_inputs_sha256":producer_source.dependency_inputs_sha256});
    ensure!(session.producer_ack(&producer_source.source,&producer).is_err(),"unexecuted producer received an acknowledgement");
    let timeout=std::time::Duration::from_secs(180);
    let outcome=session.run_with_dependency("dependency",&producer_source.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&action)).await?;
    ensure!(outcome.status=="seed_produced","producer disposition invalid");
    ensure!(outcome.dependency_result.as_ref().context("producer receipt missing")?["cargo_exit_code"]==0,"isolated Cargo compilation failed");
    action["kind"]="publish".into();action["ack"]=session.producer_ack(&producer_source.source,&producer)?;
    let outcome=session.run_with_dependency("dependency",&producer_source.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&action)).await?;
    let published=outcome.dependency_result.context("published seed receipt missing")?;
    ensure!(published["units"]==1,"isolated producer did not publish exactly the external unit");
    let install=session.dependency_seed(repo,&binding,&target,&storage,&audit).await?
        .context("ordinary verification did not select its dependency seed")?;
    ensure!(install["seed"]==published["seed"],"automatic lookup did not reuse the protected seed");
    let reopened=vm::Session::prepare(&root,&target)?;
    ensure!(reopened.authority_sha256==session.authority_sha256,"equivalent controller restart changed trusted authority");
    let restored=reopened.dependency_seed(repo,&binding,&target,&storage,&audit).await?
        .context("controller restart lost the trusted dependency seed")?;
    ensure!(restored==install,"restart selected a different seed");
    drop(reopened);
    let build=r#"set -e
python3 -I -c 'from pathlib import Path
modules=Path("node_modules")
if not (modules/"tool").exists():
 (modules/"tool").mkdir();(modules/"tool/cli.js").write_text("console.log(42)\n")
 (modules/".bin").mkdir();(modules/".bin/tool").symlink_to("../tool/cli.js")
 (modules/"tool-alias").symlink_to("tool",target_is_directory=True)'
cargo build --offline --locked --message-format=json > target/messages.json
python3 -c 'import json; from pathlib import Path
units=[json.loads(line) for line in Path("target/messages.json").read_text().splitlines()]
compiled=[m["target"]["name"] for m in units if m["reason"]=="compiler-artifact" and not m["fresh"]]
print(json.dumps(sorted(compiled)))'
target/debug/seed-consumer
"#;
    let mut generations=Vec::new();
    for _ in 0..2 {
        let source=snapshot(repo,&binding,&target).await?;generations.push(source.generation().to_owned());
        let first=session.run_with_dependency("repository",&source,repo,&binding,None,&[],&[],&[],Some(build),
            &storage,&target,&audit,timeout,Some(&install)).await?;
        ensure!(first.status=="passed" && first.stdout.trim()=="[\"seed-consumer\"]\n42",
            "external unit recompiled or consumer failed: {} {}",first.stdout,first.stderr);
        let committed=first.command_receipt.as_ref().context("successful VM command lacks protected host receipt")?;
        let record=committed.value();
        ensure!(record["kind"]=="shared_ci_command_complete" && record["status"]=="passed"
            && record["shipping_authority"]==false
            && record["cache_preparation"]["binding"]["generation"]==source.generation()
            && record["cache_preparation"]["binding"]["head"]==binding.verification_head
            && record["cache_preparation"]["binding"]["tree"]==binding.index_tree
            && record["cache_preparation"]["dependency_seed"]==published["seed"],
            "protected command receipt lost its original source/cache/seed binding");
        ensure!(record["input_write_observation"]["changed"]==true
            && record["input_write_observation"]["changed_roots"].as_array().is_some_and(|roots|!roots.is_empty()),
            "declared generated input creation and npm-style aliases were not observed");
        let quiescent=audit.controller_receipt(record["quiescence_receipt_sha256"].as_str()
            .context("command receipt lacks protected quiescence reference")?)?;
        ensure!(quiescent.value()["kind"]=="shared_ci_vm_quiescent"
            && quiescent.value()["command"]==record["cache_preparation"]["command"]
            && quiescent.value()["generation"]==source.generation(),
            "command receipt refers to another invocation's teardown");
        ensure!(audit.controller_receipt(committed.payload_sha256())?.value()==record,
            "command receipt did not survive committed audit reload");
        let again=session.run("repository",&source,repo,&binding,None,&[],&[],&[],Some(build),
            &storage,&target,&audit,timeout).await?;
        ensure!(again.status=="passed" && again.stdout.trim()=="[]\n42","compatible same-job units recompiled: {} {}",again.stdout,again.stderr);
        ensure!(again.command_receipt.as_ref().is_some_and(|receipt|
            receipt.payload_sha256()!=committed.payload_sha256()
                && receipt.value()["cache_preparation"]["binding"]["generation"]==source.generation()
                && receipt.value()["input_write_observation"]["changed"]==false),
            "repeated command reused an older invocation's protected completion");
        let tamper=r#"python3 -c 'import errno,os; from pathlib import Path
p=next(Path("target/debug/deps").glob("libitoa-*.rlib"))
for operation in (lambda:p.write_bytes(b"corrupt"),lambda:p.unlink(),lambda:p.parent.rename(p.parent.with_name("moved"))):
 try: operation()
 except OSError as error: assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM),error
 else: raise SystemExit("trusted artifact or ancestor was mutable")
assert os.environ["CARGO_HOME"]=="/ci-cargo-home"
try:list(Path("/build-cache").iterdir())
except PermissionError:pass
else:raise SystemExit("shared image visible")
print("SEALED")'"#;
        let tamper=session.run("repository",&source,repo,&binding,None,&[],&[],&[],Some(tamper),
            &storage,&target,&audit,timeout).await?;
        ensure!(tamper.status=="passed" && tamper.stdout.trim()=="SEALED","trusted seed confinement failed: {} {}",tamper.stdout,tamper.stderr);
    }
    ensure!(generations[0]!=generations[1],"source reconstruction reused generation");
    let receipt=serde_json::json!({"version":1,"kind":"real_shared_ci_dependency_seed_replay","seed":published["seed"],
        "adapter_authority":session.authority_sha256,"generations":generations,
        "external_units_recompiled_across_generations":0,"same_job_compatible_units_recompiled":0,
        "seed_corruption_denied":true,"automatic_seed_lookup":true,"equivalent_controller_restart_reuses_seed":true,
        "bound_preparation_and_committed_quiescence_receipts":true,
        "declared_generated_namespace_aliases_observed":true,"unchanged_generated_inputs_preserve_compatible_reuse":true,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

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
    // The host's idle probe is advisory. Competing managed builds can acquire
    // the lease before this test starts; retry only typed storage deferrals,
    // with no guest, model call or protected storage ownership held meanwhile.
    let admission_deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(300);
    let storage=loop {
        match crate::build_storage::admit_ci(&root,&target).await {
            Ok(storage)=>break storage,
            Err(error) if error.downcast_ref::<crate::storage_reservations::Busy>().is_some()
                && tokio::time::Instant::now()<admission_deadline => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(error)=>return Err(error),
        }
    };
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
    let private_cache=r#"python3 -c 'import os; from pathlib import Path
assert os.environ["CARGO_HOME"]=="/ci-cargo-home"
try: list(Path("/build-cache").iterdir())
except PermissionError: pass
else: raise SystemExit("worker can inspect shared cache generations")
try: open("/dev/vda","rb")
except PermissionError: pass
else: raise SystemExit("worker can read the underlying cache block device")
print("CACHE_ROOT_PRIVATE")'"#;
    let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some(private_cache),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="CACHE_ROOT_PRIVATE","shared cache was visible outside the job mounts");
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
        "shared_cache_root_private":true,"stable_cargo_home":true,
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
        drop(source);
        let seed=session.dependency_seed(&root,&before,&target,&storage,&audit).await?;
        let source=snapshot(&root,&before,&target).await?;
        session.run_with_dependency("step",&source,&root,&before,Some(check),&[],&[],&[],None,
            &storage,&target,&audit,check.remaining(std::time::Duration::ZERO)?,seed.as_ref()).await
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
