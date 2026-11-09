//! Actual preparation-created identities, never productive source permission.
use super::*;
use std::sync::Arc;

/// Neither Clone nor serde: protected JSON cannot reconstruct the creator's
/// retained handles. This is an observation, not all-writer custody or a grant.
pub(crate) struct PreflightScopeCreation {
    controller:Arc<crate::tool_audit::AuditLogger>,
    data:Arc<crate::autoship_write_scope::ScopeMaterialization>,
    state:PathBuf,
    owner:RuntimeOwner,
    event:serde_json::Value,
    receipt:crate::audit_chain::AppendReceipt,
}
impl PreflightScopeCreation {
    pub(crate) fn validate(&self)->Result<()> {
        self.controller.native_bootstrap_construction_current()?;
        ensure!(self.controller.native_runtime_owner()?.as_ref()==Some(&self.owner),
            "preflight creation original owner changed");
        self.data.validate_current()?;
        ensure!(crate::autoship_write_scope::current_scope_reference(&self.state,&self.data.reference().worktree)?==*self.data.reference(),
            "preflight creation registered scope changed");
        let mut actual=self.controller.read_append_receipt(&self.receipt)?;
        actual.as_object_mut().context("preflight creation audit row is not an object")?.remove("audit_chain");
        ensure!(actual==self.event && self.event["created_sha256"]==self.data.digest()?,
            "preflight creation protected observation changed");
        self.data.validate_current()?;
        self.controller.native_bootstrap_construction_current()
    }
}
impl NativeBootstrap<'_> {
    pub(crate) fn observe_scope_creation(&self,logger:&crate::tool_audit::AuditLogger,
        data:Arc<crate::autoship_write_scope::ScopeMaterialization>,
    )->Result<PreflightScopeCreation> {
        self.check_logger(logger)?;
        self.logger.native_bootstrap_construction_current()?;
        ensure!(data.handle_count()<=1024,"preflight creation retained handle limit exceeded");
        data.validate_current()?;
        let state=crate::state_dir::state_dir().context("preflight creation state missing")?.canonicalize()?;
        ensure!(crate::autoship_write_scope::current_scope_reference(&state,&data.reference().worktree)?==*data.reference(),
            "preflight creation registered scope changed before observation");
        let owner=self.logger.native_runtime_owner()?.context("preflight creation native owner missing")?;
        let event=serde_json::json!({"actor":"controller","kind":"native_preflight_scope_materialized","version":1,
            "workflow":owner.workflow_id,"generation":owner.lease_generation,"scope":data.reference(),
            "created_sha256":data.digest()?,"created_files":data.paths().count(),"retained_handles":data.handle_count()});
        let receipt=self.logger.record_controller_receipted(event.clone())?;
        let observed=PreflightScopeCreation {controller:self.logger.clone(),data,state,owner,event,receipt};
        observed.validate()?;Ok(observed)
    }
}
