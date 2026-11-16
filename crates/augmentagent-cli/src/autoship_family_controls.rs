//! Control/funding checks for a new mutation or native launch. This is neither
//! a compute-budget grant nor a prerequisite for reading/reconciling receipts.
use super::*;
use observation::Reason;

pub(super) fn frame(conn: &Connection, lease: &Lease) -> Result<Option<(String, Scope)>> {
    if !required(conn, &lease.workflow)? {
        return Ok(None);
    }
    if !table(conn, "workflow_family_budget")? || !table(conn, "workflow_family_member")? {
        return Err(wait::refusal(Reason::FamilyAuthorityUnknown));
    }
    let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_family_member m JOIN workflow_family_budget f ON f.family=m.family WHERE m.workflow=?1 AND f.state='pending')",[&lease.workflow],|r|r.get(0))?;
    let missing = if pending {
        Reason::FamilyMembershipPending
    } else {
        Reason::FamilyExecutionScopeUnknown
    };
    if !table(conn, "workflow_family_scope")? {
        return Err(wait::refusal(missing));
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT payload FROM workflow_family_scope WHERE workflow=?1 AND generation=?2",
            params![lease.workflow, lease.generation],
            |r| r.get(0),
        )
        .optional()?;
    let raw = raw.ok_or_else(|| wait::refusal(missing))?;
    let scope: Scope = serde_json::from_str(&raw)
        .map_err(|_| wait::refusal(Reason::FamilyExecutionScopeUnknown))?;
    if scope.schema_version != 1 || scope.verified_authority_ref.is_empty() {
        return Err(wait::refusal(Reason::FamilyExecutionScopeUnknown));
    }
    Ok(Some((raw, scope)))
}

/// Caller must already validate the current lease/binding in this transaction.
/// Success authorizes no model call, VM capacity, RPC replay or merge debit.
pub(crate) fn require_dispatch(conn: &Connection, lease: &Lease) -> Result<()> {
    let Some((_, scope)) = frame(conn, lease)? else {
        return Ok(());
    };
    let observation = observe_scope(conn, &lease.workflow, &scope, 0)?;
    // Unknown aggregate spending must still stop a paid admission. It does not
    // revoke independently verified control/funding for a zero-compute action.
    // Keep the observation intact: this is not a budget-Ready result.
    if observation.blockers.iter().any(|b| {
        !matches!(
            b.reason_code,
            Reason::FamilySpendUnknown | Reason::FamilyResourceExhausted
        )
    }) {
        return Err(AdmissionRefused { observation }.into());
    }
    Ok(())
}

impl WorkflowStore {
    pub(crate) fn validate_family_control(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        require_dispatch(&tx, lease)?;
        tx.commit()?;
        Ok(())
    }
}
