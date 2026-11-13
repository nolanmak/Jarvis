//! Opt-in fixture setup investigation. Raw observation only, never acceptance.
use super::*;
use crate::autoship_contract::{BehaviorInvocation,Criterion,OutputOracle,PathScope,Verifier,VerifierCommand};
use std::os::unix::fs::{DirBuilderExt,OpenOptionsExt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    repository:PathBuf,
    original_base:String,
    program_file:PathBuf,
    evidence_directory:PathBuf,
}

#[tokio::test]
#[ignore = "explicit original-source setup probe; isolated executor owns storage; run built test outside cargo-managed lease"]
async fn original_imessage_fixture_setup_uses_pinned_isolated_executor()->Result<()> {
    use std::io::Write;
    let input=PathBuf::from(std::env::var_os("AUTOSHIP_FIXTURE_PROBE_INPUT").context("explicit probe input required")?);
    let input:Input=serde_json::from_slice(&std::fs::read(input)?)?;
    ensure!(input.original_base.len()==40 && input.original_base.bytes().all(|b|b.is_ascii_hexdigit()),"original commit required");
    ensure!(input.repository.is_absolute() && input.program_file.is_absolute() && input.evidence_directory.is_absolute(),"literal absolute probe paths required");
    let evidence=&input.evidence_directory;
    std::fs::DirBuilder::new().mode(0o700).create(evidence)?;
    ensure!(evidence.canonicalize()?==*evidence,"probe evidence path changed");
    let root=evidence.join("original");
    let cloned=Command::new("/usr/bin/git")
        .args(["--no-replace-objects","-c","core.hooksPath=/dev/null","clone","--shared","--no-checkout","--"])
        .arg(&input.repository).arg(&root).env_clear().env("PATH","/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null").output().await?;
    ensure!(cloned.status.success(),"read-only original object clone failed");
    trusted_git(&root,&["checkout","--detach",&input.original_base]).await?;
    ensure!(actual_head(&root).await?==input.original_base,"probe original base changed");
    let program=std::fs::read_to_string(&input.program_file)?;
    ensure!(program.len()<=64*1024,"setup probe program exceeds bound");
    let adapter="crates/augmentagent-cli/src/bin/autoship_imessage_setup_probe.rs";
    let proposal=ContractProposal {
        contract:AcceptanceContract {schema_version:1,revision:1,issue:Some(1),pull_request:None,
            criteria:vec![Criterion {id:"setup_observation".into(),requirement:"Expose synthetic input through original iMessage card APIs".into(),verifier_ids:vec!["observe".into()]}],
            allowed_paths:vec![PathScope::File(adapter.into())],
            verifiers:vec![Verifier {id:"observe".into(),command:VerifierCommand::BlackBox {
                invocation:BehaviorInvocation::CargoBinary {package:"augmentagent-cli".into(),bin:"autoship_imessage_setup_probe".into(),args:vec![]},
                // Metadata is needed for immutable pinning. This probe never
                // evaluates an oracle, creates receipts or claims red/green.
                oracle:OutputOracle::Json {value:serde_json::Value::Null}},pinned_files:vec![adapter.into()],failure_witness:"setup investigation only".into()}]},
        files:BTreeMap::from([(adapter.into(),program)]),
    };
    install_harness(&root,&proposal)?;
    let registration=RegisteredContract::register(&root,proposal.contract.clone(),input.original_base.clone(),environment_id(&root)?)?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"original-imessage-fixture-setup-probe",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("acceptance_verifier",Some(&audit))?;
    let target=protected_verifier_target(&root,&registration)?;
    let lease=crate::build_storage::admit(&root,&target).await?;
    let committed=verifier_checkout::materialize(&root,&input.original_base,&registration,&target).await?;
    let invocation=proposal.contract.verifiers[0].command.invocation()?;
    let observation=isolated_observation(committed.path(),&target,invocation,&audit).await?;
    audit.protected_health()?;
    let terminal=observation.exit_code==Some(0) && observation.output_complete && !observation.interrupted;
    let record=serde_json::json!({"version":1,"kind":"original-fixture-setup-observation","original_base":input.original_base,
        "program_sha256":amendment::digest(proposal.files[adapter].as_bytes()),
        "observation":{"exit_code":observation.exit_code,"stdout":observation.stdout,"stderr":observation.stderr,
            "interrupted":observation.interrupted,"output_complete":observation.output_complete},
        "model_calls":0,"acceptance":false,"behavioral_red_claim":false,"shipping_authority":false});
    let mut output=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW)
        .open(evidence.join("observation.json"))?;
    output.write_all(&serde_json::to_vec_pretty(&record)?)?;output.sync_all()?;
    drop(committed);drop(lease);
    ensure!(terminal,"original fixture setup did not complete; inspect private observation.json");
    let data:serde_json::Value=serde_json::from_str(record["observation"]["stdout"].as_str().context("missing probe stdout")?)?;
    ensure!(data["cards"].as_u64().is_some_and(|cards|cards>0),"original API probe produced no approval card");
    println!("Original isolated fixture setup produced production card data; no oracle evaluation, model calls or acceptance authority");
    Ok(())
}
