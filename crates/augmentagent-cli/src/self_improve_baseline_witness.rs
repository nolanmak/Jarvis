//! Qualification of the *measurement path*, separate from raw red/green data.
//! A native judgment is not implementation, final review or merge authority.
use super::*;
use std::{collections::BTreeSet, io::Read, os::unix::fs::{MetadataExt,OpenOptionsExt}};

#[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="snake_case")]
enum Measurement { DirectProduction, MissingFixture, TestOutcome, SetupFailure, Unknown }

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Citation { path:String, start_line:usize, end_line:usize, quote:String }

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    verifier_id:String,
    criterion_ids:Vec<String>,
    measurement:Measurement,
    original_fixture_available:bool,
    concrete_input:String,
    reason:String,
    production_citations:Vec<Citation>,
}

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Judgment { version:u32, context:String, verifiers:Vec<Witness> }

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    version:u32,
    context:String,
    response_sha256:String,
    native_control:PathBuf,
    provenance:scope_authority::NativeReplyReference,
}

pub(super) fn policy_hash()->String {
    amendment::digest(&serde_json::to_vec(&("original-production-witness-v1",amendment::digest(include_bytes!("self_improve_baseline_witness.rs")),
        amendment::digest(include_bytes!("autoship_contract.rs")),amendment::digest(include_bytes!("self_improve_contract.rs")),
        amendment::digest(include_bytes!("self_improve_verifier_checkout.rs")),scope_authority::witness_policy_hash())).unwrap())
}
fn context(proposal:&ContractProposal,baseline:&BaselineEvidence)->Result<String> {
    context_with_policy(proposal,baseline,&policy_hash())
}
fn context_with_policy(proposal:&ContractProposal,baseline:&BaselineEvidence,policy:&str)->Result<String> {
    ensure!(proposal.contract==*baseline.registration.contract(),"witness proposal differs from registration");
    ensure!(baseline.registration.baseline_result(&baseline.receipts)?==BaselineResult::Reproduced,"witness lacks measured baseline red");
    Ok(amendment::digest(&serde_json::to_vec(&("baseline-measurement-qualification-v1",proposal,baseline,policy))?))
}
fn artifact_path(control:&Path,context:&str)->PathBuf {control.join(format!("baseline-witness-{context}.json"))}
fn raw_path(control:&Path,context:&str,hash:&str)->PathBuf {control.join(format!("baseline-witness-{context}-{hash}.response"))}
pub(super) fn private_bytes(path:&Path,limit:usize)->Result<Vec<u8>> {
    let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let meta=file.metadata()?;
    ensure!(meta.is_file() && meta.nlink()==1 && meta.uid()==unsafe{libc::geteuid()} && meta.mode()&0o077==0 && meta.len()<=limit as u64,"baseline witness is not bounded private evidence");
    let mut bytes=Vec::new();file.take(limit as u64+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=limit,"baseline witness grew beyond its envelope");Ok(bytes)
}
fn immutable(control:&Path,path:&Path,bytes:&[u8])->Result<()> {
    use std::io::Write;
    if path.try_exists()? {ensure!(private_bytes(path,bytes.len())?==bytes,"immutable baseline witness changed");return Ok(());}
    let mut file=tempfile::NamedTempFile::new_in(control)?;file.write_all(bytes)?;file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|e|e.error)?;std::fs::File::open(control)?.sync_all()?;Ok(())
}
fn original_git(root:&Path,args:&[&str])->Result<String> {
    let out=std::process::Command::new("/usr/bin/git").args(["--no-replace-objects","-c","core.hooksPath=/dev/null"])
        .args(args).current_dir(root).env_clear().env("PATH","/usr/bin:/bin").stdin(Stdio::null()).output()
        .map_err(|e|anyhow::Error::from(e).context(initial_repair::Hold::EvidenceUnknown))?;
    ensure!(out.status.success(),initial_repair::Hold::EvidenceUnknown);
    Ok(String::from_utf8(out.stdout)?)
}
fn validate(root:&Path,proposal:&ContractProposal,baseline:&BaselineEvidence,expected:&str,raw:&str)->Result<Judgment> {
    let judgment:Judgment=serde_json::from_str(raw.trim())?;
    ensure!(judgment.version==1 && judgment.context==expected,"baseline witness has a different source/measurement/policy context");
    let wanted=proposal.contract.verifiers.iter().map(|v|v.id.as_str()).collect::<BTreeSet<_>>();
    let found=judgment.verifiers.iter().map(|v|v.verifier_id.as_str()).collect::<BTreeSet<_>>();
    ensure!(wanted==found && found.len()==judgment.verifiers.len(),"baseline witness must cover every verifier exactly once");
    let mut total=0usize;
    for witness in &judgment.verifiers {
        let criteria=proposal.contract.criteria.iter().filter(|c|c.verifier_ids.contains(&witness.verifier_id)).map(|c|c.id.as_str()).collect::<BTreeSet<_>>();
        let covered=witness.criterion_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
        ensure!(criteria==covered && covered.len()==witness.criterion_ids.len(),"baseline witness dropped or invented a criterion");
        ensure!(!witness.reason.trim().is_empty(),"baseline witness needs an evidence-based reason");
        if witness.measurement!=Measurement::DirectProduction || !witness.original_fixture_available {continue;}
        ensure!(!witness.concrete_input.trim().is_empty() && !witness.production_citations.is_empty() && witness.production_citations.len()<=16,"production witness needs concrete original input and source citations");
        for cite in &witness.production_citations {
            ensure!(scope_authority::witness_source_allowed(&cite.path)
                && !cite.path.is_empty() && Path::new(&cite.path).components().all(|p|matches!(p,std::path::Component::Normal(_)))
                && !proposal.files.contains_key(&cite.path),"witness citation is not original production source");
            ensure!(cite.start_line>0 && cite.end_line>=cite.start_line && cite.end_line-cite.start_line<600 && !cite.quote.trim().is_empty(),"invalid original production citation range");
            let base=baseline.registration.baseline_head();
            let entry=original_git(root,&["ls-tree",base,"--",&cite.path])?;
            ensure!(entry.starts_with("100644 blob ") || entry.starts_with("100755 blob "),"citation must name an existing original regular source file");
            let source=original_git(root,&["show","--no-ext-diff","--no-textconv",&format!("{base}:{}",cite.path)])?;
            let lines=source.lines().collect::<Vec<_>>();
            ensure!(cite.end_line<=lines.len(),"original production citation exceeds source");
            let quoted=lines[cite.start_line-1..cite.end_line].join("\n");
            ensure!(cite.quote==quoted,"production citation differs from immutable original source");
            total=total.checked_add(quoted.len()).context("citation size overflow")?;
            ensure!(total<=512*1024,"production citations exceed existing source evidence envelope");
        }
    }
    Ok(judgment)
}
fn disposition(judgment:&Judgment)->Result<()> {
    // This is a completed, authenticated semantic judgment. Unknown does not
    // qualify production behavior, but it may dispute the verifier definition
    // through the existing bounded independent amendment path. Missing or
    // unauthenticated physical evidence is held before disposition is reached.
    if judgment.verifiers.iter().any(|v|v.measurement!=Measurement::DirectProduction || !v.original_fixture_available) {
        // Free-form native reasons/citations stay in the private artifact, not
        // the generic error path that can feed issue/PR activity notes.
        let causes=judgment.verifiers.iter().filter(|v|v.measurement!=Measurement::DirectProduction || !v.original_fixture_available)
            .map(|v|serde_json::json!({"verifier":v.verifier_id,"measurement":v.measurement,"original_fixture_available":v.original_fixture_available})).collect::<Vec<_>>();
        return Err(BaselineNotRed {kind:Some(initial_repair::FailureKind::NotReproduced),detail:format!("baseline_witness_invalid: {}; exact independent diagnosis retained privately",serde_json::to_string(&causes)?)}.into());
    }
    Ok(())
}
/// A negative observation can reopen consideration by the existing independent
/// authority. It can never be used as a positive qualification receipt.
pub(super) fn retained_semantic_judgment(root:&Path,prepared:&PreparedAcceptance,
    custody:&[crate::autoship_workflow::NativeScopeCustody])->Result<Option<String>> {
    Ok(retained_judgment_matching(root,prepared,custody,
        |judgment|judgment.verifiers.iter().any(|v|v.measurement==Measurement::Unknown))?.map(|(identity,_)|identity))
}

