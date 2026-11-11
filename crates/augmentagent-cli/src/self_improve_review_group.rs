//! The production evidence-group adapter; transport injection is used only by
//! deterministic protocol fixtures, never by a CLI or runtime configuration.
use super::*;

pub(super) const MAX_COMPLETED_GROUP_BYTES: usize =
    4 * aligned::MAX_PROMPT_BYTES + 12 * aligned::MAX_RESPONSE_BYTES + 64 * 1024;
const _: () = assert!(MAX_COMPLETED_GROUP_BYTES < 8 * 1024 * 1024);

#[async_trait::async_trait]
pub(super) trait GroupTransport: Send + Sync {
    async fn observe_group(
        &self,
        provider: augmentagent_channel_core::ProviderKind,
        opts: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> Result<(String, String)>;
}

pub(super) struct NativeGroupTransport<'a> {
    pub reasoner: &'a augmentagent_channel_core::FallbackReasoner,
}
#[async_trait::async_trait]
impl GroupTransport for NativeGroupTransport<'_> {
    async fn observe_group(
        &self,
        provider: augmentagent_channel_core::ProviderKind,
        opts: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> Result<(String, String)> {
        augmentagent_channel_core::native_session::fresh_review_scope(
            provider,
            augmentagent_channel_core::model_router::native_reviewer_scope(
                provider,
                self.reasoner.call(opts, prompt),
            ),
        )
        .await
    }
}

pub(super) struct EvidenceGroupInput<'a> {
    pub root: &'a Path,
    pub inputs: &'a evidence_plan::Inputs<'a>,
    pub original_request: &'a evidence_plan::Request,
    pub evidence_shards: &'a BTreeMap<String, String>,
    pub actor: &'a aligned::Actor,
    pub builders: &'a [aligned::Actor],
    pub origin: &'a aligned::ReviewOrigin,
    pub role: aligned::ReviewRole,
    pub provider: augmentagent_channel_core::ProviderKind,
    pub opts: &'a augmentagent_channel_core::ReasonerOpts,
    pub group_index: usize,
    pub workflow: Option<&'a workflow::WorkflowGuard>,
}

pub(super) async fn execute_evidence_group(
    input: EvidenceGroupInput<'_>,
    transport: &dyn GroupTransport,
) -> Result<CompletedGroup> {
    let EvidenceGroupInput {
        root: worktree,
        inputs,
        original_request,
        evidence_shards,
        actor,
        builders,
        origin,
        role,
        provider,
        opts,
        group_index: i,
        workflow,
    } = input;
    let binding = inputs.binding;
    let head = binding.head.as_str();
    let contract = inputs.contract;
    let session = &actor.session_id;
    let mut opts = opts.clone();
    let metadata = serde_json::to_vec(
        &serde_json::json!({"binding":binding,"actor":actor,"role":role,
        "original_hash":original_request.coverage.prompt_hash,"request_hashes":["0".repeat(64),"0".repeat(64)]}),
    )?;
    if metadata.len() > 32 * 1024 {
        return Err(EvidenceBudget("review identity metadata exceeds bound".into()).into());
    }

    let mut group_hashes = Vec::new();
    let mut group_sessions = Vec::new();
    let mut group_recovered = BTreeMap::new();
    let mut request = original_request.clone();
    let mut group_review = evidence_plan::GroupReview::default();
    for recovery_attempt in 0..=1 {
        opts.session_id = Some(format!("{session}:group-{i}:recovery-{recovery_attempt}"));
        opts.restrict_env = true;
        anyhow::ensure!(
            git(worktree, &["rev-parse", "HEAD"]).await?.trim() == head,
            "candidate changed before evidence recovery"
        );
        let (raw, native_id) = transport
            .observe_group(provider, &opts, &request.prompt)
            .await?;
        anyhow::ensure!(
            !native_id.is_empty()
                && native_id.len() <= 512
                && native_id.bytes().all(|b| b.is_ascii_graphic()),
            "invalid actual reviewer native identity"
        );
        let mut shard_actor = actor.clone();
        shard_actor.session_id = native_id.clone();
        shard_actor.independent_for(builders, origin)?;
        group_sessions.push(native_id);
        let parsed = aligned::parse_review_pass_with_sources(
            &raw,
            binding,
            shard_actor,
            role,
            request.coverage.clone(),
            contract,
        );
        let (pass, missing) = match parsed {
            Ok(value) => value,
            Err(error) if recovery_attempt == 0 => {
                request=inputs.request(original_request.files.clone(),false,Some(serde_json::json!({"kind":"structured_review_correction","validation_error":truncate(&format!("{error:#}"),4000),"instruction":"Classify every finding with a nonempty typed repair target. Do not infer a code defect from missing evidence; return incomplete with exact missing_sources requests (path, registered criterion_ids, concrete question, selector kind whole_file/rust_item/bytes) when necessary. Preserve all valid findings and the exact candidate binding."})))?;
                continue;
            }
            Err(error) => {
                return Err(error.context("bounded structured review correction exhausted"))
            }
        };
        group_hashes.push(pass.coverage.prompt_hash.clone());
        if group_review
            .observe(pass.completed, pass.findings)
            .with_context(|| format!("evidence group {i} still needs {missing:?}"))?
        {
            break;
        }
        anyhow::ensure!(
            recovery_attempt == 0,
            "review evidence remains incomplete after the second observation"
        );
        request = evidence_plan::recover_sources(
            inputs,
            worktree,
            head,
            original_request,
            &evidence_shards,
            &missing,
            group_review.pending(),
        )
        .await?;
        if let Some(workflow) = workflow {
            workflow.audit_logger().record_controller(serde_json::json!({"actor":"controller","kind":"review_evidence_recovery",
                        "binding":binding,"group":i,"role":role,"request_hash":request.coverage.prompt_hash,
                        "supplied_evidence":request.coverage.supplied_files,"missing_sources":missing}))?;
        }
        for (path, body) in &request.files {
            if path.starts_with("__requested_source__:") {
                if let Some(old) = group_recovered.insert(path.clone(), body.clone()) {
                    anyhow::ensure!(
                        old == *body,
                        "same immutable recovery path has different bytes"
                    );
                }
            }
        }
    }
    let findings = group_review.finish()?;
    let finding_evidence = if findings.is_empty() {
        BTreeMap::new()
    } else {
        request.files
    };
    let completed = CompletedGroup {
        binding: binding.clone(),
        role,
        original_hash: original_request.coverage.prompt_hash.clone(),
        findings,
        request_hashes: group_hashes,
        native_sessions: group_sessions,
        recovered_files: group_recovered,
        finding_evidence,
    };
    let encoded = serde_json::to_vec(&completed)?;
    if encoded.len() > MAX_COMPLETED_GROUP_BYTES {
        return Err(EvidenceBudget("completed review group exceeds encoded bound".into()).into());
    }
    Ok(completed)
}

#[cfg(test)]
#[path = "self_improve_review_group_tests.rs"]
mod tests;
