//! Resumable bounded final authority. Every native ruling remains attached to
//! its exact evidence and actual conversation; no partial group approves a PR.
use super::*;

pub(super) struct Input<'a> {
    pub worktree: &'a Path,
    pub guard: Option<&'a workflow::WorkflowGuard>,
    pub opts: augmentagent_channel_core::ReasonerOpts,
    pub review: &'a aligned::ReviewEvidence,
    pub contract: &'a BTreeMap<String,String>,
    pub findings: &'a [aligned::Finding],
    pub evidence: serde_json::Value,
    pub source: &'a BTreeMap<String,String>,
    pub finding_sources: &'a BTreeMap<String,BTreeSet<String>>,
    pub attempts_already_made: u32,
    pub diagnostic: bool,
}
fn sessions(review:&aligned::ReviewEvidence)->BTreeSet<String> {
    review.passes.iter().flat_map(|pass|std::iter::once(pass.actor.session_id.clone()).chain(pass.additional_sessions.iter().cloned())).collect()
}
impl Input<'_> {
    fn bytes(&self, findings:&[aligned::Finding], evidence:&serde_json::Value)->Result<usize> {
        aligned::adjudication_prompt_bytes(&self.review.binding,self.contract,findings,&serde_json::to_string(evidence)?)
    }
    async fn call(&self, findings:&[aligned::Finding], evidence:&serde_json::Value)->Result<aligned::AdjudicationReceipt> {
        let raw=serde_json::to_string(evidence)?;
        anyhow::ensure!(self.bytes(findings,evidence)?<=aligned::MAX_PROMPT_BYTES,"final authority evidence still exceeds exact wire budget before reservation");
        let model=augmentagent_channel_core::providers::model_for(augmentagent_channel_core::ProviderKind::Codex,
            augmentagent_channel_core::providers::tier_of(&self.opts));
        let key=aligned::digest(&serde_json::to_vec(&("final-authority-component-v1",&self.review.binding,self.contract,findings,&raw,model,aligned::adjudication_protocol_hash()))?);
        let cache=if self.diagnostic {None}else{self.guard};
        if let Some(stored)=cache.map(|guard|guard.review_fragment(&key)).transpose()?.flatten() {
            let receipt=serde_json::from_str(&stored)?;
            aligned::adjudication_groups::validate(&receipt,&self.review.binding,findings,&self.review.builders,&self.review.origin,&sessions(self.review))?;
            return Ok(receipt);
        }
        let attempts=if let Some(guard)=self.guard {
            guard.check_resource_policy()?;
            {
                let mut slot=None;
                for prior in 0..aligned::MAX_ADJUDICATION_ATTEMPTS {
                    if guard.admit_attempt(&format!("adjudication-component:{key}:{prior}"),1)? {slot=Some(prior);break;}
                }
                slot.context("bounded final-authority component attempts exhausted")?
            }
        } else {self.attempts_already_made};
        let receipt=aligned::adjudicate_native_with_origin(&self.review.builders,self.opts.clone(),self.review.binding.clone(),self.contract,
            findings,&raw,attempts,&self.review.origin).await?;
        aligned::adjudication_groups::validate(&receipt,&self.review.binding,findings,&self.review.builders,&self.review.origin,&sessions(self.review))?;
        if let Some(guard)=cache {
            guard.audit_health().await?;
            guard.record_review_fragment(&key,&serde_json::to_string(&receipt)?)?;
        }
        Ok(receipt)
    }
    pub async fn run(self)->Result<aligned::AdjudicationReceipt> {
        if self.bytes(self.findings,&self.evidence)?<=aligned::MAX_PROMPT_BYTES {
            return self.call(self.findings,&self.evidence).await;
        }
        let mut receipts=Vec::new();
        for component in aligned::adjudication_groups::components(self.findings)? {
            let mut source=BTreeMap::new();
            for finding in &component {
                for key in self.finding_sources.get(&finding.id).into_iter().flatten() {
                    source.insert(key.clone(),self.source.get(key).context("finding source evidence missing")?.clone());
                }
            }
            // Common contract and all cross-finding edges remain in each
            // component. Native review transcripts are references, not a claim
            // this final conversation has reread the entire candidate.
            let mut evidence=serde_json::json!({
                "full_evidence_manifest_sha256":aligned::digest(&serde_json::to_vec(&self.evidence)?),
                "controller_acceptance":self.evidence.get("controller_acceptance"),
                "completed_review_receipts":self.review.passes.iter().map(|pass|serde_json::json!({"role":pass.role,
                    "receipt_sha256":aligned::digest(&serde_json::to_vec(pass).expect("serialize review receipt"))})).collect::<Vec<_>>(),
                "reviewed_source":source,
                "scope":"Final ruling for one complete conflict-connected component. Other components cannot contradict or amend this component's owner criterion. Source supplied here is exact evidence from the independent review; omitted evidence is manifest-only, never a claim of source read in this conversation."
            });
            if self.bytes(&component,&evidence)?>aligned::MAX_PROMPT_BYTES {
                let guard=self.guard.context("oversized final authority needs protected evidence-window recovery")?;
                evidence.as_object_mut().unwrap().remove("reviewed_source");
                let overhead=self.bytes(&component,&evidence)?;
                let available=aligned::MAX_PROMPT_BYTES.saturating_sub(overhead).saturating_sub(4096)/6;
                anyhow::ensure!(available>=1024,"finding/contract metadata alone exceeds final authority budget");
                evidence["selected_source_windows"]=adjudication_windows::select(self.worktree,guard,&self.review.binding,&component,&source,available).await?;
            }
            receipts.push(self.call(&component,&evidence).await?);
        }
        aligned::adjudication_groups::compose(&self.review.binding,self.findings,receipts,&self.review.builders,&self.review.origin,&sessions(self.review))
    }
}