/// Share authentication, not selection: diagnostics accept every negative
/// class while legacy reconsideration retains its original Unknown predicate.
fn retained_judgment_matching(root:&Path,prepared:&PreparedAcceptance,
    custody:&[crate::autoship_workflow::NativeScopeCustody],select:fn(&Judgment)->bool)->Result<Option<(String,Judgment)>> {
    ensure!(prepared.proposal.contract==*prepared.baseline.registration.contract(),"retained semantic contract changed");
    prepared.baseline.registration.verify_source_files(root,&prepared.proposal.files)?;
    ensure!(prepared.baseline.registration.baseline_result(&prepared.baseline.receipts)?==BaselineResult::Reproduced,
        "retained semantic judgment lacks its original measured baseline");
    scope_authority::require_retained_native_custody(root,custody)?;
    let mut selected:Option<(String,Judgment)>=None;
    for observation in custody {
        let Ok(judgment)=serde_json::from_str::<Judgment>(&observation.response) else {continue};
        if !select(&judgment) {continue;}
        let reply=scope_authority::NativeReply {text:observation.response.clone(),provenance:serde_json::from_str(&observation.provenance)?};
        let control=reply.provenance.artifact.parent().context("retained semantic proof has no authority directory")?;
        if scope_authority::verify_witness_reply(control,prepared.baseline.registration.baseline_head(),&judgment.context,&reply).is_err() {continue;}
        // An old policy context is data, never a current qualification. Bind
        // its original request to the COMPLETE unchanged proposal and baseline.
        let proof=private_bytes(&reply.provenance.artifact,64*1024)?;
        ensure!(amendment::digest(&proof)==reply.provenance.artifact_sha256,"retained semantic proof changed during inspection");
        let provenance:serde_json::Value=serde_json::from_slice(&proof)?;
        let request=amendment::digest(qualification_prompt(&judgment.context,prepared)?.as_bytes());
        if provenance["scope_request_hash"].as_str()!=Some(request.as_str()) {continue;}
        let judgment=validate(root,&prepared.proposal,&prepared.baseline,&judgment.context,&observation.response)?;
        let identity=amendment::digest(&serde_json::to_vec(&("retained-semantic-judgment-v1",
            &observation.input,&request,amendment::digest(observation.response.as_bytes()),&reply.provenance))?);
        ensure!(selected.as_ref().is_none_or(|(previous,_)|previous==&identity),"retained semantic judgment is ambiguous");
        selected=Some((identity,judgment));
    }
    Ok(selected)
}

/// Bind source-only inspection to this workflow's existing native authority.
/// Admission correlation alone must not import a different workflow's proof.
pub(super) fn retained_semantic_judgment_in(root:&Path,control:&Path,prepared:&PreparedAcceptance,
    custody:&[crate::autoship_workflow::NativeScopeCustody])->Result<Option<String>> {
    if custody.is_empty() {return Ok(None);}
    retained_namespace(control,custody)?;
    retained_semantic_judgment(root,prepared,custody)
}
fn retained_namespace(control:&Path,custody:&[crate::autoship_workflow::NativeScopeCustody])->Result<()> {
    let expected=control.canonicalize()?;
    for observation in custody {
        let reference:scope_authority::NativeReplyReference=serde_json::from_str(&observation.provenance)?;
        ensure!(reference.artifact.parent()==Some(expected.as_path()),"retained semantic proof belongs to another workflow");
    }
    Ok(())
}

/// Private authenticated original evidence for the existing independent authority.
/// The public failure remains a fixed safe code; no private diagnosis enters it.
pub(super) fn retained_semantic_diagnosis_in(root:&Path,control:&Path,prepared:&PreparedAcceptance,
    custody:&[crate::autoship_workflow::NativeScopeCustody])->Result<Option<(String,String)>> {
    if custody.is_empty() {return Ok(None);}
    retained_namespace(control,custody)?;
    let selected=retained_judgment_matching(root,prepared,custody,|judgment|judgment.verifiers.iter()
        .any(|witness|witness.measurement!=Measurement::DirectProduction || !witness.original_fixture_available))?;
    selected.map(|(identity,judgment)|Ok((identity,serde_json::to_string(&judgment)?))).transpose()
}

/// Source data may use a positive original witness without importing approval.
pub(super) fn retained_source_context_in(root:&Path,control:&Path,prepared:&PreparedAcceptance,
    custody:&[crate::autoship_workflow::NativeScopeCustody])->Result<Option<String>> {
    if custody.is_empty() {return Ok(None);}
    retained_namespace(control,custody)?;
    Ok(retained_judgment_matching(root,prepared,custody,|_|true)?.map(|(_,judgment)|judgment.context))
}

fn persist(control:&Path,context:&str,native_control:&Path,reply:&scope_authority::NativeReply)->Result<()> {
    let hash=amendment::digest(reply.text.as_bytes());
    immutable(control,&raw_path(control,context,&hash),reply.text.as_bytes())?;
    let artifact=Artifact {version:1,context:context.into(),response_sha256:hash,native_control:native_control.canonicalize()?,provenance:reply.provenance.clone()};
    immutable(control,&artifact_path(control,context),&serde_json::to_vec(&artifact)?)
}

pub(super) fn assert_current(root:&Path,control:&Path,prepared:&PreparedAcceptance)->Result<()> {
    let expected=context(&prepared.proposal,&prepared.baseline)?;
    prepared.baseline.registration.verify_integrity(root)?;
    let control=control.canonicalize()?;
    for writable in worker_roots(root) {if writable.exists() {ensure!(!control.starts_with(writable.canonicalize()?),"baseline qualification is writable by worker");}}
    let path=artifact_path(&control,&expected);
    if !path.try_exists()? {bail!("baseline_witness_unqualified: protected original-production qualification is missing");}
    let checked=(||->Result<Judgment> {
        let artifact:Artifact=serde_json::from_slice(&private_bytes(&path,64*1024)?)?;
        ensure!(artifact.version==1 && artifact.context==expected,"baseline witness artifact context changed");
        ensure!(artifact.native_control.is_absolute() && artifact.native_control.canonicalize()?==artifact.native_control,"native witness authority directory changed");
        ensure!(artifact.response_sha256.len()==64 && artifact.response_sha256.bytes().all(|c|c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),"invalid witness response identity");
        for writable in worker_roots(root) {if writable.exists() {ensure!(!artifact.native_control.starts_with(writable.canonicalize()?),"native qualification authority overlaps worker");}}
        let bytes=private_bytes(&raw_path(&control,&expected,&artifact.response_sha256),2*1024*1024)?;
        ensure!(amendment::digest(&bytes)==artifact.response_sha256,"original witness response changed");
        let reply=scope_authority::NativeReply {text:String::from_utf8(bytes)?,provenance:artifact.provenance};
        scope_authority::verify_witness_reply(&artifact.native_control,prepared.baseline.registration.baseline_head(),&expected,&reply)?;
        validate(root,&prepared.proposal,&prepared.baseline,&expected,&reply.text)
    })().map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))?;
    disposition(&checked)
}

