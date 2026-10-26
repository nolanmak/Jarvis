//! Resumable bounded final authority. Every native ruling remains attached to
//! its exact evidence and actual conversation; no partial group approves a PR.
use super::*;

pub(super) struct Input<'a> {
    pub worktree: &'a Path,
    pub guard: Option<&'a workflow::WorkflowGuard>,
    pub opts: augmentagent_channel_core::ReasonerOpts,
    pub review: &'a aligned::ReviewEvidence,
    pub contract: &'a BTreeMap<String, String>,
    pub findings: &'a [aligned::Finding],
    pub evidence: serde_json::Value,
    pub source: &'a BTreeMap<String, String>,
    pub finding_sources: &'a BTreeMap<String, BTreeSet<String>>,
    pub attempts_already_made: u32,
    pub diagnostic: bool,
}
pub(super) struct Output {
    pub receipt: aligned::AdjudicationReceipt,
    pub source_hashes: BTreeMap<String, String>,
    pub recovered_contexts: BTreeMap<String, String>,
}

fn sessions(review: &aligned::ReviewEvidence) -> BTreeSet<String> {
    review
        .passes
        .iter()
        .flat_map(|pass| {
            std::iter::once(pass.actor.session_id.clone())
                .chain(pass.additional_sessions.iter().cloned())
        })
        .collect()
}
impl Input<'_> {
    fn bytes(&self, findings: &[aligned::Finding], evidence: &serde_json::Value) -> Result<usize> {
        aligned::adjudication_prompt_bytes(
            &self.review.binding,
            self.contract,
            findings,
            &serde_json::to_string(evidence)?,
        )
    }
    async fn call(
        &self,
        findings: &[aligned::Finding],
        evidence: &serde_json::Value,
    ) -> Result<aligned::AdjudicationReceipt> {
        let raw = serde_json::to_string(evidence)?;
        anyhow::ensure!(
            self.bytes(findings, evidence)? <= aligned::MAX_PROMPT_BYTES,
            "final authority evidence still exceeds exact wire budget before reservation"
        );
        let model = augmentagent_channel_core::providers::model_for(
            augmentagent_channel_core::ProviderKind::Codex,
            augmentagent_channel_core::providers::tier_of(&self.opts),
        );
        let key = aligned::digest(&serde_json::to_vec(&(
            "final-authority-component-v1",
            &self.review.binding,
            self.contract,
            findings,
            &raw,
            model,
            aligned::adjudication_protocol_hash(),
        ))?);
        let cache = if self.diagnostic { None } else { self.guard };
        if let Some(stored) = cache
            .map(|guard| guard.review_fragment(&key))
            .transpose()?
            .flatten()
        {
            let receipt = serde_json::from_str(&stored)?;
            aligned::adjudication_groups::validate(
                &receipt,
                &self.review.binding,
                findings,
                &self.review.builders,
                &self.review.origin,
                &sessions(self.review),
            )?;
            return Ok(receipt);
        }
        let attempts = if let Some(guard) = self.guard {
            guard.check_resource_policy()?;
            {
                let mut slot = None;
                for prior in 0..aligned::MAX_ADJUDICATION_ATTEMPTS {
                    if guard.admit_attempt(&format!("adjudication-component:{key}:{prior}"), 1)? {
                        slot = Some(prior);
                        break;
                    }
                }
                slot.context("bounded final-authority component attempts exhausted")?
            }
        } else {
            self.attempts_already_made
        };
        let receipt = aligned::adjudicate_native_with_origin(
            &self.review.builders,
            self.opts.clone(),
            self.review.binding.clone(),
            self.contract,
            findings,
            &raw,
            attempts,
            &self.review.origin,
        )
        .await?;
        aligned::adjudication_groups::validate(
            &receipt,
            &self.review.binding,
            findings,
            &self.review.builders,
            &self.review.origin,
            &sessions(self.review),
        )?;
        if let Some(guard) = cache {
            guard.audit_health().await?;
            guard.record_review_fragment(&key, &serde_json::to_string(&receipt)?)?;
        }
        Ok(receipt)
    }
    async fn resolve(
        &self,
        findings: &[aligned::Finding],
        initial: &serde_json::Value,
    ) -> Result<Output> {
        let mut evidence = initial.clone();
        let mut source_hashes = BTreeMap::new();
        let mut recovered_contexts = BTreeMap::new();
        let mut history = BTreeSet::<String>::new();
        let mut questions = BTreeMap::<String, serde_json::Value>::new();
        let mut paths = BTreeSet::new();
        loop {
            let receipt = self.call(findings, &evidence).await?;
            let requests = receipt
                .rulings
                .iter()
                .filter_map(|(id, ruling)| match ruling {
                    aligned::Ruling::NeedEvidence {
                        source_paths,
                        question,
                    } => Some((id, source_paths, question)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if requests.is_empty() {
                return Ok(Output {
                    receipt,
                    source_hashes,
                    recovered_contexts,
                });
            }
            let guard = self
                .guard
                .context("final evidence recovery requires protected workflow state")?;
            for (id, requested, question) in &requests {
                let key = aligned::evidence_request_key(id, requested, question)?;
                anyhow::ensure!(
                    history.insert(key.clone()),
                    "final authority repeated already resolved source request"
                );
                paths.extend(requested.iter().cloned());
                questions.insert(
                    key,
                    serde_json::json!({"finding_id":id,"paths":requested,"question":question}),
                );
            }
            // Cache assembled controller evidence, not merely model selections.
            // Reopening a lease replays these immutable transitions for free;
            // audit-generation recovery invalidates the fragment namespace.
            let key = aligned::digest(&serde_json::to_vec(&(
                "requested-authority-context-v1",
                &self.review.binding,
                self.contract,
                findings,
                &evidence,
                &history,
                &questions,
                aligned::adjudication_protocol_hash(),
            ))?);
            let cached = if self.diagnostic {
                None
            } else {
                guard.review_fragment(&key)?
            };
            let next = if let Some(raw) = cached {
                serde_json::from_str::<serde_json::Value>(&raw)?
            } else {
                let mut next = serde_json::json!({
                    "initial_evidence_manifest_sha256":aligned::digest(&serde_json::to_vec(initial)?),
                    "controller_acceptance":self.evidence.get("controller_acceptance"),
                    "controller_evidence_request_history":history,
                    "controller_evidence_questions":questions,
                    "scope":"Original independent reviews remain bound to the full candidate. This final conversation sees only the explicit recovered source below. Unavailable source is a controller fact, never evidence disproving a source-dependent finding. Resolve from supplied facts or request a concrete different dependency; do not send missing evidence to the builder."
                });
                let overhead = self.bytes(findings, &next)?;
                let budget = aligned::MAX_PROMPT_BYTES
                    .saturating_sub(overhead)
                    .saturating_sub(4096)
                    / 6;
                anyhow::ensure!(
                    budget >= 4096,
                    "final authority source-request metadata exceeds bounded evidence budget"
                );
                let controller_sources = self
                    .evidence
                    .get("controller_sources")
                    .cloned()
                    .filter(|value| !value.is_null())
                    .map(serde_json::from_value::<BTreeMap<String, String>>)
                    .transpose()?
                    .unwrap_or_default();
                // Questions guide evidence selection only; original finding IDs,
                // facts and fingerprints remain unchanged in native rulings.
                let mut selection_findings = findings.to_vec();
                for finding in &mut selection_findings {
                    let relevant = questions
                        .values()
                        .filter(|q| q["finding_id"].as_str() == Some(finding.id.as_str()))
                        .collect::<Vec<_>>();
                    finding.claim = format!(
                        "{}\nController source-recovery questions: {}",
                        finding.claim,
                        serde_json::to_string(&relevant)?
                    );
                }
                next["requested_source"] = requested_evidence::read(
                    self.worktree,
                    guard,
                    &self.review.binding,
                    &paths,
                    &selection_findings,
                    &controller_sources,
                    budget,
                )
                .await?;
                anyhow::ensure!(
                    self.bytes(findings, &next)? <= aligned::MAX_PROMPT_BYTES,
                    "assembled requested evidence exceeds wire budget"
                );
                if !self.diagnostic {
                    guard.record_review_fragment(&key, &serde_json::to_string(&next)?)?;
                }
                next
            };
            if let Some(hashes) = next["requested_source"]["source_hashes"].as_object() {
                for (path, hash) in hashes {
                    source_hashes.insert(
                        path.clone(),
                        hash.as_str()
                            .context("requested source hash is not text")?
                            .to_string(),
                    );
                }
            }
            recovered_contexts.insert(
                format!("__final_authority_context_{key}__"),
                serde_json::to_string(&next)?,
            );
            evidence = next;
        }
    }
    pub async fn run(self) -> Result<Output> {
        if self.bytes(self.findings, &self.evidence)? <= aligned::MAX_PROMPT_BYTES {
            return self.resolve(self.findings, &self.evidence).await;
        }
        let mut receipts = Vec::new();
        let mut source_hashes = BTreeMap::new();
        let mut recovered_contexts = BTreeMap::new();
        for component in aligned::adjudication_groups::components(self.findings)? {
            let mut source = BTreeMap::new();
            for finding in &component {
                for key in self.finding_sources.get(&finding.id).into_iter().flatten() {
                    source.insert(
                        key.clone(),
                        self.source
                            .get(key)
                            .context("finding source evidence missing")?
                            .clone(),
                    );
                }
            }
            // Common contract and all cross-finding edges remain in each
            // component. Native review transcripts are references, not a claim
            // this final conversation has reread the entire candidate.
            let mut evidence = serde_json::json!({
                "full_evidence_manifest_sha256":aligned::digest(&serde_json::to_vec(&self.evidence)?),
                "controller_acceptance":self.evidence.get("controller_acceptance"),
                "completed_review_receipts":self.review.passes.iter().map(|pass|serde_json::json!({"role":pass.role,
                    "receipt_sha256":aligned::digest(&serde_json::to_vec(pass).expect("serialize review receipt"))})).collect::<Vec<_>>(),
                "reviewed_source":source,
                "scope":"Final ruling for one complete conflict-connected component. Other components cannot contradict or amend this component's owner criterion. Source supplied here is exact evidence from the independent review; omitted evidence is manifest-only, never a claim of source read in this conversation."
            });
            if self.bytes(&component, &evidence)? > aligned::MAX_PROMPT_BYTES {
                let guard = self.guard.context(
                    "oversized final authority needs protected evidence-window recovery",
                )?;
                evidence.as_object_mut().unwrap().remove("reviewed_source");
                let overhead = self.bytes(&component, &evidence)?;
                let available = aligned::MAX_PROMPT_BYTES
                    .saturating_sub(overhead)
                    .saturating_sub(4096)
                    / 6;
                anyhow::ensure!(
                    available >= 1024,
                    "finding/contract metadata alone exceeds final authority budget"
                );
                evidence["selected_source_windows"] = adjudication_windows::select(
                    self.worktree,
                    guard,
                    &self.review.binding,
                    &component,
                    &source,
                    available,
                )
                .await?;
            }
            let result = self.resolve(&component, &evidence).await?;
            receipts.push(result.receipt);
            source_hashes.extend(result.source_hashes);
            recovered_contexts.extend(result.recovered_contexts);
        }
        let receipt = aligned::adjudication_groups::compose(
            &self.review.binding,
            self.findings,
            receipts,
            &self.review.builders,
            &self.review.origin,
            &sessions(self.review),
        )?;
        Ok(Output {
            receipt,
            source_hashes,
            recovered_contexts,
        })
    }
}
