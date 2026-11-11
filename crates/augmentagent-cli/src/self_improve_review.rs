//! Production adapter for evidence-bound review and final adjudication.
//! Loaded as a child of self_improve so existing provider/provenance checks
//! remain shared. The parent commits each candidate before invoking this path.
use super::*;
use crate::autoship_review as aligned;
use std::collections::{BTreeMap, BTreeSet};

/// Typed controller limit: never report evidence collection as provider capacity.
#[derive(Debug)]
struct EvidenceBudget(String);
impl std::fmt::Display for EvidenceBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "incomplete caller evidence: {}", self.0)
    }
}
impl std::error::Error for EvidenceBudget {}

/// Only immutable Git ancestry observations can request automatic integration.
#[derive(Debug)]
struct MainAdvanced {
    candidate: String,
    main: String,
}
impl std::fmt::Display for MainAdvanced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "main advanced beyond candidate; integrate current main before review; candidate={}; main={}", self.candidate, self.main)
    }
}
impl std::error::Error for MainAdvanced {}
/// A budget refusal keeps its meaning however many `context` layers or
/// `source()` causes the review path wrapped it in, so classify on the whole
/// chain and not the outer error (#1885: PR #1669 was held as missing reviewer
/// capacity because of one). Only the typed errors count: an unrelated error
/// whose message merely reads like a budget still requires review recovery.
/// `anyhow`'s own downcast also reaches a budget attached as a context value.
fn is_evidence_budget(error: &anyhow::Error) -> bool {
    error.is::<EvidenceBudget>()
        || error.is::<aligned::RequestBudgetExceeded>()
        || error.chain().any(|cause| cause.is::<EvidenceBudget>() || cause.is::<aligned::RequestBudgetExceeded>())
}

fn unavailable_from_error(provider: augmentagent_channel_core::ProviderKind, error: anyhow::Error) -> IndependentReview {
    let advanced = error.downcast_ref::<MainAdvanced>()
        .or_else(|| error.chain().find_map(|cause| cause.downcast_ref::<MainAdvanced>()));
    IndependentReview::unavailable(if is_evidence_budget(&error) {
        ReviewUnavailable::EvidenceBudget { detail: format!("{error:#}") }
    } else if let Some(advanced) = advanced {
        ReviewUnavailable::MainAdvanced { candidate: advanced.candidate.clone(), main: advanced.main.clone(), detail: format!("{error:#}") }
    } else {
        ReviewUnavailable::RecoveryRequired { detail: format!("aligned review by {} needs recovery: {error:#}", provider.name()) }
    })
}

#[derive(serde::Serialize,serde::Deserialize)]
struct CompletedGroup {
    binding: aligned::ReviewBinding,
    role: aligned::ReviewRole,
    original_hash: String,
    findings: Vec<aligned::Finding>,
    request_hashes: Vec<String>,
    native_sessions: Vec<String>,
    recovered_files: BTreeMap<String,String>,
    finding_evidence: BTreeMap<String,String>,
}

pub(super) struct ReviewContext<'a> {
    pub qa_ok: bool,
    pub qa_notes: &'a str,
    pub repair_round: u32,
    pub previous: Option<&'a aligned::ReviewEvidence>,
    pub environment_id: &'a str,
    /// Reserved in the protected workflow ledger BEFORE calling this adapter.
    pub adjudication_attempts_already_made: u32,
    pub workflow: Option<&'a workflow::WorkflowGuard>,
    pub acceptance_evidence: Option<(
        &'a acceptance::PreparedAcceptance,
        &'a acceptance::CandidateReceiptReference,
    )>,
}

pub(super) struct ReviewSubject<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

pub(super) async fn run(
    builder: &FallbackReasoner,
    issue: &Issue,
    worktree: PathBuf,
    criteria: &[String],
    context: ReviewContext<'_>,
) -> IndependentReview {
    run_for_subject(
        builder,
        &ReviewSubject {
            title: &issue.title,
            body: &issue.body,
        },
        worktree,
        criteria,
        context,
    )
    .await
}

pub(super) async fn run_for_subject(
    builder: &FallbackReasoner,
    issue: &ReviewSubject<'_>,
    worktree: PathBuf,
    criteria: &[String],
    context: ReviewContext<'_>,
) -> IndependentReview {
    let origin = match adoption::origin_for_candidate(&worktree, context.workflow).await {
        Ok(origin) => origin,
        Err(error) => {
            return IndependentReview::unavailable(ReviewUnavailable::ProvenanceUnverifiable {
                error: format!("{error:#}"),
            })
        }
    };
    let authors = match builder.review_authors() {
        Ok(Some(authors)) => authors,
        Ok(None) if origin.is_imported() => vec![],
        Ok(None) => return IndependentReview::unavailable(ReviewUnavailable::ProvenanceUnknown),
        Err(error) => {
            return IndependentReview::unavailable(ReviewUnavailable::ProvenanceUnverifiable {
                error: format!("{error:#}"),
            })
        }
    };
    let provider = match if origin.is_imported() {
        adoption::imported_reviewer()
    } else if origin.is_session_isolated() {
        select_session_isolated_reviewer(
            &reviewer_pool(),
            Ok(Some(authors.clone())),
            reviewer_status,
        )
    } else {
        return IndependentReview::unavailable(ReviewUnavailable::ProvenanceUnverifiable {
            error: "provider-only history requires a protected fresh-session workflow receipt"
                .into(),
        });
    } {
        Ok(provider) => provider,
        Err(why) => return IndependentReview::unavailable(why),
    };
    match run_inner(
        provider, &authors, issue, worktree, criteria, context, origin,
    )
    .await
    {
        Ok(review) => review,
        Err(error) => unavailable_from_error(provider, error),
    }
}

async fn git_output(root:&Path,args:&[&str])->Result<(bool,String,String)> {
    let output=Command::new("/usr/bin/git").arg("--no-replace-objects").args(args).current_dir(root)
        .env_clear().env("PATH","/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
        .stdin(Stdio::null()).kill_on_drop(true).output().await?;
    let stdout=String::from_utf8(output.stdout).context("review source is not UTF-8; byte-exact evidence cannot be supplied")?;
    Ok((output.status.success(),stdout,String::from_utf8_lossy(&output.stderr).into_owned()))
}
async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let (ok, stdout, stderr) = git_output(root,args).await?;
    anyhow::ensure!(ok, "git evidence command failed: {stderr}");
    Ok(stdout)
}

#[path = "self_improve_review_plan.rs"]
mod evidence_plan;
#[path="self_improve_review_source_ranges.rs"]
mod source_ranges;
#[path = "self_improve_review_group.rs"]
mod group_execution;
#[path = "self_improve_review_callers.rs"]
mod caller_stream;
#[path = "self_improve_review_adjudication.rs"]
mod final_authority;
#[path = "self_improve_adjudication_windows.rs"]
mod adjudication_windows;
#[path = "self_improve_requested_evidence.rs"]
mod requested_evidence;
#[path = "self_improve_review_symbols.rs"]
mod source_items;

const REGRESSION_REQUIREMENT: &str = "No newly demonstrated correctness, security or acceptance regression; report a reproducible example and concrete remedy.";

fn chunks(text: &str, budget: usize) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut result = vec![];
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = budget.min(rest.len());
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        assert!(end > 0, "evidence chunk budget cannot fit one UTF-8 scalar");
        result.push(&rest[..end]);
        rest = &rest[end..];
    }
    result
}

fn supplied_shards(files:&BTreeMap<String,String>,diff:&str,callers:&str,
    identities:&BTreeMap<String,serde_json::Value>)->BTreeMap<String,String> {
    let mut shards=BTreeMap::new();
    for (path,body) in files {
        let identity=identities.get(path).cloned().unwrap_or_else(||serde_json::json!({
            "kind":"rendered_source_context","path":path,"rendered_sha256":aligned::digest(body.as_bytes()),"rendered_bytes":body.len()}));
        let marker=if identity["kind"].as_str()==Some("candidate_regular_source") {"bytes"}else{"rendered-bytes"};
        let identity=serde_json::to_string(&identity).expect("serialize owned source identity");
        let mut offset=0usize;
        for chunk in chunks(body,96*1024) {
            let end=offset+chunk.len();shards.insert(format!("source:{identity}#{marker}-{offset}-{end}"),chunk.into());offset=end;
        }
    }
    for (i,body) in chunks(diff,96*1024).into_iter().enumerate() {shards.insert(format!("context:diff:part-{i}"),body.into());}
    for (i,body) in chunks(callers,96*1024).into_iter().enumerate() {shards.insert(format!("context:callers:part-{i}"),body.into());}
    shards
}

async fn snapshot_source_identity(root:&Path,head:&str,path:&str,bytes:usize)->Result<serde_json::Value> {
    let tree=git(root,&["--literal-pathspecs","ls-tree","-z",head,"--",path]).await?;
    let rows:Vec<_>=tree.split('\0').filter(|row|!row.is_empty()).collect();
    anyhow::ensure!(rows.len()==1,"candidate source identity is not one tracked object");
    let (metadata,name)=rows[0].split_once('\t').context("candidate source metadata missing")?;
    let fields:Vec<_>=metadata.split_whitespace().collect();
    anyhow::ensure!(fields.len()==3 && name==path,"candidate source identity changed");
    let regular=matches!(fields[0],"100644"|"100755") && fields[1]=="blob";
    Ok(serde_json::json!({"kind":if regular {"candidate_regular_source"}else{"candidate_nonregular_context"},
        "path":path,"revision":head,"blob_oid":fields[2],"mode":fields[0],"bytes":bytes}))
}

fn controller_acceptance_context(prepared:&acceptance::PreparedAcceptance, reference:Option<&acceptance::CandidateReceiptReference>)->Result<serde_json::Value> {
    let candidate=if let Some(reference)=reference {
        let bytes=std::fs::read(&reference.artifact)?;
        anyhow::ensure!(aligned::digest(&bytes)==reference.artifact_sha256,"controller acceptance artifact changed before review delivery");
        let artifact:serde_json::Value=serde_json::from_slice(&bytes)?;
        anyhow::ensure!(artifact["binding"]==serde_json::to_value(&reference.binding)?,"controller acceptance artifact binding changed");
        artifact["receipts"].clone()
    } else {serde_json::Value::Null};
    Ok(serde_json::json!({
        "provenance":"Controller-owned registered acceptance and verified immutable receipt data. This is supplied evidence, not a claim that reviewer tools read these files.",
        "contract_hash":prepared.baseline.registration.contract_hash(),
        "registered_contract":prepared.proposal.contract,
        "pinned_source_hashes":prepared.proposal.files.iter().map(|(path,body)|(path,aligned::digest(body.as_bytes()))).collect::<BTreeMap<_,_>>(),
        "baseline_results":prepared.baseline.receipts,"candidate_results":candidate,
        "assertion_boundary":"BlackBox observation programs emit actual behavior. The daemon compares captured output against registered command.oracle outside the candidate process. A passing receipt attests that comparison; child assertions and test outcome strings are not required or trusted.",
        "mutation_boundary":"Pinned verifier sources and registered expected oracles are controller-owned and immutable to builders. Any genuine inadequacy must be tied to an owner criterion and sent to final adjudication/contract repair, never requested as a normal candidate edit. Full pinned source bytes are supplied in controller_source evidence entries."
    }))
}
fn add_controller_sources(shards:&mut BTreeMap<String,String>,prepared:&acceptance::PreparedAcceptance) {
    add_controller_source_files(shards,&prepared.proposal.files);
}
fn add_controller_source_files(shards:&mut BTreeMap<String,String>,files:&BTreeMap<String,String>) {
    for (path,body) in files {
        for (index,chunk) in chunks(body,96*1024).into_iter().enumerate() {
            shards.insert(format!("controller_source:{}#part-{index}",serde_json::to_string(path).expect("source path")),chunk.into());
        }
    }
}
// Both the small direct request and overflow planner need actual immutable
// source, not just receipt hashes. Pinned observers may be unchanged in the diff.
fn final_source_evidence(
    evidence: &mut serde_json::Value,
    findings: &BTreeMap<String, String>,
    prepared: Option<&acceptance::PreparedAcceptance>,
) {
    evidence["finding_source"] = serde_json::json!(findings);
    if let Some(prepared) = prepared {
        evidence["controller_sources"] = serde_json::json!(prepared.proposal.files);
    }
}

