//! Production adapter for evidence-bound review and final adjudication.
//! Loaded as a child of self_improve so existing provider/provenance checks
//! remain shared. The parent commits each candidate before invoking this path.
use super::*;
use crate::autoship_review as aligned;
use std::collections::{BTreeMap, BTreeSet};

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
        Err(error) => IndependentReview::unavailable(ReviewUnavailable::NoCapacity {
            reviewers: vec![provider],
            detail: format!("aligned review needs recovery: {error:#}"),
        }),
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

fn supplied_shards(files:&BTreeMap<String,String>,diff:&str,callers:&str)->BTreeMap<String,String> {
    let mut evidence_shards = BTreeMap::new();
    for (path, body) in files {
        for (i, chunk) in chunks(body, 96 * 1024).into_iter().enumerate() {
            evidence_shards.insert(format!("source:{}#part-{i}",serde_json::to_string(path).expect("serialize source path")), chunk.to_owned());
        }
    }
    for (i, chunk) in chunks(diff, 96 * 1024).into_iter().enumerate() {
        evidence_shards.insert(format!("context:diff:part-{i}"), chunk.to_owned());
    }
    for (i, chunk) in chunks(callers, 96 * 1024).into_iter().enumerate() {
        evidence_shards.insert(format!("context:callers:part-{i}"), chunk.to_owned());
    }
    evidence_shards
}
pub(super) async fn diagnostic_call_bound(worktree:&Path, issue:&ReviewSubject<'_>, prepared:&acceptance::PreparedAcceptance)->Result<u64> {
    let snapshot=candidate_snapshot(worktree).await?;
    let callers=complete_caller_evidence(worktree,&snapshot.head,&snapshot.diff).await?;
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
    let inputs=evidence_plan::Inputs {binding:&binding,issue:&subject,contract:&contract,prior:&[],system_bytes};
    let requests=inputs.pack(&supplied_shards(&snapshot.files,&snapshot.diff,&callers))?;
    evidence_plan::worst_case_calls(requests.len())
}

struct CandidateSnapshot {
    head: String,
    base: String,
    diff: String,
    required: BTreeSet<String>,
    files: BTreeMap<String, String>,
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
    anyhow::ensure!(
        ancestor.trim() == base,
        "main advanced beyond candidate; integrate current main before review"
    );
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
        files.insert(path.clone(), content);
    }
    Ok(CandidateSnapshot {
        head,
        base,
        diff,
        required,
        files,
    })
}