fn qualification_prompt(expected:&str,prepared:&PreparedAcceptance)->Result<String> {
    Ok(format!("Independently qualify the ORIGINAL behavioral measurement, not candidate code or test success. Context: {expected}. Return only JSON {{\"version\":1,\"context\":\"{expected}\",\"verifiers\":[{{\"verifier_id\":\"exact registered id\",\"criterion_ids\":[\"every criterion assigned to this verifier\"],\"measurement\":\"direct_production\",\"original_fixture_available\":true,\"concrete_input\":\"actual fixed input supplied by pinned adapter\",\"reason\":\"why the measurement demonstrates the original production defect for the exact owner requirements\",\"production_citations\":[{{\"path\":\"original tracked source\",\"start_line\":1,\"end_line\":1,\"quote\":\"exact unnumbered source lines joined by newline\"}}]}}]}}. Cover every verifier once and every associated criterion exactly. Allowed measurement values are direct_production, missing_fixture, test_outcome, setup_failure, unknown.\nOnly direct production observations with original fixtures present may qualify. Child unittest/cargo-test/node-test assertions, discovery, zero selected tests, summaries, success tokens, candidate-added test markers, swallowed setup errors and expected answers hardcoded by the adapter are NOT original production witnesses. Existing tests do not become independent observations merely because their launcher is pinned. If the production behavior already satisfies the requirement, do not certify a marker mismatch as a defect. Boolean/JSON/text production data ARE allowed when actually measured from the original implementation. Cite original production functions that the adapter actually invokes, not new harness files. Preserve every owner requirement and the controller oracle. Unseen/insufficient evidence is unknown; do not infer missing raw details from a preview. Negative/unknown witnesses may use empty citations, but explain their concrete cause. Positive citations: 1-16 per verifier, at most 600 lines each, 512KiB total.\nThe controller validated each COMPLETE measured receipt as successful behavior data differing from its private oracle. output_digest binds the full captured output; diagnostic is ONLY an existing bounded preview, not the full observation or proof by itself. Judge the measurement's actual production path using the entire pinned harness and original source. If the preview and supplied source cannot support attribution to production, return unknown, never infer a qualified witness.\nOriginal proposal including pinned adapters and every criterion: {}\nMeasured original-baseline registration and receipts: {}",serde_json::to_string(&prepared.proposal)?,serde_json::to_string(&prepared.baseline)?))
}

pub(super) async fn qualify(root:&Path,control:&Path,prepared:&PreparedAcceptance,workflow:&workflow::WorkflowGuard)->Result<()> {
    workflow.validate_acceptance_source(prepared)?;
    let expected=context(&prepared.proposal,&prepared.baseline)?;
    std::fs::create_dir_all(control)?;
    if artifact_path(control,&expected).try_exists()? {return assert_current(root,control,prepared);}
    let retained=workflow.baseline_native_custody().map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))?;
    let native_control=control_directories(workflow.repository(),workflow.id())?.workflow_acceptance;
    if retained_semantic_judgment_in(root,&native_control,prepared,&retained)
        .map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))?.is_some() {
        return Err(BaselineNotRed {kind:Some(initial_repair::FailureKind::NotReproduced),
            detail:"baseline_witness_invalid: authenticated retained measurement is unknown; exact independent diagnosis retained privately".into()}.into());
    }
    ensure!(actual_head(root).await?==prepared.baseline.registration.baseline_head(),"qualification must use the original baseline checkout");
    prepared.baseline.registration.verify_integrity(root)?;
    ensure!(environment_id(root)?==prepared.baseline.registration.binding(prepared.baseline.registration.baseline_head())?.environment_id,
        "baseline_witness_invalid: environment changed before production witness qualification");
    let prompt=qualification_prompt(&expected,prepared)?;
    // Frozen controller policy from main 7353e9cd. This reconstructs the exact
    // original request; it grants reuse of source data, never old approval.
    let original=context_with_policy(&prepared.proposal,&prepared.baseline,"7a16a5c6b515bdf9e065f955bcc1e436de04a692e29d764bd5f89b3979e1b998")?;
    scope_authority::prepare_source_recovery(root,workflow,&expected,&prompt,&original,&qualification_prompt(&original,prepared)?).await
        .map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))?;
    let (judgment,reply)=scope_authority::persistent_witness(root,workflow,&expected,&prompt,|raw|validate(root,&prepared.proposal,&prepared.baseline,&expected,raw)).await.map_err(initial_scope_error)?;
    let native_control=control_directories(workflow.repository(),workflow.id())?.workflow_acceptance;
    persist(control,&expected,&native_control,&reply)?;
    disposition(&judgment)
}

pub(super) fn candidate_passes(root:&Path,control:&Path,prepared:&PreparedAcceptance,candidate:&[VerifierReceipt],binding:&crate::autoship_contract::EvidenceBinding)->Result<bool> {
    assert_current(root,control,prepared)?;
    prepared.baseline.registration.candidate_passes(&prepared.baseline.receipts,candidate,binding)
}

/// Script only the independent authority in disposable unit fixtures. Real
/// production qualification always uses the protected native producer above.
#[cfg(test)]
pub(super) fn persist_scripted(root:&Path,control:&Path,prepared:&PreparedAcceptance,direct:bool)->Result<()> {
    persist_scripted_measurement(root,control,prepared,if direct {Measurement::DirectProduction}else{Measurement::MissingFixture})
}
#[cfg(test)]
fn persist_scripted_measurement(root:&Path,control:&Path,prepared:&PreparedAcceptance,measurement:Measurement)->Result<()> {
    let direct=measurement==Measurement::DirectProduction;
    std::fs::create_dir_all(control)?;let control=control.canonicalize()?;
    let expected=context(&prepared.proposal,&prepared.baseline)?;
    if artifact_path(&control,&expected).try_exists()? {return assert_current(root,&control,prepared);}
    let citations=if direct {
        let base=prepared.baseline.registration.baseline_head();
        let files=original_git(root,&["ls-tree","-r","--name-only",base])?;
        let path=files.lines().find(|p|["py","rs","js","ts"].contains(&p.rsplit('.').next().unwrap_or("")) && scope_authority::witness_source_allowed(p) && !prepared.proposal.files.contains_key(*p)).context("scripted fixture has no original production source")?;
        let source=original_git(root,&["show",&format!("{base}:{path}")])?;
        vec![Citation {path:path.into(),start_line:1,end_line:1,quote:source.lines().next().context("empty fixture source")?.into()}]
    }else{vec![]};
    let judgment=Judgment {version:1,context:expected.clone(),verifiers:prepared.proposal.contract.verifiers.iter().map(|v|Witness {
        verifier_id:v.id.clone(),criterion_ids:prepared.proposal.contract.criteria.iter().filter(|c|c.verifier_ids.contains(&v.id)).map(|c|c.id.clone()).collect(),
        measurement,original_fixture_available:direct,
        concrete_input:"scripted fixture's pinned production inputs".into(),reason:if direct {"Scripted authority confirms the direct production measurement in this synthetic fixture"}else{"Original fixture is absent and the observer measures candidate-owned test markers"}.into(),production_citations:citations.clone(),
    }).collect()};
    let raw=serde_json::to_string(&judgment)?;validate(root,&prepared.proposal,&prepared.baseline,&expected,&raw)?;
    let reply=scope_authority::scripted_witness_reply(&control,prepared.baseline.registration.baseline_head(),&expected,&raw)?;
    persist(&control,&expected,&control,&reply)
}