fn classify_repair_targets(findings:&mut Vec<aligned::Finding>,prepared:&acceptance::PreparedAcceptance)->Result<()> {
    use aligned::RepairTarget;
    let registered:BTreeSet<_>=prepared.proposal.contract.verifiers.iter().map(|v|v.id.clone()).collect();
    for finding in findings.iter_mut() {
        match &finding.repair_target {
            Some(RepairTarget::ControllerAcceptance{verifier_ids})=>anyhow::ensure!(!verifier_ids.is_empty() && verifier_ids.is_subset(&registered),"finding names an unknown registered verifier"),
            Some(RepairTarget::Candidate{paths})=>{
                let affected:BTreeSet<_>=prepared.proposal.contract.verifiers.iter().filter(|v|v.pinned_files.iter().any(|p|paths.contains(p))).map(|v|v.id.clone()).collect();
                if !affected.is_empty() {finding.repair_target=Some(RepairTarget::ControllerAcceptance{verifier_ids:affected});}
                else {
                    let outside:BTreeSet<_>=paths.iter().filter(|p|aligned::immutable_control_path(p) || !prepared.proposal.contract.allowed_paths.iter().any(|scope|scope.contains(p))).cloned().collect();
                    if !outside.is_empty() {finding.repair_target=Some(RepairTarget::ContractScope{paths:outside});}
                }
            }
            Some(RepairTarget::ContractScope{..})=>{},
            None=>{}, // Historical unclassified findings go to final authority.
        }
    }
    let mut derived=Vec::new();
    for finding in findings.iter_mut() {
        if finding.criterion_id!="REGRESSION" {continue;}
        let owners:Vec<_>=match &finding.repair_target {
            Some(RepairTarget::ControllerAcceptance{verifier_ids})=>prepared.proposal.contract.criteria.iter().filter(|c|c.verifier_ids.iter().any(|id|verifier_ids.contains(id))).map(|c|c.id.clone()).collect(),
            Some(RepairTarget::ContractScope{..})=>prepared.proposal.contract.criteria.iter().map(|c|c.id.clone()).collect(),
            _=>continue,
        };
        anyhow::ensure!(!owners.is_empty(),"registered verifier has no owner criterion");
        let original=finding.clone();
        finding.criterion_id=owners[0].clone();
        for owner in owners.iter().skip(1) {
            let mut copy=original.clone();copy.id=format!("{}:owner:{owner}",original.id);copy.criterion_id=owner.clone();
            copy.conflicts_with.insert(finding.id.clone());finding.conflicts_with.insert(copy.id.clone());derived.push(copy);
        }
    }
    findings.extend(derived);
    Ok(())
}
fn controller_verifier_dispute(findings:&[aligned::Finding],_prepared:Option<&acceptance::PreparedAcceptance>)->bool {
    findings.iter().any(|f| !matches!(f.repair_target,Some(aligned::RepairTarget::Candidate{..})))
}

// Audit-backed recovery is independent of reviewer wording. An attempted native
// Write/Edit of a pinned source cannot become another ordinary builder repair.
async fn immutable_write_findings(root:&Path,guard:&workflow::WorkflowGuard,prepared:&acceptance::PreparedAcceptance)->Result<Vec<aligned::Finding>> {
    guard.audit_health().await?;
    let raw=std::fs::read_to_string(guard.audit_logger().path())?;
    immutable_write_findings_from_audit(root,prepared,&raw)
}
fn immutable_write_findings_from_audit(root:&Path,prepared:&acceptance::PreparedAcceptance,raw:&str)->Result<Vec<aligned::Finding>> {
    let mut attempts=BTreeMap::<String,BTreeSet<String>>::new();
    for line in raw.lines() {
        let record:serde_json::Value=serde_json::from_str(line)?;
        let tool=record["tool"].as_str().unwrap_or("").rsplit("__").next().unwrap_or("");
        if !matches!(tool,"Write"|"Edit") {continue;}
        let Some(path)=record["args"]["file_path"].as_str() else {continue;};
        let path=Path::new(path);
        let path=if path.is_absolute(){match path.strip_prefix(root){Ok(p)=>p,Err(_)=>continue}}else{path};
        if path.components().any(|p|!matches!(p,std::path::Component::Normal(_))) {continue;}
        let Some(path)=path.to_str() else {continue;};
        for verifier in &prepared.proposal.contract.verifiers {
            if verifier.pinned_files.iter().any(|p|p==path) {attempts.entry(verifier.id.clone()).or_default().insert(aligned::digest(line.as_bytes()));}
        }
    }
    let mut findings=Vec::new();
    for (verifier,records) in attempts {
        for criterion in &prepared.proposal.contract.criteria {
            if !criterion.verifier_ids.contains(&verifier) {continue;}
            let hash=aligned::digest(&serde_json::to_vec(&(prepared.baseline.registration.contract_hash(),&verifier,&records))?);
            findings.push(aligned::Finding {id:format!("controller-write-{}-{}",criterion.id,&hash[..16]),criterion_id:criterion.id.clone(),evidence_hash:hash,
                claim:format!("Protected tool audit records an attempted write/edit to immutable registered verifier {verifier}. This is an attempted operation, not proof the verifier is defective; inspect its source and oracle."),
                requested_change:"Resolve whether the owner criterion requires independent contract amendment, or dismiss the unsupported verifier-edit request. Never send immutable source changes to the candidate builder.".into(),
                conflicts_with:BTreeSet::new(),repair_rounds:0,repair_target:Some(aligned::RepairTarget::ControllerAcceptance{verifier_ids:BTreeSet::from([verifier.clone()])})});
        }
    }
    Ok(findings)
}

#[cfg(test)]
pub(super) async fn diagnostic_call_bound(worktree:&Path, issue:&ReviewSubject<'_>, prepared:&acceptance::PreparedAcceptance,reference:&acceptance::CandidateReceiptReference)->Result<u64> {
    let snapshot=candidate_snapshot(worktree).await?;
    let caller_symbols=caller_discovery_symbols(worktree,&snapshot,Some(&prepared.proposal.files)).await?;
    let (callers,caller_plan)=complete_or_streamed_callers(worktree,&snapshot.head,&caller_symbols).await?;
    let mut contract:BTreeMap<_,_>=prepared.proposal.contract.criteria.iter().map(|c|(c.id.clone(),c.requirement.clone())).collect();
    anyhow::ensure!(!contract.contains_key("REGRESSION"),"reserved criterion collision");
    contract.insert("REGRESSION".into(),REGRESSION_REQUIREMENT.into());
    // All digests are fixed-width ASCII. These placeholders have exactly the
    // same serialized size as run_impl's real environment/evidence digests.
    let binding=aligned::ReviewBinding {head:snapshot.head,base:snapshot.base,environment_id:"0".repeat(64),
        contract_hash:prepared.baseline.registration.contract_hash().into(),evidence_hash:"0".repeat(64)};
    let rubric=review_calibration::active()?.prompt();
    let system_bytes=evidence_plan::ROLES.iter().map(|role|evidence_plan::system_prompt(*role,&rubric).len()).max().unwrap();
    let subject=format!("{}\n{}",issue.title,issue.body);
    let controller=controller_acceptance_context(prepared,Some(reference))?;
    let inputs=evidence_plan::Inputs {binding:&binding,issue:&subject,contract:&contract,prior:&[],system_bytes,controller_acceptance:Some(&controller)};
    let caller_context=caller_plan.as_ref().map(|plan|plan.review_context()).unwrap_or(callers);
    let mut shards=supplied_shards(&snapshot.files,&snapshot.diff,&caller_context,&snapshot.source_identities);add_controller_sources(&mut shards,prepared);
    let requests=inputs.pack_initial(&shards,caller_plan.is_none())?;
    planned_review_calls(worktree,&inputs,&requests,caller_plan.as_ref()).await
}

async fn complete_or_streamed_callers(worktree:&Path,head:&str,symbols:&BTreeSet<String>)->Result<(String,Option<caller_stream::Plan>)> {
    match complete_caller_evidence_for_symbols(worktree,head,symbols).await {
        Ok(inline)=>Ok((inline,None)),
        Err(error) if error.downcast_ref::<EvidenceBudget>().is_some()=>{
            let plan=caller_stream::Plan::collect_symbols(worktree,head,symbols.clone()).await?;
            Ok((plan.manifest.clone(),Some(plan)))
        }
        Err(error)=>Err(error),
    }
}

async fn planned_review_calls(worktree:&Path,inputs:&evidence_plan::Inputs<'_>,
    requests:&[evidence_plan::Request],callers:Option<&caller_stream::Plan>)->Result<u64> {
    let mut groups=requests.len();
    if let Some(plan)=callers {
        let mut stream=plan.stream(worktree);
        while let Some(batch)=stream.next_batch().await? {
            groups=groups.checked_add(inputs.pack_initial(&batch,false)?.len()).context("review group count overflow")?;
        }
    }
    evidence_plan::worst_case_calls(groups)
}

struct CandidateSnapshot {
    head: String,
    base: String,
    diff: String,
    required: BTreeSet<String>,
    files: BTreeMap<String, String>,
    source_identities: BTreeMap<String,serde_json::Value>,
}

async fn caller_discovery_symbols(
    root: &Path,
    snapshot: &CandidateSnapshot,
    controller_sources: Option<&BTreeMap<String, String>>,
) -> Result<BTreeSet<String>> {
    // A rename may hide the old path in --name-only output. Inspect both
    // sides as additions/deletions so removed APIs retain their callers.
    let paths=git(root,&["diff","--no-renames","--name-only","-z",&snapshot.base,&snapshot.head,"--"]).await?;
    let changed:BTreeSet<String>=paths.split('\0').filter(|path|!path.is_empty()).map(str::to_owned).collect();
    if let Some(sources) = controller_sources {
        for (path, body) in sources {
            if !changed.contains(path) { continue; }
            anyhow::ensure!(acceptance::harness_path(path)
                && snapshot.files.get(path) == Some(body),
                "caller discovery can exclude only unchanged registered dedicated verifier source");
        }
    }
    let implementation: Vec<_> = changed.iter()
        .filter(|path| !controller_sources.is_some_and(|sources| sources.contains_key(*path)))
        .collect();
    if implementation.is_empty() { return Ok(BTreeSet::new()); }
    // Dedicated pinned harnesses are still delivered in the complete diff,
    // full files, and controller_source evidence. Their main/test entrypoints
    // do not define product APIs whose callers should be scanned repo-wide.
    // Syntax spans identify the enclosing edited item, without treating every
    // unchanged function in a module or a quoted fixture string as an API edit.
    source_items::collect(root,&snapshot.base,&snapshot.head,&implementation).await
}