// Resolve callers against the immutable candidate, not a mutable working tree.
// This is a lexical Rust-symbol lookup, not a claim of semantic call-graph coverage.
async fn complete_caller_evidence(worktree: &Path, head: &str, diff: &str) -> Result<String> {
    let removed_as_added = diff
        .lines()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .map(|line| format!("+{}", &line[1..]))
        .collect::<Vec<_>>()
        .join("\n");
    let symbols: BTreeSet<_> = changed_symbols(diff)
        .into_iter()
        .chain(changed_symbols(&removed_as_added))
        .collect();
    let mut paths = BTreeSet::new();
    for symbol in &symbols {
        let (ok, output, error) = git_output(
            worktree,
            &[
                "grep",
                "-l",
                "-z",
                "--fixed-strings",
                "-e",
                symbol,
                head,
                "--",
            ],
        )
        .await?;
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
    let mut files = BTreeMap::new();
    for path in paths {
        let content = git(worktree, &["show", &format!("{head}:{path}")]).await?;
        files.insert(path, content);
    }
    Ok(serde_json::to_string(&serde_json::json!({
        "lookup": "All lexical matches for changed and removed Rust item names; not a semantic call graph",
        "symbols": symbols, "full_files": files
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
    let CandidateSnapshot {
        head,
        base,
        diff,
        required,
        files,
    } = candidate_snapshot(&worktree).await?;
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
    let callers = complete_caller_evidence(&worktree, &head, &diff).await?;
    let rubric = review_calibration::active()?;
    let evidence_json = serde_json::to_string(
        &serde_json::json!({"diff":diff,"files":files,"callers":callers,"rubric":rubric,"executable_acceptance":context.acceptance_evidence.map(|(_, receipt)| receipt)}),
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
    let reasoner = augmentagent_channel_core::build_pinned(provider)
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
    let evidence_shards = supplied_shards(&files,&diff,&callers);
    let subject=format!("{}\n{}",issue.title,issue.body);
    let rubric_prompt=rubric.prompt();
    let system_bytes=evidence_plan::ROLES.iter().map(|role|evidence_plan::system_prompt(*role,&rubric_prompt).len()).max().unwrap();
    let inputs=evidence_plan::Inputs {binding:&binding,issue:&subject,contract:&contract,prior:&prior,system_bytes};
    let requests=inputs.pack(&evidence_shards)?;
    if let Some(allow_shipping)=observation_priority {
        let workflow=context.workflow.context("diagnostic budget requires workflow")?;
        workflow.check_resource_policy()?;
        if !workflow.reserve_canary_calls(evidence_plan::worst_case_calls(requests.len())?,allow_shipping)? {
            return Err(review_canary::CanaryDeferred.into());
        }
    }
    let mut recovered_files=BTreeMap::<String,String>::new();
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
        for (i, original_request) in requests.iter().enumerate() {
            let mut request=original_request.clone();
            let mut group_review=evidence_plan::GroupReview::default();
            for recovery_attempt in 0..=1 {
                opts.session_id = Some(format!("{session}:group-{i}:recovery-{recovery_attempt}"));
                opts.restrict_env = true;
                anyhow::ensure!(git(&worktree,&["rev-parse","HEAD"]).await?.trim()==head,"candidate changed before evidence recovery");
                let (raw,native_id)=augmentagent_channel_core::native_session::fresh_review_scope(provider,
                    augmentagent_channel_core::model_router::native_reviewer_scope(provider,reasoner.call(&opts,&request.prompt))).await?;
                let mut shard_actor=actor.clone();shard_actor.session_id=native_id.clone();
                shard_actor.independent_for(&builders,&origin)?;
                native_sessions.push(native_id);
                let (pass,missing)=aligned::parse_review_pass_with_missing(&raw,&binding,shard_actor,role,request.coverage.clone(),&contract)?;
                request_hashes.push(pass.coverage.prompt_hash.clone());
                if group_review.observe(pass.completed,pass.findings).with_context(||format!("evidence group {i} still needs {missing:?}"))? { break; }
                request=evidence_plan::recover(&inputs,&worktree,&head,original_request,&evidence_shards,&missing,group_review.pending()).await?;
                if let Some(workflow)=context.workflow {
                    workflow.audit_logger().record_controller(serde_json::json!({"actor":"controller","kind":"review_evidence_recovery",
                        "binding":binding,"group":i,"role":role,"request_hash":request.coverage.prompt_hash,
                        "supplied_evidence":request.coverage.supplied_files,"missing_paths":missing}))?;
                }
                for (path,body) in &request.files {
                    if path.starts_with("__requested_source__:") {
                        if let Some(old)=recovered_files.insert(path.clone(),body.clone()) {
                            anyhow::ensure!(old==*body,"same immutable recovery path has different bytes");
                        }
                    }
                }
            }
            let mut shard_findings=group_review.finish()?;
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
            findings.extend(shard_findings);
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
    let qa_findings = if context.qa_ok {
        vec![]
    } else {
        vec![aligned::Finding {
        id:format!("qa-{}", &aligned::digest(context.qa_notes.as_bytes())[..16]), criterion_id:"REGRESSION".into(),
        evidence_hash:aligned::digest(context.qa_notes.as_bytes()), claim:context.qa_notes.into(),
        requested_change:"Resolve the concrete QA findings against this shared contract, or adjudicate conflicting scope instructions.".into(),
        conflicts_with:BTreeSet::new(), repair_rounds:context.repair_round,
    }]
    };
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
    let unresolved = deduplicate_findings(
        passes
            .iter()
            .flat_map(|p| p.findings.iter().cloned())
            .collect(),
    )?;
    let supplemental_source_hashes=recovered_files.iter().map(|(p,b)|(p.clone(),aligned::digest(b.as_bytes()))).collect::<BTreeMap<_,_>>();
    let mut structured = aligned::ReviewEvidence {
        supplemental_source_hashes: supplemental_source_hashes.clone(),
        origin,
        binding: binding.clone(),
        builders,
        passes,
        adjudication: None,
    };
    if !unresolved.is_empty() && (aligned::needs_adjudication(&unresolved) || !context.qa_ok) {
        let reusable = context
            .previous
            .filter(|p| supplemental_context_matches(&p.supplemental_source_hashes,&supplemental_source_hashes))
            .and_then(|p| p.adjudication.as_ref())
            .filter(|receipt| receipt.binding == binding)
            .filter(|receipt| {
                unresolved.iter().all(|finding| {
                    finding.fingerprint().ok().is_some_and(|hash| {
                        receipt.finding_fingerprints.get(&finding.id) == Some(&hash)
                    })
                })
            });
        if let Some(receipt) = reusable {
            // The same finding/evidence cannot demand a fresh consensus vote.
            structured.adjudication = Some(receipt.clone());
        } else {
            let mut opts = codex_review_opts(worktree.clone(), "Final adjudication");
            opts.session_id = None;
            if let Some(workflow) = context.workflow {
                opts.audit_logger = Some(workflow.audit_logger());
            }
            // Full source coverage was established above. Supply direct diff
            // and caller evidence plus the actual review receipts; explicitly
            // label the source digests as a manifest, not source read here.
            let adjudication_evidence = serde_json::to_string(&serde_json::json!({
                "candidate_diff": diff,
                "caller_evidence": callers,
                "source_manifest_only": files.iter().map(|(path,body)| (path,aligned::digest(body.as_bytes()))).collect::<BTreeMap<_,_>>(),
                "completed_reviews": structured.passes,
                "supplemental_immutable_source": recovered_files,
            }))?;
            let attempts = match context.workflow {
                Some(workflow) => {
                    workflow.check_resource_policy()?;
                    workflow.reserve_adjudication()?
                },
                None => context.adjudication_attempts_already_made,
            };
            structured.adjudication = Some(
                aligned::adjudicate_native_with_origin(
                    &structured.builders,
                    opts,
                    binding.clone(),
                    &contract,
                    &unresolved,
                    &adjudication_evidence,
                    attempts,
                    &structured.origin,
                )
                .await?,
            );
        }
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
        let collected=if recovered_files.is_empty() {
            review_calibration::collect(guard,&structured,&contract,&evidence_json)
        } else {
            review_calibration::collect_supplemental_outcome(guard,&structured,&contract,&evidence_json,&recovered_files)
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
        if let Some(existing) = unique.get(&finding.id) {
            anyhow::ensure!(
                existing.fingerprint()? == finding.fingerprint()?,
                "finding ID has conflicting evidence; request distinct IDs"
            );
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

    fn candidate_fixture() -> tempfile::TempDir {
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
            repair_rounds: 0,
        };
        assert_eq!(
            deduplicate_findings(vec![first.clone(), first.clone()])
                .unwrap()
                .len(),
            1
        );
        let mut changed = first.clone();
        changed.evidence_hash = "proof2".into();
        assert!(deduplicate_findings(vec![first, changed]).is_err());
    }
}