#[cfg(test)]
pub(super) fn prepared_fixture()->(tempfile::TempDir,tempfile::TempDir,PreparedAcceptance) {tests::fixture()}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn fixture()->(tempfile::TempDir,tempfile::TempDir,PreparedAcceptance) {
        let root=tempfile::tempdir_in("/tmp").unwrap();let control=tempfile::tempdir_in("/tmp").unwrap();
        std::fs::write(root.path().join("behavior.py"),"READY = False\n").unwrap();
        for args in [vec!["init","-q"],vec!["add","."],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","baseline"],vec!["update-ref","refs/remotes/origin/main","HEAD"]] {
            let out=std::process::Command::new("/usr/bin/git").args(args).current_dir(root.path()).env_clear().env("PATH","/usr/bin:/bin").output().unwrap();assert!(out.status.success());
        }
        let base=original_git(root.path(),&["rev-parse","HEAD"]).unwrap().trim().to_owned();
        let path="autoship_verifiers/observe.py";
        let proposal=parse_proposal_for(&format!("```autoship-contract\n{}\n```",serde_json::json!({
            "contract":{"schema_version":1,"revision":1,"pull_request":840,"criteria":[{"id":"READY","requirement":"Readiness becomes true","verifier_ids":["observe"]}],"allowed_paths":[{"kind":"file","path":"behavior.py"}],
            "verifiers":[{"id":"observe","command":{"kind":"black_box","invocation":{"kind":"python_script","file":path,"args":[]},"oracle":{"kind":"json","value":true}},"pinned_files":[path],"failure_witness":"readiness is false"}]},
            "files":{path:"import json,sys\nfrom pathlib import Path\nsys.path.insert(0,str(Path.cwd()))\nimport behavior\nprint(json.dumps(behavior.READY))\n"}
        })),AcceptanceSource::PullRequest(840)).unwrap();
        install_harness(root.path(),&proposal).unwrap();
        let registration=RegisteredContract::register(root.path(),proposal.contract.clone(),base.clone(),environment_id(root.path()).unwrap()).unwrap();
        let observed=Observation {exit_code:Some(0),stdout:"false\n".into(),stderr:String::new(),interrupted:false,output_complete:true};
        let receipts=vec![registration.observe(root.path(),"observe",registration.binding(&base).unwrap(),&observed).unwrap()];
        (root,control,PreparedAcceptance {proposal,baseline:BaselineEvidence {registration,receipts}})
    }
    fn saved_judgment(control:&Path,prepared:&PreparedAcceptance)->serde_json::Value {
        let expected=context(&prepared.proposal,&prepared.baseline).unwrap();
        let artifact:Artifact=serde_json::from_slice(&private_bytes(&artifact_path(control,&expected),64*1024).unwrap()).unwrap();
        serde_json::from_slice(&private_bytes(&raw_path(control,&expected,&artifact.response_sha256),2*1024*1024).unwrap()).unwrap()
    }
    #[test]
    fn authenticated_unknown_judgment_is_repairable_without_qualifying_or_replacing_evidence() {
        let (root,control,prepared)=fixture();
        persist_scripted_measurement(root.path(),control.path(),&prepared,Measurement::Unknown).unwrap();
        let expected=context(&prepared.proposal,&prepared.baseline).unwrap();
        let artifact_bytes=private_bytes(&artifact_path(control.path(),&expected),64*1024).unwrap();
        let artifact:Artifact=serde_json::from_slice(&artifact_bytes).unwrap();
        let response=raw_path(control.path(),&expected,&artifact.response_sha256);
        let original=private_bytes(&response,2*1024*1024).unwrap();
        for _ in 0..2 {
            let error=assert_current(root.path(),control.path(),&prepared).unwrap_err();
            assert!(error.downcast_ref::<BaselineNotRed>().is_some(),"completed authenticated uncertainty must reach bounded independent repair: {error:#}");
            assert!(error.downcast_ref::<initial_repair::Hold>().is_none(),"semantic uncertainty became unavailable physical custody");
            assert!(format!("{error:#}").contains("baseline_witness_invalid"));
            assert_eq!(private_bytes(&response,2*1024*1024).unwrap(),original);
            assert_eq!(private_bytes(&artifact_path(control.path(),&expected),64*1024).unwrap(),artifact_bytes);
        }
        std::fs::remove_file(&response).unwrap();
        let lost=assert_current(root.path(),control.path(),&prepared).unwrap_err();
        assert_eq!(lost.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown),"missing physical evidence must remain an unpaid hold");
    }

    #[tokio::test]
    async fn authenticated_unknown_reaches_existing_amendment_budget_without_new_models() {
        const CHILD:&str="AUTOSHIP_UNKNOWN_AMENDMENT_FIXTURE";
        if let Some(script)=std::env::var_os(CHILD).map(PathBuf::from) {
            let (root,control,mut prepared)=fixture();
            persist_scripted_measurement(root.path(),control.path(),&prepared,Measurement::Unknown).unwrap();
            let base=prepared.baseline.registration.baseline_head().to_owned();
            std::fs::write(&script,format!("#!/bin/sh\n[ \"$1\" = api ] || exit 91\nprintf '%s\\n' '{base}'\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script,std::fs::Permissions::from_mode(0o700)).unwrap();
            let database=control.path().join("workflow.db");
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&database,workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),branch:"fixture",contract:"original owner request",environment:"fixture"}).await.unwrap().unwrap();
            let epoch=amendment::digest(&serde_json::to_vec(&(AcceptanceSource::PullRequest(840),&base,environment_id(root.path()).unwrap())).unwrap());
            for _ in 0..4 {assert!(guard.admit_epoch_attempt("verifier-amend-total",&epoch,4).unwrap());}
            let reasoner=FallbackReasoner::for_tests(vec![],augmentagent_channel_core::CooldownLatch::at(control.path().join("cooldowns.json")));
            for _ in 0..2 {
                let error=qualify_or_amend(&reasoner,AcceptanceSource::PullRequest(840),"original requirement",root.path(),control.path(),&mut prepared,&mut guard).await.unwrap_err();
                assert_eq!(error.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::Exhausted),"completed unknown never reached the existing bounded amendment authority: {error:#}");
                let state=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(guard.id()).unwrap().unwrap();
                assert_eq!(state.model_calls,0,"exhausted independent repair cannot query another model");
                assert!(guard.bind_acceptance(&prepared,control.path()).is_err(),"unknown judgment authorized a builder");
            }
            guard.finish().unwrap();return;
        }
        let scripts=tempfile::tempdir().unwrap();let script=scripts.path().join("gh");
        let out=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::acceptance::baseline_witness::tests::authenticated_unknown_reaches_existing_amendment_budget_without_new_models","--nocapture"])
            .env(CHILD,&script).env("GH_BIN",&script).env("AUGMENTAGENT_RUN_RESOURCES","1")
            .env("XDG_STATE_HOME",scripts.path().join("state")).output().unwrap();
        assert!(out.status.success(),"{}\n{}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));
    }

    #[tokio::test]
    async fn retained_unknown_reenters_existing_repair_without_requerying_witness() {
        const CHILD:&str="AUTOSHIP_RETAINED_UNKNOWN_CALLER_FIXTURE";
        if std::env::var_os(CHILD).is_some() {
            use std::os::unix::fs::PermissionsExt;
            let (root,control,prepared)=fixture();
            std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
            persist_scripted_measurement(root.path(),control.path(),&prepared,Measurement::Unknown).unwrap();
            let current=context(&prepared.proposal,&prepared.baseline).unwrap();
            let mut judgment=saved_judgment(control.path(),&prepared);
            let original="e".repeat(64);judgment["context"]=serde_json::json!(original);
            let raw=judgment.to_string();let input="a".repeat(64);
            let proof=serde_json::json!({"version":1,"head":prepared.baseline.registration.baseline_head(),"provider":"codex","native_session":"scripted-retained-caller",
                "supplied_request_hash":amendment::digest(b"scripted selected sources"),"response_hash":amendment::digest(raw.as_bytes()),
                "purpose":"baseline_witness","scope_context_hash":original,"native_input_hash":input,
                "scope_request_hash":amendment::digest(qualification_prompt(&original,&prepared).unwrap().as_bytes()),"scope_policy_hash":scope_authority::witness_policy_hash()});
            let bytes=serde_json::to_vec(&proof).unwrap();let artifact=control.path().join(format!("primary-scope-{}.json",uuid::Uuid::new_v4()));
            std::fs::write(&artifact,&bytes).unwrap();std::fs::set_permissions(&artifact,std::fs::Permissions::from_mode(0o600)).unwrap();
            
            // Only the old native response remains; there is no current-policy artifact.
            std::fs::remove_file(artifact_path(control.path(),&current)).unwrap();
            let database=control.path().join("workflow.db");
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&database,workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),branch:"fixture",contract:"original owner request",environment:"fixture"}).await.unwrap().unwrap();
            let native_control=control_directories(guard.repository(),guard.id()).unwrap().workflow_acceptance;
            std::fs::create_dir_all(&native_control).unwrap();
            std::fs::set_permissions(&native_control,std::fs::Permissions::from_mode(0o700)).unwrap();
            let retained=native_control.join(artifact.file_name().unwrap());std::fs::rename(&artifact,&retained).unwrap();
            let provenance=serde_json::to_string(&scope_authority::NativeReplyReference {artifact:retained.clone(),artifact_sha256:amendment::digest(&bytes)}).unwrap();
            let purpose=format!("{}{}",crate::autoship_workflow::BASELINE_NATIVE_PURPOSE_PREFIX,input);
            guard.bounded_native_calls(purpose.clone(),1,augmentagent_channel_core::native_session::fresh_review_scope(
                augmentagent_channel_core::ProviderKind::Codex,async {
                    let session=augmentagent_channel_core::native_session::current_for(augmentagent_channel_core::ProviderKind::Codex)?.unwrap();
                    let mut native=session.begin(augmentagent_channel_core::ProviderKind::Codex)?;
                    let receipt=guard.audit_logger().model_call_admitted("codex")?;
                    native.observe("scripted-retained-caller")?;native.finish()?;
                    Ok::<_,anyhow::Error>(receipt)
                })).await.unwrap();
            let (response_key,proof_key)=crate::autoship_workflow::baseline_native_response_keys(&purpose).unwrap();
            guard.record_scope_stage_response_pair(&response_key,&raw,&proof_key,&provenance).unwrap();
            assert!(retained_semantic_judgment(root.path(),&prepared,&guard.baseline_native_custody().unwrap()).unwrap().is_some());
            #[cfg(target_os="linux")]
            {
                verifier_execution::retain_scripted_original_data(root.path(),&native_control,&prepared,&guard).unwrap();
                for path in prepared.proposal.files.keys() {std::fs::remove_file(root.path().join(path)).unwrap();}
                let store=crate::autoship_workflow::WorkflowStore::open_read_only(&database).unwrap().unwrap();
                let before=store.snapshot(guard.id()).unwrap().unwrap();let events=store.events(guard.id()).unwrap();
                let custody=store.inspect_baseline_native_custody(guard.id()).unwrap();
                let recovered=inspect_retained_semantic(root.path(),&native_control,AcceptanceSource::PullRequest(840),guard.binding(),&custody,&store.inspect_verifier_evidence(guard.id())).unwrap();
                assert!(recovered.is_some(),"free semantic inspector cannot consume a retained unenrolled result");
                assert!(!root.path().join(prepared.proposal.files.keys().next().unwrap()).exists(),"semantic inspection installed an adapter");
                assert!(prepared.baseline.registration.verify_integrity(root.path()).is_err());
                assert_eq!(store.snapshot(guard.id()).unwrap().unwrap(),before);assert_eq!(store.events(guard.id()).unwrap(),events);
                install_harness(root.path(),&prepared.proposal).unwrap();
            }
            let before=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(guard.id()).unwrap().unwrap();
            let error=qualify(root.path(),control.path(),&prepared,&guard).await.unwrap_err();
            assert!(error.downcast_ref::<BaselineNotRed>().is_some(),"authenticated retained unknown did not enter existing repair: {error:#}");
            assert!(error.downcast_ref::<initial_repair::Hold>().is_none(),"completed old judgment became missing evidence");
            let after=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(after.model_calls,before.model_calls,"replayed a completed witness question");
            assert!(!artifact_path(control.path(),&current).exists(),"negative data became current approval");
            assert!(guard.bind_acceptance(&prepared,control.path()).is_err());
            std::fs::remove_file(&retained).unwrap();
            let lost=qualify(root.path(),control.path(),&prepared,&guard).await.unwrap_err();
            assert_eq!(lost.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown),"lost original source became semantic repair admission");
            assert_eq!(crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,before.model_calls);
            #[cfg(target_os="linux")]
            {
                guard.hold_initial_acceptance("acceptance_verifier_evidence_unknown","await_original_evidence_recovery").await.unwrap();
                let id=guard.id().to_owned();guard.finish().unwrap();
                std::fs::write(&retained,&bytes).unwrap();std::fs::set_permissions(&retained,std::fs::Permissions::from_mode(0o600)).unwrap();
                let source=AcceptanceSource::PullRequest(840);let policy=semantic_reconsideration_policy();
                let mut store=crate::autoship_workflow::WorkflowStore::open(&database).unwrap();
                let held=store.snapshot(&id).unwrap().unwrap();
                assert!(store.recover_semantic_qualification(&held,None,&policy,chrono::Utc::now().timestamp_millis(),|custody,evidence|
                    inspect_retained_semantic(root.path(),&native_control,source,&held.checkpoint.binding,custody,evidence)).unwrap());drop(store);
                let start=||workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),branch:"fixture",contract:"original owner request",environment:"fixture"};
                let mut resumed=workflow::WorkflowGuard::acquire_fixture(&database,start()).await.unwrap().unwrap();
                let accounting=resumed.capacity_accounting(true).unwrap();
                augmentagent_channel_core::run_resources::check_requirement("codex",Some(&resumed.audit_logger())).unwrap();
                std::fs::remove_file(&retained).unwrap();
                let error=augmentagent_channel_core::run_resources::check_requirement("codex",Some(&resumed.audit_logger()))
                    .expect_err("missing original source reached native launch admission");
                assert!(!resumed.audit_logger().resource_capacity_seen(),"missing source was misclassified as host capacity");
                let store=crate::autoship_workflow::WorkflowStore::open(&database).unwrap();
                let waiting=store.snapshot(&id).unwrap().unwrap();
                assert_eq!(waiting.checkpoint.stage,crate::autoship_workflow::Stage::Scope);
                assert_eq!(waiting.checkpoint.next.reason,"acceptance_verifier_evidence_unknown");
                assert_eq!(waiting.model_calls,before.model_calls);
                assert_eq!(store.events(&id).unwrap().iter().filter(|(_,kind,_,_)|kind=="semantic_source_unavailable").count(),1);drop(store);
                // Provider wrappers may replace the error type; durable source
                // wait must still prevent generic retries or initial enrollment.
                let wrapped=anyhow::anyhow!("synthetic provider wrapper around local source refusal");
                let report=hold_initial_error(&mut resumed,&wrapped).await.unwrap().expect("source hold was lost by preparation wrapper");
                assert!(report.held && !report.billed);
                assert!(resumed.checkpoint(crate::autoship_workflow::Stage::Reconcile,"generic_failure","retry",0,&[]).await.is_err(),"generic failure overwrote the source wait");
                let report=accounting.report().unwrap().expect("source wait disappeared from command accounting");
                assert!(report.held && !report.billed);let _=error;
                resumed.finish().unwrap();
                std::fs::write(&retained,&bytes).unwrap();std::fs::set_permissions(&retained,std::fs::Permissions::from_mode(0o600)).unwrap();
                let mut store=crate::autoship_workflow::WorkflowStore::open(&database).unwrap();let held=store.snapshot(&id).unwrap().unwrap();
                assert!(store.recover_semantic_qualification(&held,None,&policy,chrono::Utc::now().timestamp_millis(),|custody,evidence|
                    inspect_retained_semantic(root.path(),&native_control,source,&held.checkpoint.binding,custody,evidence)).unwrap());
                assert_eq!(store.events(&id).unwrap().iter().filter(|(_,kind,_,_)|kind=="original_semantic_witness_recovered").count(),1);drop(store);
                let mut next=workflow::WorkflowGuard::acquire_fixture(&database,start()).await.unwrap().unwrap();
                let reasoner=FallbackReasoner::for_tests(vec![],augmentagent_channel_core::CooldownLatch::at(control.path().join("cooldowns.json")));
                let mut recovered=prepare_acceptance_for(root.path(),root.path(),&native_control,source,prepared.baseline.registration.baseline_head(),"original requirement",&reasoner,&worker_roots(root.path()),&[],&next)
                    .await.expect("normal preparation rescoped instead of consuming retained source");
                assert_eq!(serde_json::to_vec(&recovered).unwrap(),serde_json::to_vec(&prepared).unwrap());
                assert!(native_control.join(format!("{}-workflow.json",source.key())).exists());
                let base=prepared.baseline.registration.baseline_head();
                let script=PathBuf::from(std::env::var_os("GH_BIN").unwrap());
                std::fs::write(&script,format!("#!/bin/sh\n[ \"$1\" = api ] || exit 91\nprintf '%s\\n' '{base}'\n")).unwrap();
                std::fs::set_permissions(&script,std::fs::Permissions::from_mode(0o700)).unwrap();
                let epoch=amendment::digest(&serde_json::to_vec(&(source,base,environment_id(root.path()).unwrap())).unwrap());
                for _ in 0..4 {assert!(next.admit_epoch_attempt("verifier-amend-total",&epoch,4).unwrap());}
                let accounting=next.capacity_accounting(true).unwrap();
                let exhausted=qualify_or_amend(&reasoner,source,"original requirement",root.path(),&native_control,&mut recovered,&mut next).await.unwrap_err();
                assert_eq!(exhausted.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::Exhausted));
                assert!(hold_qualification_error(&mut next,&recovered,&exhausted,&accounting).await.unwrap().unwrap().held);
                let state=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
                assert_eq!(state.checkpoint.binding.head,actual_head(root.path()).await.unwrap(),"exhausted hold retained the head from before controller commit");
                let expected=workflow::git_binding(root.path(),&format!("registered:{}",prepared.baseline.registration.contract_hash()),&prepared.baseline.registration.binding(base).unwrap().environment_id,&state.checkpoint.evidence).await.unwrap();
                assert_eq!(state.checkpoint.binding,expected,"qualification hold lost controller-installed inputs");
                assert_eq!(state.model_calls,before.model_calls);
                next.finish().unwrap();
                assert!(workflow::WorkflowGuard::acquire_fixture(&database,start()).await.unwrap().is_none(),"controller preparation bought an unchanged exhausted retry");
            }
            #[cfg(not(target_os="linux"))]
            guard.finish().unwrap();
            return;
        }
        let scripts=tempfile::tempdir_in("/tmp").unwrap();
        let out=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::acceptance::baseline_witness::tests::retained_unknown_reenters_existing_repair_without_requerying_witness","--nocapture"])
            .env(CHILD,"1").env("GH_BIN",scripts.path().join("gh")).env("AUGMENTAGENT_RUN_RESOURCES","1").env("CODEX_CLI","/bin/false").env("CLAUDE_CLI","/bin/false")
            .env("XDG_STATE_HOME",scripts.path().join("state")).output().unwrap();
        assert!(out.status.success(),"{}\n{}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn negative_diagnosis_covers_all_measurements_without_widening_legacy_recovery() {
        use crate::autoship_workflow::{NativeScopeCustody,submission::Provider};
        use std::os::unix::fs::PermissionsExt;
        for (measurement,original_fixture) in [(Measurement::Unknown,false),(Measurement::MissingFixture,false),
            (Measurement::TestOutcome,false),(Measurement::SetupFailure,false),
            (Measurement::DirectProduction,false),(Measurement::DirectProduction,true)] {
            let (root,control,prepared)=fixture();
            std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
            persist_scripted_measurement(root.path(),control.path(),&prepared,measurement).unwrap();
            let mut judgment=saved_judgment(control.path(),&prepared);
            for witness in judgment["verifiers"].as_array_mut().unwrap() {witness["original_fixture_available"]=original_fixture.into();}
            let context=judgment["context"].as_str().unwrap();let raw=judgment.to_string();
            let proof=serde_json::json!({"version":1,"head":prepared.baseline.registration.baseline_head(),"provider":"codex","native_session":"scripted-diagnosis-fixture",
                "supplied_request_hash":amendment::digest(b"scripted original source"),"response_hash":amendment::digest(raw.as_bytes()),
                "purpose":"baseline_witness","scope_context_hash":context,"native_input_hash":"a".repeat(64),
                "scope_request_hash":amendment::digest(qualification_prompt(context,&prepared).unwrap().as_bytes()),"scope_policy_hash":scope_authority::witness_policy_hash()});
            let bytes=serde_json::to_vec(&proof).unwrap();let artifact=control.path().join(format!("primary-scope-{}.json",uuid::Uuid::new_v4()));
            std::fs::write(&artifact,&bytes).unwrap();std::fs::set_permissions(&artifact,std::fs::Permissions::from_mode(0o600)).unwrap();
            let mut custody=[NativeScopeCustody {input:"a".repeat(64),response:raw,provider:Provider::Codex,
                provenance:serde_json::to_string(&scope_authority::NativeReplyReference {artifact,artifact_sha256:amendment::digest(&bytes)}).unwrap()}];
            let diagnosis=retained_semantic_diagnosis_in(root.path(),control.path(),&prepared,&custody).unwrap();
            let negative=measurement!=Measurement::DirectProduction || !original_fixture;
            assert_eq!(diagnosis.is_some(),negative,"authenticated {measurement:?} diagnosis omitted or positive witness misclassified");
            if let Some((identity,text))=diagnosis {assert_eq!(identity.len(),64);assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(),judgment);}
            assert_eq!(retained_semantic_judgment_in(root.path(),control.path(),&prepared,&custody).unwrap().is_some(),measurement==Measurement::Unknown,"diagnosis broadened legacy reconsideration");
            if !negative {
                assert_eq!(retained_source_context_in(root.path(),control.path(),&prepared,&custody).unwrap().as_deref(),Some(context),"positive qualified witness cannot identify its historical source projection");
            }
            let foreign=tempfile::tempdir().unwrap();assert!(retained_semantic_diagnosis_in(root.path(),foreign.path(),&prepared,&custody).is_err());
            custody[0].response.push(' ');
            assert!(retained_semantic_diagnosis_in(root.path(),control.path(),&prepared,&custody).is_err(),"changed response bypassed original custody");
        }
    }

    #[test]
    fn retained_unknown_requires_exact_original_request_and_authenticated_custody() {
        use crate::autoship_workflow::{NativeScopeCustody,submission::Provider};
        let (root,control,prepared)=fixture();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        persist_scripted_measurement(root.path(),control.path(),&prepared,Measurement::Unknown).unwrap();
        let judgment=saved_judgment(control.path(),&prepared);let context=judgment["context"].as_str().unwrap();
        let raw=judgment.to_string();
        let proof=serde_json::json!({"version":1,"head":prepared.baseline.registration.baseline_head(),"provider":"codex","native_session":"scripted-negative-source-fixture",
            "supplied_request_hash":amendment::digest(b"scripted selected sources"),"response_hash":amendment::digest(raw.as_bytes()),
            "purpose":"baseline_witness","scope_context_hash":context,"native_input_hash":"a".repeat(64),
            "scope_request_hash":amendment::digest(qualification_prompt(context,&prepared).unwrap().as_bytes()),"scope_policy_hash":scope_authority::witness_policy_hash()});
        let bytes=serde_json::to_vec(&proof).unwrap();let artifact=control.path().join(format!("primary-scope-{}.json",uuid::Uuid::new_v4()));
        std::fs::write(&artifact,&bytes).unwrap();std::fs::set_permissions(&artifact,std::fs::Permissions::from_mode(0o600)).unwrap();
        let observation=NativeScopeCustody {input:"a".repeat(64),response:raw,provider:Provider::Codex,
            provenance:serde_json::to_string(&scope_authority::NativeReplyReference {artifact:artifact.clone(),artifact_sha256:amendment::digest(&bytes)}).unwrap()};
        let custody=[observation];
        assert!(retained_semantic_judgment(root.path(),&prepared,&custody).unwrap().is_some(),"retained authenticated semantic judgment has no supported reconsideration evidence");
        assert!(retained_semantic_judgment_in(root.path(),control.path(),&prepared,&custody).unwrap().is_some());
        let (identity,diagnosis)=retained_semantic_diagnosis_in(root.path(),control.path(),&prepared,&custody).unwrap().unwrap();
        assert_eq!(Some(identity),retained_semantic_judgment(root.path(),&prepared,&custody).unwrap());
        assert_eq!(serde_json::from_str::<serde_json::Value>(&diagnosis).unwrap(),judgment);
        assert!(diagnosis.contains("Original fixture is absent and the observer measures candidate-owned test markers"));
        let foreign=tempfile::tempdir_in("/tmp").unwrap();
        assert!(retained_semantic_diagnosis_in(root.path(),foreign.path(),&prepared,&custody).is_err());
        assert!(retained_semantic_judgment_in(root.path(),foreign.path(),&prepared,&custody).is_err(),"negative source crossed workflow authority directories");
        let mut different=prepared.clone();different.proposal.contract.criteria[0].requirement="A different owner requirement".into();
        assert!(retained_semantic_judgment(root.path(),&different,&custody).is_err(),"changed original contract must not reuse judgment");
        std::fs::remove_file(artifact).unwrap();
        assert!(retained_semantic_judgment(root.path(),&prepared,&custody).is_err(),"missing physical custody cannot authorize reconsideration");
    }

    #[test]
    #[ignore = "read-only compatibility proof for explicit retained production state; no model calls or workflow mutation"]
    fn retained_semantic_judgment_production_compatibility() {
        let path=PathBuf::from(std::env::var_os("AUTOSHIP_SEMANTIC_PREPARED").expect("explicit original prepared evidence path"));
        let root=PathBuf::from(std::env::var_os("AUTOSHIP_SEMANTIC_WORKTREE").expect("explicit retained worktree"));
        let state=PathBuf::from(std::env::var_os("AUTOSHIP_SEMANTIC_STATE").expect("explicit read-only workflow store"));
        let id=std::env::var("AUTOSHIP_SEMANTIC_WORKFLOW").expect("explicit workflow id");
        let original:PreparedAcceptance=private_json(&path).unwrap();
        let prepared=retained_prepared(&root,path.parent().unwrap(),original.proposal.contract.source().unwrap()).unwrap().expect("original prepared loader could not authenticate retained evidence");
        assert_eq!(serde_json::to_vec(&prepared).unwrap(),serde_json::to_vec(&original).unwrap());
        let store=crate::autoship_workflow::WorkflowStore::open_read_only(&state).unwrap().unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();let events=store.events(&id).unwrap();
        let custody=store.inspect_baseline_native_custody(&id).unwrap();
        let identity=inspect_retained_semantic(&root,path.parent().unwrap(),prepared.proposal.contract.source().unwrap(),&before.checkpoint.binding,&custody,&store.inspect_verifier_evidence(&id)).unwrap().expect("free controller cannot authenticate retained production judgment");
        assert_eq!(identity.len(),64);
        assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);
        assert_eq!(store.events(&id).unwrap(),events);
        println!("Read-only retained semantic compatibility verified; {} original responses, no workflow mutation or model calls",custody.len());
    }

    #[test]
    fn retained_prepared_loader_reads_original_or_orphan_without_rewriting_evidence() {
        for indexed in [false,true] {
            let (root,control,prepared)=fixture();let source=AcceptanceSource::PullRequest(840);
            let file=evidence_path(control.path(),source,prepared.baseline.registration.baseline_head()).unwrap();
            immutable(control.path(),&file,&serde_json::to_vec(&prepared).unwrap()).unwrap();
            if indexed {write_manifest(control.path(),&WorkflowManifest::from_prepared(source,&prepared)).unwrap();}
            let before=std::fs::read_dir(control.path()).unwrap().map(|e|e.unwrap().file_name()).collect::<BTreeSet<_>>();
            let loaded=retained_prepared(root.path(),control.path(),source).unwrap().expect("free recovery cannot locate retained prepared original evidence");
            assert_eq!(serde_json::to_vec(&loaded).unwrap(),serde_json::to_vec(&prepared).unwrap());
            let after=std::fs::read_dir(control.path()).unwrap().map(|e|e.unwrap().file_name()).collect::<BTreeSet<_>>();
            assert_eq!(after,before,"free inspection wrote a derived manifest or another measurement");
        }
    }

    #[test]
    fn retained_prepared_loader_refuses_corrupt_cross_source_or_worker_owned_evidence() {
        for fault in ["manifest-source","manifest-contract","manifest-path","proposal","permissions","symlink","worker"] {
            let (root,control,prepared)=fixture();let source=AcceptanceSource::PullRequest(840);
            let file=evidence_path(control.path(),source,prepared.baseline.registration.baseline_head()).unwrap();
            immutable(control.path(),&file,&serde_json::to_vec(&prepared).unwrap()).unwrap();
            let mut manifest=WorkflowManifest::from_prepared(source,&prepared);
            if fault=="manifest-source" {manifest.pull_request=Some(841);}
            if fault=="manifest-contract" {manifest.contract_hash="f".repeat(64);}
            if fault=="manifest-path" {manifest.evidence_file=Some("../outside.json".into());}
            if fault=="manifest-source" {
                // Corrupt this workflow's indexed payload, rather than create
                // a valid manifest at a different source's normal filename.
                immutable(control.path(),&control.path().join(format!("{}-workflow.json",source.key())),&serde_json::to_vec(&manifest).unwrap()).unwrap();
            }else{write_manifest(control.path(),&manifest).unwrap();}
            if fault=="proposal" {
                let mut changed=prepared.clone();changed.proposal.contract.criteria[0].requirement="different owner intent".into();
                std::fs::write(&file,serde_json::to_vec(&changed).unwrap()).unwrap();
            }
            if fault=="permissions" {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&file,std::fs::Permissions::from_mode(0o640)).unwrap();}
            if fault=="symlink" {let actual=control.path().join("actual.json");std::fs::rename(&file,&actual).unwrap();std::os::unix::fs::symlink(actual,&file).unwrap();}
            let inspected=if fault=="worker" {root.path()} else {control.path()};
            assert!(retained_prepared(root.path(),inspected,source).is_err(),"{fault} was treated as recoverable original data");
        }
    }

    #[test]
    fn classifier_and_controller_rust_changes_do_not_reset_verifier_budget_environment() {
        let (root,_control,prepared)=fixture();let before=environment_id(root.path()).unwrap();
        let epoch=|environment:&str|amendment::digest(&serde_json::to_vec(&(AcceptanceSource::PullRequest(840),prepared.baseline.registration.baseline_head(),environment)).unwrap());
        let file=root.path().join("crates/augmentagent-cli/src/self_improve_baseline_witness.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file,"// changed classifier/controller source, no runner/toolchain change\n").unwrap();
        let after=environment_id(root.path()).unwrap();
        assert_eq!(after,before,"ordinary controller Rust was confused with actual verifier environment");
        assert_eq!(epoch(&after),epoch(&before),"classifier change created a fresh amendment spending epoch");
    }

    #[test]
    fn qualification_preserves_every_criterion_and_exact_original_source() {
        let (root,control,prepared)=fixture();persist_scripted(root.path(),control.path(),&prepared,true).unwrap();
        assert_current(root.path(),control.path(),&prepared).unwrap();
        let original=saved_judgment(control.path(),&prepared);let expected=context(&prepared.proposal,&prepared.baseline).unwrap();
        for fault in ["criteria","verifier","duplicate","quote","new_adapter","escape","context","claimed_approval"] {
            let mut changed=original.clone();
            match fault {
                "criteria"=>changed["verifiers"][0]["criterion_ids"]=serde_json::json!([]),
                "verifier"=>changed["verifiers"][0]["verifier_id"]="unregistered".into(),
                "duplicate"=>{let item=changed["verifiers"][0].clone();changed["verifiers"].as_array_mut().unwrap().push(item);},
                "quote"=>changed["verifiers"][0]["production_citations"][0]["quote"]="READY = True".into(),
                "new_adapter"=>changed["verifiers"][0]["production_citations"][0]["path"]="autoship_verifiers/observe.py".into(),
                "escape"=>changed["verifiers"][0]["production_citations"][0]["path"]="../../.env".into(),
                "context"=>changed["context"]="f".repeat(64).into(),
                "claimed_approval"=>changed["approved"]=true.into(),
                _=>unreachable!(),
            }
            assert!(validate(root.path(),&prepared.proposal,&prepared.baseline,&expected,&changed.to_string()).is_err(),"accepted {fault}");
        }
        for kind in ["missing_fixture","test_outcome","setup_failure","unknown"] {
            let mut changed=original.clone();changed["verifiers"][0]["measurement"]=kind.into();
            let judgment=validate(root.path(),&prepared.proposal,&prepared.baseline,&expected,&changed.to_string()).unwrap();
            assert!(disposition(&judgment).is_err(),"a {kind} became production proof");
        }
    }
    #[tokio::test]
    async fn builder_requires_qualification_and_rechecks_it_at_actual_model_admission() {
        use augmentagent_channel_core::{ProviderKind,native_session};
        let (root,control,prepared)=fixture();let database=control.path().join("workflow.db");
        let mut guard=workflow::WorkflowGuard::acquire_fixture(&database,workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),branch:"fixture",contract:"original scope",environment:"test"}).await.unwrap().unwrap();
        assert!(guard.bind_acceptance(&prepared,control.path()).is_err());
        guard.bind_acceptance_for_recovery(&prepared).unwrap();
        assert!(guard.audit_logger().model_call_admitted("codex").is_err(),"recovery metadata admitted a builder");
        native_session::fresh_review_scope(ProviderKind::Codex,async {
            let session=native_session::current_for(ProviderKind::Codex)?.unwrap();let mut native=session.begin(ProviderKind::Codex)?;
            guard.audit_logger().model_call_admitted("codex")?;native.observe("scripted-independent-repair-session")?;native.finish()?;Ok::<_,anyhow::Error>(())
        }).await.unwrap();
        persist_scripted(root.path(),control.path(),&prepared,true).unwrap();guard.bind_acceptance(&prepared,control.path()).unwrap();
        guard.audit_logger().model_call_admitted("codex").unwrap();
        let expected=context(&prepared.proposal,&prepared.baseline).unwrap();let artifact:Artifact=serde_json::from_slice(&private_bytes(&artifact_path(control.path(),&expected),64*1024).unwrap()).unwrap();
        std::fs::remove_file(raw_path(control.path(),&expected,&artifact.response_sha256)).unwrap();
        assert!(guard.audit_logger().model_call_admitted("codex").is_err(),"deleted qualification was reused from memory");
        assert_eq!(crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,2);
        guard.finish().unwrap();
    }
    #[tokio::test]
    async fn original_verifier_plan_exists_before_the_executor_can_start() {
        use rusqlite::OptionalExtension;
        let (root,control,prepared)=fixture();let database=control.path().join("workflow.db");
        let guard=workflow::WorkflowGuard::acquire_fixture(&database,workflow::Start {
            repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
            branch:"fixture",contract:"original owner requirements",environment:"fixture",
        }).await.unwrap().unwrap();
        let observed=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {
            let conn=rusqlite::Connection::open(&database).unwrap();
            let plan:Option<String>=conn.query_row("SELECT response FROM scope_stage_response WHERE json_valid(response) AND json_extract(response,'$.kind')='original_verifier_plan'",[],|r|r.get(0)).optional().unwrap();
            let plan:serde_json::Value=serde_json::from_str(&plan.expect("original verifier could start without a frozen operation recipe")).unwrap();
            assert_eq!(plan["proposal"],serde_json::to_value(&prepared.proposal).unwrap());
            assert_eq!(plan["workflow"],guard.id());
            assert_eq!(plan["base"],prepared.baseline.registration.baseline_head());
            Err::<BaselineEvidence,_>(BaselineNotRed {detail:"scripted original infrastructure failure".into(),kind:None}.into())
        }).await.unwrap_err();
        assert!(observed.downcast_ref::<BaselineNotRed>().is_some());
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn failed_original_verifier_keeps_its_exact_recovery_plan() {
        use rusqlite::OptionalExtension;
        let (root,control,prepared)=fixture();let database=control.path().join("workflow.db");
        let mut guard=workflow::WorkflowGuard::acquire_fixture(&database,workflow::Start {
            repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
            branch:"fixture",contract:"original owner requirements",environment:"fixture",
        }).await.unwrap().unwrap();
        let error=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {
            Err::<BaselineEvidence,_>(BaselineNotRed {detail:"scripted original infrastructure failure".into(),kind:None}.into())
        }).await.unwrap_err().context(initial_repair::Hold::InfrastructureUnavailable);
        let conn=rusqlite::Connection::open(&database).unwrap();
        let before:String=conn.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[guard.id()],|r|r.get(0)).unwrap();
        conn.execute_batch("CREATE TRIGGER deny_original_hold BEFORE INSERT ON workflow_event WHEN NEW.kind='original_verifier_recovery_held' BEGIN SELECT RAISE(ABORT,'recovery history unavailable'); END;").unwrap();
        assert!(hold_initial_error(&mut guard,&error).await.is_err());
        let after:String=conn.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[guard.id()],|r|r.get(0)).unwrap();
        assert_eq!(after,before,"hold committed without its exact recovery plan");
        conn.execute_batch("DROP TRIGGER deny_original_hold;").unwrap();
        hold_initial_error(&mut guard,&error).await.unwrap().unwrap();
        let plan:String=conn.query_row("SELECT key FROM scope_stage_response WHERE json_valid(response) AND json_extract(response,'$.kind')='original_verifier_plan'",[],|r|r.get(0)).unwrap();
        let held:Option<String>=conn.query_row("SELECT payload FROM workflow_event WHERE kind='original_verifier_recovery_held' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).optional().unwrap();
        let held:serde_json::Value=serde_json::from_str(&held.expect("the infrastructure hold discarded its exact original operation")).unwrap();
        assert_eq!(held["plan"],plan);assert_eq!(held["workflow"],guard.id());
        assert_eq!(held["binding"],serde_json::to_value(guard.binding()).unwrap());
        // A later semantic unknown must revoke the raw-operation recovery
        // classification even when the candidate binding has not changed.
        let semantic=anyhow::Error::from(initial_repair::Hold::EvidenceUnknown);
        hold_initial_error(&mut guard,&semantic).await.unwrap().unwrap();
        let latest:String=conn.query_row("SELECT payload FROM workflow_event WHERE kind='original_verifier_recovery_held' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).unwrap();
        let latest:serde_json::Value=serde_json::from_str(&latest).unwrap();
        assert!(latest["plan"].is_null(),"semantic unknown inherited an older raw-operation recovery plan");
        assert_eq!(latest["binding"],held["binding"]);
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn original_measurement_survives_restart_without_regeneration_or_lost_file_retry() {
        use std::cell::Cell;
        let (root,control,prepared)=fixture();let database=control.path().join("workflow.db");
        let start=||workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),branch:"fixture",contract:"original scope",environment:"test"};
        let guard=workflow::WorkflowGuard::acquire_fixture(&database,start()).await.unwrap().unwrap();
        let first=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {Ok(prepared.baseline.clone())}).await.unwrap();
        guard.finish().unwrap();
        let guard=workflow::WorkflowGuard::acquire_fixture(&database,start()).await.unwrap().unwrap();
        let reexecuted=Cell::new(false);
        let second=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {
            reexecuted.set(true);let mut noisy=prepared.baseline.clone();noisy.receipts[0].output_digest="b".repeat(64);Ok(noisy)
        }).await.unwrap();
        assert!(!reexecuted.get(),"a restart reran the original observation before native qualification");
        assert_eq!(serde_json::to_vec(&first).unwrap(),serde_json::to_vec(&second).unwrap());
        let retained=std::fs::read_dir(control.path()).unwrap().map(|e|e.unwrap().path()).find(|p|p.file_name().unwrap().to_string_lossy().starts_with("baseline-measurement-")).unwrap();
        std::fs::remove_file(retained).unwrap();
        let lost=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {
            reexecuted.set(true);Ok(prepared.baseline.clone())
        }).await.unwrap_err();
        assert_eq!(lost.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
        assert!(!reexecuted.get(),"lost original measurement was replaced with another observation");
        assert_eq!(crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        guard.finish().unwrap();
    }
    #[tokio::test]
    async fn qualification_for_another_source_cannot_authorize_a_builder() {
        let (root,control,prepared)=fixture();persist_scripted(root.path(),control.path(),&prepared,true).unwrap();
        let mut guard=workflow::WorkflowGuard::acquire_fixture(&control.path().join("workflow.db"),workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(841),branch:"fixture",contract:"original scope",environment:"test"}).await.unwrap().unwrap();
        assert!(guard.bind_acceptance(&prepared,control.path()).is_err(),"PR840's valid qualification authorized the different PR841 workflow");
        guard.finish().unwrap();
    }
    #[test]
    fn rejected_native_details_remain_private_in_public_error_surfaces() {
        let (root,control,prepared)=fixture();persist_scripted(root.path(),control.path(),&prepared,false).unwrap();
        let expected=context(&prepared.proposal,&prepared.baseline).unwrap();let mut raw=saved_judgment(control.path(),&prepared);
        raw["verifiers"][0]["reason"]="private-native-diagnostic-never-publish".into();
        let judged=validate(root.path(),&prepared.proposal,&prepared.baseline,&expected,&raw.to_string()).unwrap();
        let error=disposition(&judged).unwrap_err();
        assert!(!format!("{error:#}").contains("private-native-diagnostic-never-publish"),"private native diagnosis escaped through the general error channel");
        assert!(error.downcast_ref::<BaselineNotRed>().is_some());
    }
}