async fn candidate_snapshot(worktree: &Path) -> Result<CandidateSnapshot> {
    let dirty = git(
        &worktree,
        &["status", "--porcelain", "--untracked-files=all"],
    )
    .await?;
    anyhow::ensure!(
        dirty.trim().is_empty(),
        "candidate has uncommitted changes; commit before exact-head review"
    );
    let head = git(&worktree, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_owned();
    let base = git(&worktree, &["rev-parse", "origin/main"])
        .await?
        .trim()
        .to_owned();
    let ancestor = git(&worktree, &["merge-base", &base, &head]).await?;
    if ancestor.trim() != base {
        return Err(MainAdvanced { candidate: head, main: base }.into());
    }
    let diff = git(&worktree, &["diff", "--binary", &base, &head, "--"]).await?;
    let paths = git(
        &worktree,
        &["diff", "--name-only", "-z", &base, &head, "--"],
    )
    .await?;
    let required: BTreeSet<String> = paths
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    anyhow::ensure!(!required.is_empty(), "candidate has no diff against main");
    let mut files = BTreeMap::new();
    let mut source_identities=BTreeMap::new();
    for path in &required {
        let spec = format!("{head}:{path}");
        let (exists, content, _) = git_output(&worktree, &["show", &spec]).await?;
        // Deleted files remain review evidence: the exact old bytes accompany
        // their deletion diff, without reading a working-tree symlink target.
        let content = if exists {
            content
        } else {
            let old = format!("{base}:{path}");
            format!(
                "Deleted from candidate. Prior content:\n{}",
                git(&worktree, &["show", &old]).await?
            )
        };
        let identity=if exists {snapshot_source_identity(worktree,&head,path,content.len()).await?} else {
            serde_json::json!({"kind":"rendered_deleted_context","path":path,"base":base,"head":head,
                "rendered_sha256":aligned::digest(content.as_bytes()),"rendered_bytes":content.len()})};
        source_identities.insert(path.clone(),identity);
        files.insert(path.clone(), content);
    }
    Ok(CandidateSnapshot {
        head,
        base,
        diff,
        required,
        files,
        source_identities,
    })
}

// Resolve callers against the immutable candidate, not a mutable working tree.
// This is a lexical Rust-symbol lookup, not a claim of semantic call-graph coverage.
async fn caller_paths(worktree: &Path, head: &str, diff: &str) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let removed_as_added = diff
        .lines()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .map(|line| format!("+{}", &line[1..]))
        .collect::<Vec<_>>()
        .join("\n");
    let item_context = diff.lines().filter_map(|line| {
        if let Some(context) = line.strip_prefix(' ') {
            Some(format!("+{context}"))
        } else if line.starts_with("@@") {
            line.splitn(3, "@@").nth(2).map(|context| format!("+{context}"))
        } else { None }
    }).collect::<Vec<_>>().join("\n");
    let symbols: BTreeSet<_> = changed_symbols(diff)
        .into_iter()
        .chain(changed_symbols(&removed_as_added))
        .chain(changed_symbols(&item_context))
        .collect();
    caller_paths_for_symbols(worktree,head,&symbols).await.map(|paths|(symbols,paths))
}
async fn caller_paths_for_symbols(worktree:&Path,head:&str,symbols:&BTreeSet<String>)->Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for symbol in symbols {
        // Crate-local items (see source_items::scoped_names) are searched in
        // their declaring crate only; everything else stays repository-wide.
        let (pattern, scope) = source_items::split_symbol(symbol);
        let mut args = vec!["grep", "-l", "-z", "--fixed-strings", "-e", pattern, head, "--"];
        if let Some(dir) = scope { args.push(dir); }
        let (ok, output, error) = git_output(worktree, &args).await?;
        anyhow::ensure!(
            ok || error.trim().is_empty(),
            "caller lookup failed: {error}"
        );
        for path in output.split('\0').filter(|p| !p.is_empty()) {
            let prefix = format!("{head}:");
            let path = path
                .strip_prefix(&prefix)
                .context("unexpected git caller path")?;
            paths.insert(path.to_owned());
        }
    }
    Ok(paths)
}
async fn complete_caller_evidence(worktree: &Path, head: &str, diff: &str) -> Result<String> {
    let (symbols, paths)=caller_paths(worktree,head,diff).await?;
    complete_caller_paths(worktree,head,&symbols,paths).await
}
async fn complete_caller_evidence_for_symbols(worktree:&Path,head:&str,symbols:&BTreeSet<String>)->Result<String> {
    let paths=caller_paths_for_symbols(worktree,head,symbols).await?;
    complete_caller_paths(worktree,head,symbols,paths).await
}
async fn complete_caller_paths(worktree:&Path,head:&str,symbols:&BTreeSet<String>,paths:BTreeSet<String>)->Result<String> {
    const MAX_CALLER_FILES: usize = 256;
    const MAX_CALLER_BYTES: usize = 8 * 1024 * 1024;
    if paths.len() > MAX_CALLER_FILES {
        return Err(EvidenceBudget(format!("{} files exceed {MAX_CALLER_FILES}; decompose evidence without omitting callers", paths.len())).into());
    }
    let mut files = BTreeMap::new();
    let mut total_bytes = 0usize;
    for path in paths {
        let spec=format!("{head}:{path}");
        let bytes=git(worktree,&["cat-file","-s",&spec]).await?.trim().parse::<usize>()?;
        total_bytes=total_bytes.checked_add(bytes).context("caller evidence byte count overflow")?;
        if total_bytes > MAX_CALLER_BYTES {
            return Err(EvidenceBudget(format!("source exceeds {MAX_CALLER_BYTES} bytes; decompose evidence without omitting callers")).into());
        }
        let content = git(worktree, &["show", &spec]).await?;
        anyhow::ensure!(content.len()==bytes,"immutable caller evidence size changed");
        files.insert(path, content);
    }
    Ok(serde_json::to_string(&serde_json::json!({
        "lookup": "All lexical matches for changed and removed Rust item names; not a semantic call graph",
        "symbols": symbols.iter().map(|s| match source_items::split_symbol(s) {
            (name, Some(dir)) => format!("{name} (crate-local: {dir})"),
            (name, None) => name.to_owned(),
        }).collect::<Vec<_>>(), "full_files": files
    }))?)
}

pub(super) async fn run_inner(
    provider: augmentagent_channel_core::ProviderKind,
    _authors: &[augmentagent_channel_core::ProviderKind],
    issue: &ReviewSubject<'_>,
    worktree: PathBuf,
    criteria: &[String],
    context: ReviewContext<'_>,
    origin: aligned::ReviewOrigin,
) -> Result<IndependentReview> {
    run_impl(provider, _authors, issue, worktree, criteria, context, origin,false,None).await
}

pub(super) async fn run_diagnostic_canary(
    provider: augmentagent_channel_core::ProviderKind,
    _authors: &[augmentagent_channel_core::ProviderKind],
    issue: &ReviewSubject<'_>,
    worktree: PathBuf,
    criteria: &[String],
    context: ReviewContext<'_>,
    origin: aligned::ReviewOrigin,
    observation_priority: Option<bool>,
) -> Result<IndependentReview> {
    anyhow::ensure!(provider == augmentagent_channel_core::ProviderKind::Codex,
        "diagnostic canary requires pinned native Codex");
    anyhow::ensure!(observation_priority.is_some() || cfg!(test), "production diagnostic must reserve its planned calls");
    anyhow::ensure!(context.previous.is_none(), "diagnostic must start without prior reviewer persuasion");
    let workflow = context.workflow.context("diagnostic review requires a protected workflow")?;
    anyhow::ensure!(workflow.repository().starts_with("autoship-canary/") && workflow.pr()?.is_none(),
        "diagnostic review is limited to synthetic canary workflows");
    let origin = aligned::ReviewOrigin::DiagnosticCanary { origin: Box::new(origin) };
    origin.validate(&workflow.binding().head)?;
    run_impl(provider, _authors, issue, worktree, criteria, context, origin,true,observation_priority).await
}

