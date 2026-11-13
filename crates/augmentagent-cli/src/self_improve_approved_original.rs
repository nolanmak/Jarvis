//! A retained review reference is data. Only this service can return the live
//! capability, which borrows the qualified stage and rechecks it before use.
use super::*;

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::self_improve::acceptance::amendment) struct Reference {
    version:u32,kind:String,attempt:Attempt,intent_sha256:String,response_sha256:String,
    completion_sha256:String,session:String,proposal_sha256:String,old_contract:String,
    original_plan:String,original_result:String,base:String,original_environment:String,
    candidate_environment:String,transition:bool,
}
impl Reference {
    /// Excludes routing binding and staging/publication references. Those are
    /// still authenticated separately; they cannot manufacture another phase.
    pub(in crate::self_improve::acceptance::amendment) fn semantic_identity(&self)->Result<String> {
        Ok(digest(&serde_json::to_vec(&("approved-original-authorization-v1",&self.attempt,
            &self.intent_sha256,&self.response_sha256,&self.proposal_sha256,&self.old_contract,
            &self.base,&self.original_environment,&self.candidate_environment,self.transition))?))
    }
    /// Retained review provenance only. Original-plan/result and measurement
    /// ownership references still need their own validation. This never creates
    /// a live capability, checks out source, repairs publication or admits work.
    pub(super) fn authenticate_review(&self,root:&Path,control:&Path,workflow:&workflow::WorkflowGuard)->Result<()> {
        ensure!(self.version==1 && self.kind=="approved_original_authority"
            && self.attempt.version==2 && self.attempt.workflow==workflow.id(),initial_repair::Hold::EvidenceUnknown);
        self.attempt.validate(workflow.id())?;
        let intent=workflow.recorded_scope_stage_response(&self.attempt.key())?.context(initial_repair::Hold::EvidenceUnknown)?;
        ensure!(intent==serde_json::to_string(&self.attempt)? && digest(intent.as_bytes())==self.intent_sha256,
            initial_repair::Hold::EvidenceUnknown);
        let purpose=self.attempt.native_purpose()?;
        let actual=workflow.authority_native_admission(&purpose)?.context(initial_repair::Hold::EvidenceUnknown)?;
        let (raw_key,proof_key)=crate::autoship_workflow::authority_native::response_keys(&purpose)?;
        let (raw,proof)=workflow.scope_stage_response_pair(&raw_key,&proof_key)?.context(initial_repair::Hold::EvidenceUnknown)?;
        ensure!(digest(raw.as_bytes())==self.response_sha256 && digest(proof.as_bytes())==self.completion_sha256,
            initial_repair::Hold::EvidenceUnknown);
        validate_observation(root,control,workflow.id(),&crate::autoship_workflow::NativeScopeCustody {
            input:raw_key,response:raw.clone(),provenance:proof.clone(),provider:actual})?;
        let completion:Completion=serde_json::from_str(&proof)?;
        ensure!(completion.session==self.session && completion.provider==self.attempt.provider,
            initial_repair::Hold::EvidenceUnknown);
        let decision=decode_authority_response(&raw,&self.old_contract)?;
        ensure!(decision.approved && decision.approve_environment_transition==self.transition
            && decision.proposal.as_ref().map(serde_json::to_vec).transpose()?.is_some_and(|bytes|digest(&bytes)==self.proposal_sha256),
            initial_repair::Hold::EvidenceUnknown);
        Ok(())
    }
}

/// This is the review-authority leg, not a source/owner/lease override. It is
/// deliberately neither serializable nor deserializable. A new current stage
/// is required to obtain it, even when the native decision itself replays.
pub(in crate::self_improve::acceptance::amendment) struct ApprovedOriginal<'a> {
    root:&'a Path,episode:&'a ConstructionEpisode,staged:&'a construction::StagedCorrection,
    binding:crate::autoship_workflow::Binding,reference:Reference,
}
impl std::fmt::Debug for ApprovedOriginal<'_> {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        f.write_str("ApprovedOriginal(current qualified stage and retained native review)")
    }
}
impl<'a> ApprovedOriginal<'a> {
    pub(super) fn from_review(root:&'a Path,workflow:&workflow::WorkflowGuard,episode:&'a ConstructionEpisode,
        staged:&'a construction::StagedCorrection,decision:&Decision,attempt:Attempt,response_sha256:String,
        session:&str)->Result<Self> {
        ensure!(decision.approved && attempt.version==2 && attempt.input.len()==64
            && attempt.episode==episode.key(),initial_repair::Hold::EvidenceUnknown);
        staged.validate_decision(decision)?;
        let (original_plan,original_result)=staged.original_references();
        let purpose=attempt.native_purpose()?;
        let (raw_key,proof_key)=crate::autoship_workflow::authority_native::response_keys(&purpose)?;
        let (_,proof)=workflow.scope_stage_response_pair(&raw_key,&proof_key)?.context(initial_repair::Hold::EvidenceUnknown)?;
        let reference=Reference {version:1,kind:"approved_original_authority".into(),
            intent_sha256:digest(serde_json::to_string(&attempt)?.as_bytes()),attempt,response_sha256,
            completion_sha256:digest(proof.as_bytes()),session:session.into(),
            proposal_sha256:digest(&serde_json::to_vec(decision.proposal.as_ref().context(initial_repair::Hold::EvidenceUnknown)?)?),
            old_contract:decision.previous_contract_hash.clone(),original_plan:original_plan.into(),original_result:original_result.into(),
            base:episode.original_base().into(),original_environment:episode.original_environment().into(),
            candidate_environment:staged.candidate_environment().into(),transition:decision.approve_environment_transition};
        reference.authenticate_review(root,episode.control(),workflow)?;
        Ok(Self {root,episode,staged,binding:workflow.binding().clone(),reference})
    }
    pub(in crate::self_improve::acceptance::amendment) fn reference(&self)->&Reference {&self.reference}
    pub(in crate::self_improve::acceptance::amendment) async fn registration(&self,original:&Path,proposal:&ContractProposal,
        base:&str,expected_original_environment:&str,candidate_environment:&str,workflow:&workflow::WorkflowGuard)->Result<RegisteredContract> {
        workflow.validate_before_side_effect()?;
        ensure!(workflow.binding()==&self.binding && self.reference.attempt.workflow==workflow.id(),initial_repair::Hold::EvidenceUnknown);
        self.staged.require_current(self.root,workflow,self.episode).await?;
        self.reference.authenticate_review(self.root,self.episode.control(),workflow)?;
        ensure!(digest(&serde_json::to_vec(proposal)?)==self.reference.proposal_sha256 && base==self.reference.base
            && expected_original_environment==self.reference.original_environment
            && candidate_environment==self.reference.candidate_environment,initial_repair::Hold::EvidenceUnknown);
        ensure!(actual_head(original).await?==self.reference.base,initial_repair::Hold::EvidenceUnknown);
        install_harness(original,proposal)?;
        let environment=environment_id(original)?;
        ensure!(environment==self.reference.original_environment,initial_repair::Hold::EvidenceUnknown);
        if self.reference.transition {
            RegisteredContract::register_transition(original,proposal.contract.clone(),base.into(),environment,candidate_environment.into())
        }else {
            ensure!(candidate_environment==environment,initial_repair::Hold::EvidenceUnknown);
            RegisteredContract::register(original,proposal.contract.clone(),base.into(),environment)
        }
    }
}