async fn run_impl(
    provider: augmentagent_channel_core::ProviderKind,
    _authors: &[augmentagent_channel_core::ProviderKind],
    issue: &ReviewSubject<'_>,
    worktree: PathBuf,
    criteria: &[String],
    context: ReviewContext<'_>,
    origin: aligned::ReviewOrigin,
    diagnostic: bool,
    observation_priority: Option<bool>,
) -> Result<IndependentReview> {
    anyhow::ensure!(
        !criteria.is_empty(),
        "acceptance criteria missing; scope before review"
    );
    let snapshot=candidate_snapshot(&worktree).await?;
    let caller_symbols=caller_discovery_symbols(&worktree,&snapshot,
        context.acceptance_evidence.map(|(prepared,_)|&prepared.proposal.files)).await?;
    let CandidateSnapshot {
        head,
        base,
        diff,
        required,
        files,
        source_identities,
    } = snapshot;
    let mut contract: BTreeMap<String, String> =
        if let Some((prepared, reference)) = context.acceptance_evidence {
            if diagnostic {
                acceptance::assert_verifier_receipt_current(&worktree,prepared,reference).await?;
            } else {
                acceptance::assert_receipt_current(&worktree, prepared, reference).await?;
            }
            anyhow::ensure!(
                reference.binding.head == head,
                "review acceptance head mismatch"
            );
            for criterion in criteria {
                anyhow::ensure!(
                    prepared
                        .proposal
                        .contract
                        .criteria
                        .iter()
                        .any(|registered| registered.requirement == *criterion),
                    "review cannot drop original acceptance criterion: {criterion}"
                );
            }
            prepared
                .proposal
                .contract
                .criteria
                .iter()
                .map(|criterion| (criterion.id.clone(), criterion.requirement.clone()))
                .collect()
        } else {
            anyhow::ensure!(
                context.workflow.is_none(),
                "production review requires executable acceptance evidence"
            );
            criteria
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("C{}", i + 1), c.clone()))
                .collect()
        };
    anyhow::ensure!(
        !contract.contains_key("REGRESSION"),
        "reserved reviewer criterion collides with acceptance contract"
    );
    contract.insert("REGRESSION".into(), REGRESSION_REQUIREMENT.into());
    let contract_hash = context
        .acceptance_evidence
        .map(|(_, receipt)| receipt.binding.contract_hash.clone())
        .unwrap_or(aligned::digest(&serde_json::to_vec(&contract)?));
    let (callers,caller_plan)=complete_or_streamed_callers(&worktree,&head,&caller_symbols).await?;
    let rubric = review_calibration::active()?;
    let controller_context=context.acceptance_evidence.map(|(prepared,reference)|controller_acceptance_context(prepared,Some(reference))).transpose()?;
    let guarded_attempts=if let (Some(guard),Some((prepared,_)))=(context.workflow,context.acceptance_evidence) {immutable_write_findings(&worktree,guard,prepared).await?}else{Vec::new()};
    let evidence_json = serde_json::to_string(
        &serde_json::json!({"diff":diff,"files":files,"callers":callers,"rubric":rubric,"controller_acceptance":controller_context,"guarded_immutable_write_attempts":guarded_attempts,"controller_sources":context.acceptance_evidence.map(|(prepared,_)|&prepared.proposal.files),"executable_acceptance":context.acceptance_evidence.map(|(_, receipt)| receipt)}),
    )?;
    let binding = aligned::ReviewBinding {
        head: head.clone(),
        base: base.clone(),
        environment_id: aligned::digest(&serde_json::to_vec(&(
            context.environment_id,
            context
                .acceptance_evidence
                .map(|(_, receipt)| &receipt.binding.environment_id),
        ))?),
        contract_hash,
        evidence_hash: aligned::digest(evidence_json.as_bytes()),
    };
    binding.validate()?;
    // Provider history does not identify historical native sessions. Preserve
    // it in the original provenance ledger; never invent builder Actor IDs.
    let builders = Vec::new();
    let reasoner = augmentagent_channel_core::build_workflow_pinned(provider)
        .context("native reviewer construction failed")?;
    let prior: Vec<_> = context
        .previous
        .filter(|p| p.binding.contract_hash == binding.contract_hash)
        .into_iter()
        .flat_map(|p| p.passes.iter())
        .flat_map(|p| p.findings.iter().cloned())
        .collect();
    let prior = deduplicate_findings(prior)?;
    let mut passes = vec![];
    let mut rendered = vec![];
    // Large files are fully delivered in bounded shards. Coverage is the union
    // of successful shard requests, never a model-supplied percentage.
    let caller_context=caller_plan.as_ref().map(|plan|plan.review_context()).unwrap_or_else(||callers.clone());
    let mut evidence_shards = supplied_shards(&files,&diff,&caller_context,&source_identities);
    if let Some((prepared,_))=context.acceptance_evidence {add_controller_sources(&mut evidence_shards,prepared);}
    let subject=format!("{}\n{}",issue.title,issue.body);
    let rubric_prompt=rubric.prompt();
    let system_bytes=evidence_plan::ROLES.iter().map(|role|evidence_plan::system_prompt(*role,&rubric_prompt).len()).max().unwrap();
    let inputs=evidence_plan::Inputs {binding:&binding,issue:&subject,contract:&contract,prior:&prior,system_bytes,controller_acceptance:controller_context.as_ref()};
    let requests=inputs.pack_initial(&evidence_shards,caller_plan.is_none())?;
    if let Some(allow_shipping)=observation_priority {
        let workflow=context.workflow.context("diagnostic budget requires workflow")?;
        workflow.check_resource_policy()?;
        let calls=planned_review_calls(&worktree,&inputs,&requests,caller_plan.as_ref()).await?;
        if !workflow.reserve_canary_calls(calls,allow_shipping)? {
            return Err(review_canary::CanaryDeferred.into());
        }
    }
    // Diagnostic runs prove actual transport behavior and must never borrow a
    // previous model observation from the resumable production cache.
    let cache_guard=if diagnostic {None}else{context.workflow};
    let mut recovered_files=BTreeMap::<String,String>::new();
    let mut finding_evidence=BTreeMap::<String,String>::new();
    let mut finding_source_keys=BTreeMap::<String,BTreeSet<String>>::new();
    for role in evidence_plan::ROLES {
        let session = format!("autoship-review:{role:?}:{}", uuid::Uuid::new_v4());
        let mut opts = codex_review_opts(worktree.clone(), &evidence_plan::system_prompt(role,&rubric_prompt));
        if let Some(workflow) = context.workflow {
            opts.audit_logger = Some(workflow.audit_logger());
        }
        if provider == augmentagent_channel_core::ProviderKind::Claude {
            opts.model = Some(build_model());
        }
        let model = if provider == augmentagent_channel_core::ProviderKind::Codex {
            augmentagent_channel_core::providers::model_for(
                provider,
                augmentagent_channel_core::providers::tier_of(&opts),
            )
        } else {
            opts.model.clone().context("review model missing")?
        };
        let actor = aligned::Actor {
            provider: provider.name().into(),
            backend: "native".into(),
            model,
            session_id: session.clone(),
        };
        origin.validate(&binding.head)?;
        actor.independent_for(&builders, &origin)?;
        let mut findings = vec![];
        let mut request_hashes = vec![];
        let mut native_sessions = vec![];
        let mut stream=caller_plan.as_ref().map(|plan|plan.stream(&worktree));
        let mut batch_requests=requests.clone();
        let mut group_index=0usize;
        loop {
        for original_request in &batch_requests {
            let i=group_index; group_index+=1;
            let fragment_key=aligned::digest(&serde_json::to_vec(&("review-source-ranges-v1",&binding,role,&actor.provider,&actor.model,&opts.system_prompt,&original_request.coverage.prompt_hash))?);
            let cached=cache_guard.map(|guard|guard.review_fragment(&fragment_key)).transpose()?.flatten();
            let group=if let Some(raw)=cached {
                anyhow::ensure!(raw.len()<=group_execution::MAX_COMPLETED_GROUP_BYTES,"cached review exceeds encoded bound");
                let cached:CompletedGroup=serde_json::from_str(&raw)?;
                anyhow::ensure!(cached.native_sessions.len()<=2,"cached group exceeds observation bound");
                anyhow::ensure!(cached.binding==binding && cached.original_hash==original_request.coverage.prompt_hash && cached.role==role,
                    "review fragment binding mismatch");
                for native_id in &cached.native_sessions {
                    anyhow::ensure!(!native_id.is_empty() && native_id.len()<=512 && native_id.bytes().all(|b|b.is_ascii_graphic()),"invalid cached native identity");
                    let mut actual=actor.clone(); actual.session_id=native_id.clone(); actual.independent_for(&builders,&origin)?;
                }
                anyhow::ensure!(!cached.native_sessions.is_empty(),"cached review lacks native observation");
                cached
            } else {
            let completed=group_execution::execute_evidence_group(group_execution::EvidenceGroupInput {
                root:&worktree,inputs:&inputs,original_request,evidence_shards:&evidence_shards,
                actor:&actor,builders:&builders,origin:&origin,role,provider,opts:&opts,
                group_index:i,workflow:context.workflow,
            },&group_execution::NativeGroupTransport {reasoner:reasoner.as_ref()}).await?;
            if let Some(guard)=cache_guard {
                guard.audit_health().await?;
                guard.record_review_fragment(&fragment_key,&serde_json::to_string(&completed)?)?;
            }
            completed
            };
            request_hashes.extend(group.request_hashes);
            native_sessions.extend(group.native_sessions);
            for (path,body) in group.recovered_files {
                if let Some(old)=recovered_files.insert(path,body.clone()) { anyhow::ensure!(old==body,"recovered source changed across completed groups"); }
            }
            let supplied_finding_keys:BTreeSet<_>=group.finding_evidence.keys().cloned().collect();
            for (path,body) in group.finding_evidence {
                if let Some(old)=finding_evidence.insert(path,body.clone()) { anyhow::ensure!(old==body,"finding source changed across review groups"); }
            }
            let mut shard_findings=group.findings;
            if let Some((prepared,_))=context.acceptance_evidence {classify_repair_targets(&mut shard_findings,prepared)?;}
            let known: BTreeSet<_> = prior.iter().map(|f| f.id.as_str()).collect();
            let mut remap = BTreeMap::new();
            for finding in &shard_findings {
                if !known.contains(finding.id.as_str()) {
                    remap.insert(
                        finding.id.clone(),
                        format!(
                            "finding-{}",
                            &aligned::digest(&serde_json::to_vec(&(
                                &finding.criterion_id,
                                &finding.evidence_hash,
                                &finding.claim,
                                &finding.requested_change,
                                &finding.repair_target,
                            ))?)[..20]
                        ),
                    );
                }
            }
            for finding in &mut shard_findings {
                if let Some(id) = remap.get(&finding.id) {
                    finding.id = id.clone();
                }
                finding.conflicts_with = finding
                    .conflicts_with
                    .iter()
                    .map(|id| remap.get(id).cloned().unwrap_or_else(|| id.clone()))
                    .collect();
            }
            for finding in &shard_findings {
                finding_source_keys.entry(finding.id.clone()).or_default().extend(supplied_finding_keys.clone());
            }
            findings.extend(shard_findings);
        }
        let next=match &mut stream { Some(stream)=>stream.next_batch().await?,None=>None };
        let Some(batch)=next else {break;};
        batch_requests=inputs.pack_initial(&batch,false)?;
        }
        let mut findings = deduplicate_findings(findings)?;
        for finding in &mut findings {
            finding.repair_rounds = context.repair_round;
        }
        anyhow::ensure!(
            !native_sessions.is_empty(),
            "review has no observed native conversation"
        );
        let mut actor = actor;
        actor.session_id = native_sessions.remove(0);
        let pass = aligned::ReviewPass {
            additional_sessions: native_sessions,
            binding: binding.clone(),
            actor,
            role,
            completed: true,
            coverage: aligned::CoverageReceipt {
                binding: binding.clone(),
                required_paths: required.clone(),
                supplied_files: files
                    .iter()
                    .map(|(p, b)| (p.clone(), aligned::digest(b.as_bytes())))
                    .collect(),
                prompt_hash: aligned::digest(&serde_json::to_vec(&request_hashes)?),
            },
            findings,
        };
        rendered.push(format!(
            "### {provider:?} {role:?}\n{}",
            serde_json::to_string_pretty(&pass)?
        ));
        passes.push(pass);
    }
    let mut qa_findings = if context.qa_ok {
        vec![]
    } else {
        vec![aligned::Finding {
        id:format!("qa-{}", &aligned::digest(context.qa_notes.as_bytes())[..16]), criterion_id:"REGRESSION".into(),
        evidence_hash:aligned::digest(context.qa_notes.as_bytes()), claim:context.qa_notes.into(),
        requested_change:"Resolve the concrete QA findings against this shared contract, or adjudicate conflicting scope instructions.".into(),
        conflicts_with:BTreeSet::new(), repair_target: if files.is_empty(){None}else{Some(aligned::RepairTarget::Candidate {paths:files.keys().cloned().collect()})}, repair_rounds: context.repair_round,
    }]
    };
    if let Some((prepared,_))=context.acceptance_evidence {classify_repair_targets(&mut qa_findings,prepared)?;}
    if !qa_findings.is_empty() {
        let keys:BTreeSet<_>=evidence_shards.keys().filter(|key|key.starts_with("source:")||key.starts_with("context:diff:")).cloned().collect();
        for key in &keys {finding_evidence.insert(key.clone(),evidence_shards[key].clone());}
        for finding in &qa_findings {finding_source_keys.insert(finding.id.clone(),keys.clone());}
    }
    // A previous QA rejection remains an explicit finding, even if the new
    // complete QA pass approves. Only repair evidence or adjudication resolves
    // a conflicting old instruction; it cannot evaporate during adaptation.
    if !qa_findings.is_empty() {
        passes
            .iter_mut()
            .find(|p| p.role == aligned::ReviewRole::Qa)
            .expect("QA pass collected above")
            .findings
            .extend(qa_findings);
    }
    if !guarded_attempts.is_empty() {
        passes.iter_mut().find(|pass|pass.role==aligned::ReviewRole::System).context("system review missing")?.findings.extend(guarded_attempts);
    }
    let unresolved = deduplicate_findings(
        passes
            .iter()
            .flat_map(|p| p.findings.iter().cloned())
            .collect(),
    )?;
    let mut final_recovered_contexts=BTreeMap::new();
    let supplemental_source_hashes=recovered_files.iter().map(|(p,b)|(p.clone(),aligned::digest(b.as_bytes()))).collect::<BTreeMap<_,_>>();
    let mut structured = aligned::ReviewEvidence {
        supplemental_source_hashes: supplemental_source_hashes.clone(),
        origin,
        binding: binding.clone(),
        builders,
        passes,
        adjudication: None,
    };
    if !unresolved.is_empty() && (aligned::needs_adjudication(&unresolved) || !context.qa_ok || controller_verifier_dispute(&unresolved,context.acceptance_evidence.map(|(prepared,_)|prepared))) {
        // Exact assembled-context cache keys replace broad previous-receipt
        // reuse: new selected windows or controller evidence need new authority.
            let mut opts = codex_review_opts(worktree.clone(), "Final adjudication");
            opts.session_id = None;
            if let Some(workflow) = context.workflow {
                opts.audit_logger = Some(workflow.audit_logger());
            }
            // Full source coverage was established above. Supply direct diff
            // and caller evidence plus the actual review receipts; explicitly
            // label the source digests as a manifest, not source read here.
            let caller_adjudication=if caller_plan.is_some() {
                serde_json::json!({"source_manifest_sha256":aligned::digest(callers.as_bytes()),
                    "delivery":"Every caller blob was reviewed in completed bounded groups. This digest is a manifest reference, not source read by the adjudicator. Actual evidence from groups with findings is supplied below."})
            } else {serde_json::Value::String(callers.clone())};
            let review_adjudication=if caller_plan.is_some() {
                serde_json::to_value(structured.passes.iter().map(|pass|serde_json::json!({
                    "role":pass.role,"actor":pass.actor,"completed":pass.completed,"findings":pass.findings,
                    "full_receipt_sha256":aligned::digest(&serde_json::to_vec(pass).expect("serialize pass")),
                    "reviewed_request_hash":pass.coverage.prompt_hash,"additional_native_sessions":pass.additional_sessions.len()
                })).collect::<Vec<_>>())?
            } else {serde_json::to_value(&structured.passes)?};
            let mut adjudication_evidence = serde_json::json!({
                "candidate_diff": diff,
                "controller_acceptance":controller_context,
                "caller_evidence": caller_adjudication,
                "source_manifest_only": files.iter().map(|(path,body)| (path,aligned::digest(body.as_bytes()))).collect::<BTreeMap<_,_>>(),
                "completed_reviews": review_adjudication,
                "supplemental_immutable_source": recovered_files,
            });
            final_source_evidence(&mut adjudication_evidence,&finding_evidence,context.acceptance_evidence.map(|(prepared,_)|prepared));
            let resolved = final_authority::Input {
                worktree:&worktree,guard:context.workflow,opts,review:&structured,contract:&contract,findings:&unresolved,
                evidence:adjudication_evidence,source:&finding_evidence,finding_sources:&finding_source_keys,
                attempts_already_made:context.adjudication_attempts_already_made,diagnostic,
            }.run().await?;
            structured.supplemental_source_hashes.extend(resolved.source_hashes);
            final_recovered_contexts=resolved.recovered_contexts;
            structured.adjudication=Some(resolved.receipt);
    }
    if let Some(workflow) = context.workflow {
        workflow.audit_health().await?;
    }
    // Check the immutable source again after model calls. A sibling push or
    // local edit invalidates the evidence rather than borrowing its approval.
    anyhow::ensure!(
        git(&worktree, &["rev-parse", "HEAD"]).await?.trim() == head
            && git(&worktree, &["rev-parse", "origin/main"]).await?.trim() == base
            && git(
                &worktree,
                &["status", "--porcelain", "--untracked-files=all"]
            )
            .await?
            .trim()
            .is_empty(),
        "candidate changed while reviews ran"
    );
    if let Some(guard) = context.workflow {
        let mut calibration_context=recovered_files.clone();
        calibration_context.extend(final_recovered_contexts);
        if caller_plan.is_some() {
            // Preserve outcomes but do not treat a source manifest as replayable
            // source bytes. A replay must reconstruct these exact Git blobs.
            calibration_context.insert("__streamed_caller_manifest_not_source__".into(),callers.clone());
        }
        let collected=if calibration_context.is_empty() {
            review_calibration::collect(guard,&structured,&contract,&evidence_json)
        } else {
            review_calibration::collect_supplemental_outcome(guard,&structured,&contract,&evidence_json,&calibration_context)
        };
        if let Err(error)=collected { warn!(%error,"could not collect review outcome"); }
    }
    let diff_ok = structured
        .passes
        .iter()
        .find(|p| p.role == aligned::ReviewRole::Diff)
        .is_some_and(|p| p.findings.is_empty());
    let system_ok = structured
        .passes
        .iter()
        .find(|p| p.role == aligned::ReviewRole::System)
        .is_some_and(|p| p.findings.is_empty());
    let model = structured
        .passes
        .iter()
        .find(|p| p.role == aligned::ReviewRole::Diff)
        .map(|p| p.actor.model.clone());
    if let Some(adjudication) = &structured.adjudication {
        rendered.push(format!(
            "### Final adjudication\n{}",
            serde_json::to_string_pretty(adjudication)?
        ));
    }
    Ok(IndependentReview {
        provider: Some(provider),
        model,
        available: true,
        diff_ok,
        system_ok,
        notes: rendered.join("\n\n"),
        why_unavailable: None,
        structured: Some(structured),
    })
}

fn supplemental_context_matches(previous:&BTreeMap<String,String>,current:&BTreeMap<String,String>)->bool {
    previous==current
}

fn deduplicate_findings(findings: Vec<aligned::Finding>) -> Result<Vec<aligned::Finding>> {
    let mut unique = BTreeMap::<String, aligned::Finding>::new();
    for finding in findings {
        finding.validate()?;
        if let Some(existing) = unique.get_mut(&finding.id) {
            anyhow::ensure!(
                existing.fingerprint()? == finding.fingerprint()?,
                "finding ID has conflicting evidence; request distinct IDs"
            );
            existing.conflicts_with.extend(finding.conflicts_with);
            existing.repair_rounds=existing.repair_rounds.max(finding.repair_rounds);
        } else {
            unique.insert(finding.id.clone(), finding);
        }
    }
    Ok(unique.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_git(path: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    pub(super) fn candidate_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fixture_git(path, &["init", "-q"]);
        fixture_git(path, &["config", "user.name", "Synthetic Test"]);
        fixture_git(path, &["config", "user.email", "synthetic@example.invalid"]);
        std::fs::write(path.join("source.rs"), "fn old() {}\n").unwrap();
        std::fs::write(path.join("deleted.rs"), "fn removed() {}\n").unwrap();
        fixture_git(path, &["add", "."]);
        fixture_git(path, &["commit", "-qm", "base"]);
        fixture_git(path, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(path.join("source.rs"), "fn changed() {}\n").unwrap();
        std::fs::remove_file(path.join("deleted.rs")).unwrap();
        fixture_git(path, &["add", "-A"]);
        fixture_git(path, &["commit", "-qm", "candidate"]);
        dir
    }

    #[tokio::test]
    async fn pinned_observer_entrypoint_does_not_expand_product_callers_but_edited_api_does() {
        let dir=candidate_fixture();
        let path=dir.path();
        std::fs::write(path.join("api.rs"),"pub fn resolve_person() -> u32 {\n    1\n}\n").unwrap();
        std::fs::write(path.join("caller.rs"),"fn caller() { resolve_person(); }\n").unwrap();
        std::fs::write(path.join("unrelated.txt"),"main unrelated repo metadata\n").unwrap();
        fixture_git(path,&["add","."]);fixture_git(path,&["commit","-qm","product and caller"]);
        fixture_git(path,&["update-ref","refs/remotes/origin/main","HEAD"]);
        std::fs::write(path.join("api.rs"),"pub fn resolve_person() -> u32 {\n    2\n}\n").unwrap();
        let pin="src/bin/autoship_observer.rs";
        std::fs::create_dir_all(path.join("src/bin")).unwrap();
        std::fs::write(path.join(pin),"fn main() { resolve_person(); }\n").unwrap();
        fixture_git(path,&["add","."]);fixture_git(path,&["commit","-qm","body edit and pinned observer"]);
        let snapshot=candidate_snapshot(path).await.unwrap();
        let pins=BTreeMap::from([(pin.into(),snapshot.files[pin].clone())]);
        let symbols=caller_discovery_symbols(path,&snapshot,Some(&pins)).await.unwrap();
        let paths=caller_paths_for_symbols(path,&snapshot.head,&symbols).await.unwrap();
        assert!(!symbols.contains("main"),"a controller observer entrypoint is not a changed product API");
        assert!(symbols.contains("resolve_person"),"body-only edits must collect the existing API's callers");
        assert!(paths.contains("caller.rs"));
        assert!(!paths.contains("unrelated.txt"));
        assert!(snapshot.diff.contains("+fn main()"));
        assert_eq!(snapshot.files[pin],pins[pin],"full review source still includes immutable observer bytes");
        let mut changed_pin=pins.clone();changed_pin.insert(pin.into(),"changed source".into());
        assert!(caller_discovery_symbols(path,&snapshot,Some(&changed_pin)).await.is_err());
        let unregistered=BTreeMap::from([("api.rs".into(),snapshot.files["api.rs"].clone())]);
        assert!(caller_discovery_symbols(path,&snapshot,Some(&unregistered)).await.is_err(),
            "a product file cannot be excluded by claiming it is pinned verifier source");
    }

    #[test]
    fn incomplete_group_is_review_recovery_not_provider_capacity() {
        for depth in 0..3 {
            let mut group = evidence_plan::GroupReview::default();
            assert!(!group.observe(false, vec![]).unwrap());
            let mut error = group.finish().unwrap_err();
            for layer in 0..depth {
                error = error.context(format!("private review context {layer}"));
            }
            let review = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex, error);
            let why = review.why_unavailable.as_ref().unwrap();
            assert_eq!(why.code(), "review-recovery-required");
            assert!(!why.permanent());
            assert!(!why.headline().contains("capacity"));
            assert!(!why.headline().contains("private review context"));
            assert!(why.log_line().contains("incomplete group cannot authorize coverage"));
            let plan = unreviewable_plan(71000, 70994, why, 1, false);
            assert!(!plan.billed && plan.stand_down.is_none());
            let comment = plan.wait_comment.unwrap();
            assert!(comment.contains("No approval or code rejection"));
            assert!(!comment.contains("reasoner-selftest"));
        }
    }

    #[test]
    fn exhausted_group_is_review_recovery_not_provider_capacity() {
        let mut group = evidence_plan::GroupReview::default();
        assert!(!group.observe(false, vec![]).unwrap());
        let error = group.observe(false, vec![]).unwrap_err();
        let review = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex, error);
        assert_eq!(review.why_unavailable.unwrap().code(), "review-recovery-required");
    }

    #[test]
    fn oversized_review_wire_is_evidence_budget_not_provider_capacity() {
        let binding=aligned::ReviewBinding {head:"a".repeat(40),base:"b".repeat(40),
            environment_id:"environment".into(),contract_hash:"c".repeat(64),evidence_hash:"d".repeat(64)};
        let files=BTreeMap::from([("large.rs".into(),"x".repeat(aligned::MAX_PROMPT_BYTES))]);
        let error=aligned::review_request(&binding,"fixture",&BTreeMap::from([("C1".into(),"correct".into())]),"diff",files.keys().cloned().collect(),&files,&[]).unwrap_err();
        let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
        assert!(matches!(review.why_unavailable,Some(ReviewUnavailable::EvidenceBudget {..})),
            "a deterministic request-size refusal cannot be reported as a missing provider");
    }

    #[test]
    fn context_wrapped_budget_errors_stay_evidence_budget() {
        let budgets:[fn()->anyhow::Error;2]=[
            ||anyhow::Error::new(aligned::RequestBudgetExceeded),
            ||anyhow::Error::new(EvidenceBudget("caller evidence".into()))];
        for budget in budgets {
            for depth in 0..3 {
                let mut error=budget();
                for layer in 0..depth {error=error.context(format!("review layer {layer}"));}
                let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
                let why=review.why_unavailable.as_ref().unwrap();
                assert_eq!(why.code(),"review-evidence-budget",
                    "a budget refusal wrapped in {depth} context layers is not missing reviewer capacity");
                let ReviewUnavailable::EvidenceBudget {detail}=why else {unreachable!()};
                for layer in 0..depth {assert!(detail.contains(&format!("review layer {layer}")),"the reported detail keeps every context layer");}
            }
        }
        for depth in 0..3 {
            let mut error=anyhow::anyhow!("provider down");
            for layer in 0..depth {error=error.context(format!("review layer {layer}"));}
            let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
            assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-recovery-required",
                "an unrelated reviewer failure wrapped in {depth} context layers is not an evidence budget");
        }
    }

    /// Reports its cause through `source` rather than an `anyhow` context
    /// layer, which is the chain level `anyhow` downcasting cannot reach.
    #[derive(Debug)]
    struct SourcedFailure(Box<dyn std::error::Error+Send+Sync+'static>);
    impl std::fmt::Display for SourcedFailure {
        fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.write_str("review group failed") }
    }
    impl std::error::Error for SourcedFailure {
        fn source(&self)->Option<&(dyn std::error::Error+'static)> { Some(&*self.0) }
    }

    #[test]
    fn source_chained_budget_errors_stay_evidence_budget() {
        // The budget is the typed `source()` of another error. An `anyhow`
        // error boxed straight into a plain `dyn Error` no longer exposes its
        // type, and matching its message instead is what QA refused on #1893.
        type Boxed=Box<dyn std::error::Error+Send+Sync>;
        let budgets:[fn()->Boxed;2]=[
            ||Box::new(aligned::RequestBudgetExceeded),
            ||Box::new(EvidenceBudget("caller evidence".into()))];
        for budget in budgets {
            let error=anyhow::Error::new(SourcedFailure(budget())).context("review group");
            let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
            assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-evidence-budget",
                "a budget refusal reported as the source of a review failure is not missing reviewer capacity");
        }
        let error=anyhow::Error::new(SourcedFailure(anyhow::anyhow!("provider down").into()));
        let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
        assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-recovery-required",
            "an unrelated reviewer failure is not an evidence budget whatever reports it");
    }

    /// Alternates the two kinds of wrapping the review path produces, so each
    /// budget type is classified from every level it can occupy in the chain.
    fn wrap_review_failure(error:anyhow::Error,level:usize)->anyhow::Error {
        if level%2==0 {error.context(format!("review layer {level}"))}
        else {anyhow::Error::new(SourcedFailure(error.into())).context(format!("review group {level}"))}
    }

    #[test]
    fn every_budget_type_is_recognized_at_every_chain_level() {
        let budgets:[fn()->anyhow::Error;2]=[
            ||anyhow::Error::new(aligned::RequestBudgetExceeded),
            ||anyhow::Error::new(EvidenceBudget("caller evidence".into()))];
        for depth in 0..5 {
            for budget in budgets {
                let mut error=budget();
                for level in 0..depth {error=wrap_review_failure(error,level);}
                let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
                assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-evidence-budget",
                    "a budget refusal {depth} chain levels down is not missing reviewer capacity");
            }
            let mut error=anyhow::anyhow!("provider down");
            for level in 0..depth {error=wrap_review_failure(error,level);}
            let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
            assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-recovery-required",
                "an unrelated reviewer failure {depth} chain levels down is not an evidence budget");
        }
    }

    #[test]
    fn budget_carried_as_a_context_value_stays_evidence_budget() {
        let budgets:[fn()->anyhow::Error;2]=[
            ||anyhow::anyhow!("serialized review request refused").context(aligned::RequestBudgetExceeded),
            ||anyhow::anyhow!("inline caller evidence refused").context(EvidenceBudget("bounded caller inventory".into()))];
        for budget in budgets {
            let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,budget().context("review evidence"));
            assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-evidence-budget",
                "a budget carried as an anyhow context value is not missing reviewer capacity");
        }
    }

    /// Text that reads like a budget is not a budget: only the typed errors
    /// are evidence budgets, so a provider failure that happens to render the
    /// same message still requires review recovery without claiming a budget.
    #[test]
    fn budget_lookalike_messages_require_review_recovery() {
        let lookalikes=[
            EvidenceBudget("caller evidence".into()).to_string(),
            aligned::RequestBudgetExceeded.to_string()];
        for text in lookalikes {
            for error in [
                anyhow::anyhow!(text.clone()),
                anyhow::anyhow!("provider down").context(text.clone()),
                anyhow::Error::new(SourcedFailure(anyhow::anyhow!(text.clone()).into())).context("review group"),
            ] {
                let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error);
                assert_eq!(review.why_unavailable.as_ref().unwrap().code(),"review-recovery-required",
                    "an untyped error that only reads like {text:?} is not an evidence budget");
            }
        }
    }

    #[tokio::test]
    async fn rename_does_not_hide_removed_rust_items_from_caller_lookup() {
        let dir=candidate_fixture();let root=dir.path();
        let common="// retained source context\n".repeat(50);
        std::fs::write(root.join("original.rs"),format!("{common}pub fn kept() {{}}\npub fn removed_api() {{}}\n")).unwrap();
        std::fs::write(root.join("consumer.rs"),"fn caller() { removed_api(); }\n").unwrap();
        fixture_git(root,&["add","."]);fixture_git(root,&["commit","-qm","rename baseline"]);
        fixture_git(root,&["update-ref","refs/remotes/origin/main","HEAD"]);
        fixture_git(root,&["mv","original.rs","renamed.rs"]);
        std::fs::write(root.join("renamed.rs"),format!("{common}pub fn kept() {{}}\n")).unwrap();
        fixture_git(root,&["add","."]);fixture_git(root,&["commit","-qm","move and remove an API"]);
        let snapshot=candidate_snapshot(root).await.unwrap();
        assert_eq!(snapshot.required,BTreeSet::from(["renamed.rs".into()]),"fixture must exercise Git rename detection");
        let symbols=caller_discovery_symbols(root,&snapshot,None).await.unwrap();
        assert!(symbols.contains("removed_api"),"removed API must be discovered in the old path, not only the new path");
        assert!(caller_paths_for_symbols(root,&snapshot.head,&symbols).await.unwrap().contains("consumer.rs"));
    }

    #[tokio::test]
    #[ignore = "read-only packing replay requires an existing candidate and privately captured bound request; no native dispatch"]
    async fn replay_readonly_caller_plan_from_bound_candidate() {
        let root=PathBuf::from(std::env::var("AUTOSHIP_REPLAY_REPOSITORY").unwrap());
        let input:serde_json::Value=serde_json::from_slice(&std::fs::read(std::env::var("AUTOSHIP_REPLAY_REQUEST").unwrap()).unwrap()).unwrap();
        let binding:aligned::ReviewBinding=serde_json::from_value(input["binding"].clone()).unwrap();binding.validate().unwrap();
        let paths=git(&root,&["diff","--name-only","-z",&binding.base,&binding.head,"--"]).await.unwrap();
        let required:BTreeSet<String>=paths.split('\0').filter(|path|!path.is_empty()).map(str::to_owned).collect();
        let mut files=BTreeMap::new();
        for path in &required {
            let (exists,body,_)=git_output(&root,&["show",&format!("{}:{path}",binding.head)]).await.unwrap();
            assert!(exists,"this replay expects the published candidate's changed sources");files.insert(path.clone(),body);
        }
        let snapshot=CandidateSnapshot {head:binding.head.clone(),base:binding.base.clone(),
            diff:git(&root,&["diff","--binary",&binding.base,&binding.head,"--"]).await.unwrap(),required,files,source_identities:BTreeMap::new()};
        let mut pins=BTreeMap::new();
        for (path,sha) in input["controller_acceptance"]["pinned_source_hashes"].as_object().unwrap() {
            let body=git(&root,&["show",&format!("{}:{path}",binding.head)]).await.unwrap();
            assert_eq!(aligned::digest(body.as_bytes()),sha.as_str().unwrap());pins.insert(path.clone(),body);
        }
        let symbols=caller_discovery_symbols(&root,&snapshot,Some(&pins)).await.unwrap();
        let callers=complete_caller_evidence_for_symbols(&root,&binding.head,&symbols).await.unwrap();
        let caller_data:serde_json::Value=serde_json::from_str(&callers).unwrap();
        let mut shards=supplied_shards(&snapshot.files,&snapshot.diff,&callers,&snapshot.source_identities);add_controller_source_files(&mut shards,&pins);
        let contract:BTreeMap<String,String>=serde_json::from_value(input["acceptance_criteria"].clone()).unwrap();
        let prior:Vec<aligned::Finding>=serde_json::from_value(input["unresolved_findings"].clone()).unwrap();
        // The original request was near 512 KiB including its actual system
        // budget. A conservative 32 KiB system allowance tests extra headroom.
        let inputs=evidence_plan::Inputs {binding:&binding,issue:input["issue"].as_str().unwrap(),contract:&contract,
            prior:&prior,system_bytes:32*1024,controller_acceptance:Some(&input["controller_acceptance"])};
        let requests=inputs.pack(&shards).unwrap();assert_eq!(requests.len(),1,"the published small change should have one coherent evidence group");
        let result=serde_json::json!({"head":binding.head,"base":binding.base,"shipping_authority":false,
            "symbols":symbols,"caller_files":caller_data["full_files"].as_object().unwrap().len(),
            "caller_bytes":caller_data["full_files"].as_object().unwrap().values().map(|body|body.as_str().unwrap().len()).sum::<usize>(),
            "request_bytes":requests[0].prompt.len(),"system_allowance_bytes":inputs.system_bytes,
            "limit_bytes":aligned::MAX_PROMPT_BYTES,
            "review_source_sha256":aligned::digest(include_bytes!("self_improve_review.rs")),
            "item_parser_source_sha256":aligned::digest(include_bytes!("self_improve_review_symbols.rs"))});
        let output=std::env::var("AUTOSHIP_REPLAY_PROOF").unwrap();std::fs::write(output,serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        println!("{result}");
    }

    #[tokio::test]
    async fn snapshot_binds_exact_committed_files_and_deleted_source_to_main() {
        let dir = candidate_fixture();
        let snapshot = candidate_snapshot(dir.path()).await.unwrap();
        assert_eq!(
            snapshot.head,
            fixture_git(dir.path(), &["rev-parse", "HEAD"])
        );
        assert_eq!(
            snapshot.base,
            fixture_git(dir.path(), &["rev-parse", "origin/main"])
        );
        assert_eq!(snapshot.files["source.rs"], "fn changed() {}\n");
        assert!(snapshot.files["deleted.rs"].contains("fn removed() {}"));
        assert_eq!(snapshot.required, snapshot.files.keys().cloned().collect());
        assert!(snapshot.diff.contains("-fn old() {}"));
    }

    #[tokio::test]
    async fn snapshot_missing_committed_blob_is_incomplete_not_approval_evidence() {
        let dir = candidate_fixture();
        let blob = fixture_git(dir.path(), &["rev-parse", "HEAD:source.rs"]);
        std::fs::remove_file(
            dir.path()
                .join(".git/objects")
                .join(&blob[..2])
                .join(&blob[2..]),
        )
        .unwrap();
        assert!(
            candidate_snapshot(dir.path()).await.is_err(),
            "missing source bytes cannot be represented by a coverage digest"
        );
    }

    #[tokio::test]
    async fn snapshot_rejects_untracked_staged_and_unstaged_candidate_code() {
        let dir = candidate_fixture();
        std::fs::write(dir.path().join("untracked.rs"), "fn hidden() {}\n").unwrap();
        assert!(candidate_snapshot(dir.path()).await.is_err());
        fixture_git(dir.path(), &["add", "untracked.rs"]);
        assert!(candidate_snapshot(dir.path()).await.is_err());
        fixture_git(dir.path(), &["reset", "--hard", "HEAD"]);
        std::fs::write(dir.path().join("source.rs"), "fn hidden() {}\n").unwrap();
        assert!(candidate_snapshot(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn snapshot_rejects_advanced_main_instead_of_reviewing_its_reverse_diff() {
        let dir = candidate_fixture();
        let candidate = fixture_git(dir.path(), &["rev-parse", "HEAD"]);
        fixture_git(dir.path(), &["checkout", "--detach", "origin/main"]);
        std::fs::write(dir.path().join("unrelated.rs"), "fn sibling() {}\n").unwrap();
        fixture_git(dir.path(), &["add", "."]);
        fixture_git(dir.path(), &["commit", "-qm", "sibling ships"]);
        fixture_git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        fixture_git(dir.path(), &["checkout", "--detach", &candidate]);
        let err = candidate_snapshot(dir.path()).await.err().unwrap();
        assert!(err.to_string().contains("integrate current main"));
    }

    #[tokio::test]
    async fn review_unavailability_checkpoint_retains_typed_cause_without_private_details() {
        use crate::autoship_workflow::{WorkflowStore, Stage};
        let dir=candidate_fixture();
        let control=tempfile::Builder::new().prefix("review-cause-control-").tempdir_in("/tmp").unwrap();
        let state=control.path().join("workflow.db");
        let mut guard=workflow::WorkflowGuard::acquire_fixture(&state,workflow::Start {
            repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(7),
            pr:Some(42),branch:"fix/fixture",contract:"original owner contract",environment:"fixture",
        }).await.unwrap().unwrap();
        guard.checkpoint(Stage::Review,"verified","review",0,&["original-pin".into()]).await.unwrap();
        let id=guard.id().to_owned();let store=WorkflowStore::open(&state).unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();
        let why=ReviewUnavailable::RecoveryRequired {detail:"private reviewer response".into()};
        checkpoint_unavailable_review(&mut guard,&why,chrono::Utc::now().timestamp_millis()).await.unwrap();
        let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage,Stage::Held);
        assert_eq!(after.checkpoint.next.reason,why.code(),"typed review cause was lost at the producer");
        assert_eq!(after.checkpoint.next.action,"resume_preserved_work");
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert_eq!(after.model_calls,before.model_calls);
        assert!(!serde_json::to_string(&store.progress_events(&id).unwrap()).unwrap().contains("private reviewer"));
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn main_advance_is_typed_through_context_and_source_wrapping() {
        for depth in 0..3 {
            let dir = candidate_fixture();
            let candidate = fixture_git(dir.path(), &["rev-parse", "HEAD"]);
            fixture_git(dir.path(), &["checkout", "--detach", "origin/main"]);
            std::fs::write(dir.path().join("new-main.rs"), "fn shipped() {}\n").unwrap();
            fixture_git(dir.path(), &["add", "."]);
            fixture_git(dir.path(), &["commit", "-qm", "main advances"]);
            let main = fixture_git(dir.path(), &["rev-parse", "HEAD"]);
            fixture_git(dir.path(), &["update-ref", "refs/remotes/origin/main", &main]);
            fixture_git(dir.path(), &["checkout", "--detach", &candidate]);
            let mut error = candidate_snapshot(dir.path()).await.err().unwrap();
            for layer in 0..depth { error = wrap_review_failure(error, layer); }
            let review = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex, error);
            let why = review.why_unavailable.unwrap();
            assert_eq!(why.code(), "review-main-advanced");
            assert!(!why.permanent());
            assert!(why.log_line().contains(&candidate) && why.log_line().contains(&main));
            assert!(!why.headline().contains(&candidate));
            let bounded = candidate_snapshot(dir.path()).await.err().unwrap()
                .context(EvidenceBudget("outer evidence limit".into()));
            let denial = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex, bounded);
            assert_eq!(denial.why_unavailable.unwrap().code(), "review-evidence-budget",
                "main integration cannot override an independently typed evidence limit");
        }
        let lookalike = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,
            anyhow::anyhow!("main advanced beyond candidate; integrate current main before review"));
        assert_eq!(lookalike.why_unavailable.unwrap().code(), "review-recovery-required");
    }

    #[tokio::test]
    async fn main_advance_queues_original_workflow_without_resetting_work_or_charges() {
        use crate::autoship_workflow::{WorkflowStore, ResourceCharge, Stage};
        use crate::autoship_workflow::submission::{Request, Target, Milestone};
        for command in [false, true] {
            let dir = candidate_fixture();
            let candidate = fixture_git(dir.path(), &["rev-parse", "HEAD"]);
            fixture_git(dir.path(), &["checkout", "--detach", "origin/main"]);
            std::fs::write(dir.path().join("new-main.rs"), "fn shipped() {}\n").unwrap();
            fixture_git(dir.path(), &["add", "."]);
            fixture_git(dir.path(), &["commit", "-qm", "main advances"]);
            fixture_git(dir.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
            fixture_git(dir.path(), &["checkout", "--detach", &candidate]);
            let error = candidate_snapshot(dir.path()).await.err().unwrap();
            let why = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex, error)
                .why_unavailable.unwrap();
            let control = tempfile::Builder::new().prefix("main-advance-control-").tempdir_in("/tmp").unwrap();
            let state = control.path().join("workflow.db");
            let mut guard = workflow::WorkflowGuard::acquire_fixture(&state, workflow::Start {
                repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo", source_issue: Some(7),
                pr: if command { Some(42) } else { None }, branch: "fix/fixture", contract: "original owner contract", environment: "fixture",
            }).await.unwrap().unwrap();
            guard.checkpoint(Stage::Review, "verified", "review", 0, &["original-immutable-pin".into()]).await.unwrap();
            let id = guard.id().to_owned();
            let now = chrono::Utc::now().timestamp_millis();
            let mut store = WorkflowStore::open(&state).unwrap();
            let lease = guard.merge_lease().unwrap();
            store.attach_pr_in_repo(&lease, "owner/repo", 42, now).unwrap();
            assert_eq!(id, if command { "owner/repo/pr/42" } else { "owner/repo#7" });
            store.charge(&lease, "original-paid-work", ResourceCharge {
                model_calls: 7, tokens: 42, wall_ms: 10,
            }, now).unwrap();
            if command {
                store.submit_authorized("original-command", "github:owner", &Request {
                    repository: "owner/repo".into(), target: Target::Pr(42), provider: None, until: Milestone::Merged,
                }, now).unwrap();
            }
            let before = store.snapshot(&id).unwrap().unwrap();
            let allowance = serde_json::to_value(store.merge_allowance(now, 5).unwrap()).unwrap();
            checkpoint_unavailable_review(&mut guard, &why, now).await.unwrap();
            let after = store.snapshot(&id).unwrap().unwrap();
            assert_eq!(after.checkpoint.stage, Stage::Reconcile, "a changed main must not require another owner resume");
            assert_eq!(after.checkpoint.next.reason, "review_main_changed");
            assert_eq!(after.checkpoint.next.action, "reconcile_main");
            assert_eq!(after.checkpoint.next.eligible_at_ms, now + 300_000);
            assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
            assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
            assert_eq!((after.model_calls, after.tokens), (7, 42));
            assert_eq!(serde_json::to_value(store.merge_allowance(now, 5).unwrap()).unwrap(), allowance);
            guard.finish().unwrap();
            if command {
                assert!(!store.command_queue_due("owner/repo", now + 299_999).unwrap());
                assert!(store.command_queue_due("owner/repo", now + 300_000).unwrap());
            } else { assert!(!store.has_command_jobs().unwrap()); }
            assert_eq!(fixture_git(dir.path(), &["rev-parse", "HEAD"]), candidate);
            assert!(!store.has_due_main_reconciliation(None, now + 299_999).unwrap());
            assert!(store.has_due_main_reconciliation(None, now + 300_000).unwrap());
            assert!(store.has_due_main_reconciliation(Some("owner/repo"), now + 300_000).unwrap());
            assert!(!store.has_due_main_reconciliation(Some("owner/repo_extra"), now + 300_000).unwrap());
            let pr = adoption::AdoptedPr { repository: "owner/repo".into(), pr: 42, source_issue: Some(7),
                branch: "fix/fixture".into(), head: candidate.clone(), base: "main".into(), author: "owner".into(),
                title: "fixture".into(), body: "original owner contract".into(), dependency: adoption::Dependency::Main };
            let candidates = [pr];
            assert!(adoption::episode_retries(&state, &candidates, now + 299_999).unwrap().is_empty());
            let retries = adoption::episode_retries(&state, &candidates, now + 300_000).unwrap();
            assert!(retries.contains(&42), "automatic source-issue selection must recognize this dependency wake");
            let mut ledger = AttemptLedger::default();
            ledger.mark(1, 7);
            assert!(adoption::select_linked_with_retries(&candidates, &ledger, 1, &Default::default(), None, 0, &Default::default()).is_none());
            assert_eq!(adoption::select_linked_with_retries(&candidates, &ledger, 1, &Default::default(), None, 0, &retries).unwrap().0, 42);
            store.set_owner_pause(&id, "github:owner", 0, true, now).unwrap();
            assert!(adoption::episode_retries(&state, &candidates, now + 300_000).unwrap().is_empty());
            assert!(!store.has_due_main_reconciliation(None, now + 300_000).unwrap());
            store.set_owner_pause(&id, "github:owner", 1, false, now).unwrap();
            let lease = store.claim_productive(&id, "later-worker", now + 300_000, 100_000).unwrap().unwrap();
            let mut unrelated = store.snapshot(&id).unwrap().unwrap().checkpoint;
            unrelated.next.reason = "resume_started".into();
            store.checkpoint(&lease, &unrelated, now + 300_000).unwrap();
            store.release(&lease, now + 300_000).unwrap();
            assert!(adoption::episode_retries(&state, &candidates, now + 300_000).unwrap().is_empty(),
                "a generic reconcile action cannot bypass the same-day paid-attempt fence");
            assert!(!store.has_due_main_reconciliation(None, now + 300_000).unwrap());
        }
    }

    #[tokio::test]
    async fn main_advance_does_not_bypass_owner_lease_publication_or_candidate_fences() {
        use crate::autoship_workflow::{WorkflowStore, ResourceCharge, Stage, OperationKind};
        for case in ["owner-pause", "stale-lease", "unknown-merge", "stale-candidate"] {
            let dir = candidate_fixture();
            let candidate = fixture_git(dir.path(), &["rev-parse", "HEAD"]);
            fixture_git(dir.path(), &["checkout", "--detach", "origin/main"]);
            std::fs::write(dir.path().join("new-main.rs"), "fn shipped() {}\n").unwrap();
            fixture_git(dir.path(), &["add", "."]);
            fixture_git(dir.path(), &["commit", "-qm", "main advances"]);
            fixture_git(dir.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
            fixture_git(dir.path(), &["checkout", "--detach", &candidate]);
            let error = candidate_snapshot(dir.path()).await.err().unwrap();
            let why = unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex, error)
                .why_unavailable.unwrap();
            if case == "stale-candidate" {
                std::fs::write(dir.path().join("later.rs"), "fn preserved_later_work() {}\n").unwrap();
                fixture_git(dir.path(), &["add", "."]);
                fixture_git(dir.path(), &["commit", "-qm", "candidate advances after observation"]);
            }
            let expected_head = fixture_git(dir.path(), &["rev-parse", "HEAD"]);
            let control = tempfile::Builder::new().prefix("main-advance-fences-").tempdir_in("/tmp").unwrap();
            let state = control.path().join("workflow.db");
            let mut guard = workflow::WorkflowGuard::acquire_fixture(&state, workflow::Start {
                repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo", source_issue: Some(7),
                pr: Some(42), branch: "fix/fixture", contract: "original contract", environment: "fixture",
            }).await.unwrap().unwrap();
            guard.transition(Stage::Review, "verified", "review", 0).await.unwrap();
            let id = guard.id().to_owned();
            let lease = guard.merge_lease().unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            let mut store = WorkflowStore::open(&state).unwrap();
            store.charge(&lease, "original-work", ResourceCharge {model_calls: 3, tokens: 5, wall_ms: 10}, now).unwrap();
            match case {
                "owner-pause" => { store.set_owner_pause(&id, "github:owner", 0, true, now).unwrap(); }
                "stale-lease" => {
                    store.release(&lease, now).unwrap();
                    store.claim_reconciliation(&id, "new-owner", now + 1, 100_000).unwrap().unwrap();
                }
                "unknown-merge" => { guard.intent(OperationKind::MergePr).unwrap(); }
                _ => {}
            }
            let before = store.snapshot(&id).unwrap().unwrap();
            assert!(checkpoint_unavailable_review(&mut guard, &why, now).await.is_err(), "{case}");
            let after = store.snapshot(&id).unwrap().unwrap();
            assert_eq!(after.checkpoint, before.checkpoint, "{case}");
            assert_eq!((after.model_calls, after.tokens), (3, 5), "{case}");
            assert_eq!(fixture_git(dir.path(), &["rev-parse", "HEAD"]), expected_head, "{case}");
            if case == "owner-pause" { assert!(store.owner_control(&id).unwrap().unwrap().paused); }
            if case == "unknown-merge" { assert_eq!(store.pending_operations(&id).unwrap().len(), 1); }
        }
    }

    #[tokio::test]
    async fn caller_evidence_preserves_all_matches_and_full_files_beyond_old_hit_cap() {
        let dir = candidate_fixture();
        for i in 0..20 {
            std::fs::write(
                dir.path().join(format!("caller-{i}.rs")),
                format!("// context {i}\nfn caller() {{ changed(); }}\n// tail {i}\n"),
            )
            .unwrap();
        }
        fixture_git(dir.path(), &["add", "."]);
        fixture_git(dir.path(), &["commit", "-qm", "callers"]);
        let snapshot = candidate_snapshot(dir.path()).await.unwrap();
        let raw = complete_caller_evidence(dir.path(), &snapshot.head, &snapshot.diff)
            .await
            .unwrap();
        let evidence: serde_json::Value = serde_json::from_str(&raw).unwrap();
        for i in 0..20 {
            assert!(evidence["full_files"][format!("caller-{i}.rs")]
                .as_str()
                .unwrap()
                .contains(&format!("// tail {i}")));
        }
    }

    #[tokio::test]
    async fn original_source_and_callers_reject_lossy_or_replaced_evidence() {
        let dir=candidate_fixture();
        std::fs::write(dir.path().join("source.rs"),[255u8]).unwrap();
        fixture_git(dir.path(),&["add","source.rs"]);fixture_git(dir.path(),&["commit","-qm","nonutf8 source"]);
        assert!(candidate_snapshot(dir.path()).await.is_err());
        std::fs::write(dir.path().join("source.rs"),"fn changed() {}\n").unwrap();
        std::fs::write(dir.path().join("caller.rs"),b"fn use_changed() { changed(); }\n\xff").unwrap();
        fixture_git(dir.path(),&["add","."]);fixture_git(dir.path(),&["commit","-qm","nonutf8 caller"]);
        let head=fixture_git(dir.path(),&["rev-parse","HEAD"]);
        assert!(complete_caller_evidence(dir.path(),&head,"+fn changed() {}").await.is_err());
        std::fs::remove_file(dir.path().join("caller.rs")).unwrap();
        fixture_git(dir.path(),&["add","-A"]);fixture_git(dir.path(),&["commit","-qm","valid source"]);
        let original=fixture_git(dir.path(),&["rev-parse","HEAD:source.rs"]);
        std::fs::write(dir.path().join("replacement"),"poisoned replacement source\n").unwrap();
        let replacement=fixture_git(dir.path(),&["hash-object","-w","replacement"]);
        fixture_git(dir.path(),&["replace",&original,&replacement]);
        std::fs::remove_file(dir.path().join("replacement")).unwrap();
        assert_eq!(git(dir.path(),&["show","HEAD:source.rs"]).await.unwrap(),"fn changed() {}\n");
    }
    #[tokio::test]
    async fn common_symbol_caller_expansion_is_bounded_before_review() {
        let dir=candidate_fixture();
        for n in 0..257 {std::fs::write(dir.path().join(format!("caller{n}.rs")),"fn caller() { changed(); }\n").unwrap();}
        fixture_git(dir.path(),&["add","."]);fixture_git(dir.path(),&["commit","-qm","large lexical caller set"]);
        let head=fixture_git(dir.path(),&["rev-parse","HEAD"]);
        let error=complete_caller_evidence(dir.path(),&head,"+fn changed() {}").await.unwrap_err();
        assert!(error.to_string().contains("incomplete caller evidence"));
        assert!(error.to_string().contains("files exceed"));
        let review=unavailable_from_error(augmentagent_channel_core::ProviderKind::Codex,error.context("source collection"));
        assert!(!review.available && !review.approved());
        let why=review.why_unavailable.as_ref().unwrap();
        assert!(matches!(why,ReviewUnavailable::EvidenceBudget { .. }));
        assert_eq!(why.code(),"review-evidence-budget");
        assert!(!why.permanent());
        assert!(review.structured.is_none());
    }
    #[test]
    fn registered_oracle_and_bound_receipts_are_delivered_in_every_review_group() {
        use crate::autoship_contract::{RegisteredContract,VerifierReceipt,CheckResult};
        let root=candidate_fixture();let pin="autoship_verifiers/observe.py";
        std::fs::create_dir(root.path().join("autoship_verifiers")).unwrap();
        let script="import json\nprint(json.dumps(actual_behavior()))\n";
        std::fs::write(root.path().join(pin),script).unwrap();
        let proposal:acceptance::ContractProposal=serde_json::from_value(serde_json::json!({
            "contract":{"schema_version":1,"revision":1,"issue":42,"criteria":[{"id":"C1","requirement":"Return exactly 77","verifier_ids":["oracle"]}],
                "allowed_paths":[{"kind":"file","path":"source.rs"}],"verifiers":[{"id":"oracle","command":{"kind":"black_box","invocation":{"kind":"python_script","file":pin,"args":[]},"oracle":{"kind":"json","value":77}},"pinned_files":[pin],"failure_witness":"actual differs from expected"}]},
            "files":{pin:script}
        })).unwrap();
        let head=fixture_git(root.path(),&["rev-parse","HEAD"]);
        let registration=RegisteredContract::register(root.path(),proposal.contract.clone(),head.clone(),"fixture-environment".into()).unwrap();
        let binding=registration.binding(&head).unwrap();
        let red=VerifierReceipt {verifier_id:"oracle".into(),binding:binding.clone(),result:CheckResult::IntendedFailure,output_digest:"a".repeat(64),diagnostic:"synthetic baseline observation".into()};
        let green=VerifierReceipt {result:CheckResult::Passed,diagnostic:String::new(),..red.clone()};
        let artifact=root.path().join("fixture-controller-receipt.json");
        let bytes=serde_json::to_vec(&serde_json::json!({"schema_version":1,"binding":binding,"receipts":[green]})).unwrap();
        std::fs::write(&artifact,&bytes).unwrap();
        let reference=acceptance::CandidateReceiptReference {schema_version:1,binding,artifact:artifact.clone(),artifact_sha256:aligned::digest(&bytes),receipt_ids:vec!["oracle".into()],alignment:None};
        let prepared=acceptance::PreparedAcceptance {proposal,baseline:acceptance::BaselineEvidence {registration,receipts:vec![red]}};
        let controller=controller_acceptance_context(&prepared,Some(&reference)).unwrap();
        let attempt=serde_json::json!({"tool":"mcp__jarvis__Write","args":{"file_path":root.path().join(pin),"content":"private-never-report"},"stderr_truncated":"denied"}).to_string();
        let recovered=immutable_write_findings_from_audit(root.path(),&prepared,&attempt).unwrap();
        assert_eq!(recovered.len(),1);
        assert!(controller_verifier_dispute(&recovered,Some(&prepared)));
        assert!(!serde_json::to_string(&recovered).unwrap().contains("private-never-report"));
        let ordinary=serde_json::json!({"tool":"Write","args":{"file_path":"arithmetic.py"}}).to_string();
        assert!(immutable_write_findings_from_audit(root.path(),&prepared,&ordinary).unwrap().is_empty());

        // Non-streamed, unchanged pinned observer: the diff contains no source
        // and there are no streamed caller groups. Final authority's small
        // fast path must still receive actual bytes, not only pin digests.
        let mut final_evidence=serde_json::json!({"candidate_diff":"","controller_acceptance":controller});
        let finding_sources=BTreeMap::from([("source:implementation".into(),"actual_behavior = 77".into())]);
        final_source_evidence(&mut final_evidence,&finding_sources,Some(&prepared));
        assert_eq!(final_evidence["controller_sources"][pin],script);
        assert_eq!(final_evidence["finding_source"]["source:implementation"],"actual_behavior = 77");

        assert_eq!(controller["registered_contract"]["verifiers"][0]["command"]["oracle"]["value"],77);
        assert_eq!(controller["candidate_results"][0]["result"],"passed");
        assert_eq!(controller["pinned_source_hashes"][pin],aligned::digest(script.as_bytes()));
        let review_binding=aligned::ReviewBinding {head,base:"b".repeat(40),contract_hash:reference.binding.contract_hash.clone(),environment_id:"fixture-environment".into(),evidence_hash:aligned::digest(&bytes)};
        let contract=BTreeMap::from([("C1".into(),"Return exactly 77".into())]);
        let direct_bytes=aligned::adjudication_prompt_bytes(&review_binding,&contract,&[],&serde_json::to_string(&final_evidence).unwrap()).unwrap();
        assert!(direct_bytes<aligned::MAX_PROMPT_BYTES,"fixture exercises direct final authority, not overflow source recovery");
        let inputs=evidence_plan::Inputs {binding:&review_binding,issue:"behavior",contract:&contract,prior:&[],system_bytes:1024,controller_acceptance:Some(&controller)};
        let mut shards=BTreeMap::from([("source:large".into(),"x".repeat(aligned::MAX_PROMPT_BYTES))]);
        add_controller_sources(&mut shards,&prepared);
        assert!(shards.values().any(|source|source==script));
        let requests=inputs.pack(&shards).unwrap();assert!(requests.len()>1);
        for request in requests {
            let value:serde_json::Value=serde_json::from_str(&request.prompt).unwrap();
            assert_eq!(value["controller_acceptance"],controller);
            assert!(request.prompt.len()+inputs.system_bytes<=aligned::MAX_PROMPT_BYTES);
        }
        let mut finding=aligned::Finding {id:"F1".into(),criterion_id:"C1".into(),evidence_hash:"proof".into(),claim:"observer lacks child assertions".into(),requested_change:format!("add assertions to {pin}"),conflicts_with:BTreeSet::new(),repair_target: Some(crate::autoship_review::RepairTarget::Candidate { paths: BTreeSet::from(["source.rs".into()]) }), repair_rounds: 0};
        finding.repair_target=Some(aligned::RepairTarget::Candidate{paths:BTreeSet::from([pin.into()])});
        finding.criterion_id="REGRESSION".into();
        let mut classified=vec![finding];classify_repair_targets(&mut classified,&prepared).unwrap();
        let mut finding=classified.pop().unwrap();assert_eq!(finding.criterion_id,"C1","controller defects map to their registered owner criterion, which final authority can amend");
        assert!(controller_verifier_dispute(&[finding.clone()],Some(&prepared)),"controller disputes go to final authority on their first round");
        let mut scope=finding.clone();scope.criterion_id="REGRESSION".into();scope.repair_target=Some(aligned::RepairTarget::Candidate{paths:BTreeSet::from(["new_support.py".into()])});
        let mut scoped=vec![scope];classify_repair_targets(&mut scoped,&prepared).unwrap();
        assert_eq!(scoped[0].criterion_id,"C1");
        assert!(matches!(&scoped[0].repair_target,Some(aligned::RepairTarget::ContractScope{paths}) if paths.contains("new_support.py")));
        assert!(controller_verifier_dispute(&scoped,Some(&prepared)));
        finding.claim="incorrect sum".into();finding.requested_change="fix arithmetic.py".into();
        finding.repair_target=Some(aligned::RepairTarget::Candidate{paths:BTreeSet::from(["arithmetic.py".into()])});
        assert!(!controller_verifier_dispute(&[finding],Some(&prepared)));
        std::fs::write(&artifact,b"tampered").unwrap();
        assert!(controller_acceptance_context(&prepared,Some(&reference)).is_err());
    }

    #[test]
    fn new_supplemental_context_invalidates_old_dismissal_reuse() {
        let previous=BTreeMap::new();
        let current=BTreeMap::from([("support.rs".into(),aligned::digest(b"newly supplied source"))]);
        assert!(!supplemental_context_matches(&previous,&current));
        assert!(!supplemental_context_matches(&current,&previous));
        assert!(supplemental_context_matches(&current,&current));
        let changed=BTreeMap::from([("support.rs".into(),aligned::digest(b"changed source"))]);
        assert!(!supplemental_context_matches(&current,&changed));
    }

    #[test]
    fn evidence_chunks_preserve_every_unicode_byte() {
        let text = "source αβγ\n".repeat(100);
        let pieces = chunks(&text, 17);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|p| p.len() <= 17));
    }

    #[test]
    fn identical_finding_ids_cannot_hide_new_evidence() {
        let first = aligned::Finding {
            id: "F1".into(),
            criterion_id: "C1".into(),
            evidence_hash: "proof1".into(),
            claim: "observable regression".into(),
            requested_change: "make fixture pass".into(),
            conflicts_with: BTreeSet::new(),
            repair_target: Some(crate::autoship_review::RepairTarget::Candidate { paths: BTreeSet::from(["source.rs".into()]) }), repair_rounds: 0,
        };
        assert_eq!(
            deduplicate_findings(vec![first.clone(), first.clone()])
                .unwrap()
                .len(),
            1
        );
        let mut linked=first.clone();linked.conflicts_with.insert("F2".into());linked.repair_rounds=2;
        let merged=deduplicate_findings(vec![first.clone(),linked]).unwrap();
        assert!(merged[0].conflicts_with.contains("F2"));assert_eq!(merged[0].repair_rounds,2);
        let mut changed = first.clone();
        changed.evidence_hash = "proof2".into();
        assert!(deduplicate_findings(vec![first, changed]).is_err());
    }
}
