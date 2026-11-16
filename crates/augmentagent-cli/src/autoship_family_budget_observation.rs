//! Read-only budget facts and typed blockers. Neither a Ready observation nor
//! its next action authenticates a request, grants a lease, or spends resources.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract::NextAction as WorkflowNextAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    OwnerPaused,
    FamilyAuthorityUnknown,
    FamilyExecutionScopeUnknown,
    FamilyMembershipPending,
    FamilyBudgetPolicyMissing,
    FamilyBudgetPolicyUnsupported,
    FamilySpendUnknown,
    FamilyResourceExhausted,
}
impl Reason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OwnerPaused => "owner_paused",
            Self::FamilyAuthorityUnknown => "family_authority_unknown",
            Self::FamilyExecutionScopeUnknown => "family_execution_scope_unknown",
            Self::FamilyMembershipPending => "family_membership_pending",
            Self::FamilyBudgetPolicyMissing => "family_budget_policy_missing",
            Self::FamilyBudgetPolicyUnsupported => "family_budget_policy_unsupported",
            Self::FamilySpendUnknown => "family_spend_unknown",
            Self::FamilyResourceExhausted => "family_resource_exhausted",
        }
    }
    fn priority(self) -> u8 {
        match self {
            Self::OwnerPaused => 0,
            Self::FamilyAuthorityUnknown => 1,
            Self::FamilyExecutionScopeUnknown => 2,
            Self::FamilyMembershipPending => 3,
            Self::FamilyBudgetPolicyMissing => 4,
            Self::FamilyBudgetPolicyUnsupported => 5,
            Self::FamilySpendUnknown => 6,
            Self::FamilyResourceExhausted => 7,
        }
    }
    pub(super) fn next(self) -> WorkflowNextAction {
        let action = match self {
            Self::OwnerPaused => "await_owner_resume",
            Self::FamilyAuthorityUnknown => "await_authoritative_family_state",
            Self::FamilyExecutionScopeUnknown => "await_protected_execution_scope",
            Self::FamilyMembershipPending => "await_verified_lineage",
            Self::FamilyBudgetPolicyMissing => "await_owner_budget_policy",
            Self::FamilyBudgetPolicyUnsupported => "await_compatible_budget_policy",
            Self::FamilySpendUnknown => "await_authoritative_usage_evidence",
            Self::FamilyResourceExhausted => "await_owner_budget_revision",
        };
        // These are input-dependent actions, not a new automatic retry grant.
        WorkflowNextAction {
            action: action.into(),
            eligible_at_ms: None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Metric {
    ModelAdmissions,
    WallMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Blocker {
    pub family: Option<String>,
    pub workflow: String,
    pub reason_code: Reason,
    pub metric: Option<Metric>,
    /// Actual retained usage, never a charge for the proposed admission.
    pub observed: Option<u64>,
    pub limit: Option<u64>,
    /// Prospective model admissions; committed wall has no new charge here.
    pub requested: Option<u64>,
    pub next: WorkflowNextAction,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TokenCompleteness {
    Unknown,
}

/// Status values may be zero. They are not owner policy Limits, whose bounds
/// must be positive. An unknown dimension stays None rather than becoming zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Remaining {
    pub model_admissions: Option<u64>,
    pub wall_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub schema_version: u32,
    pub workflow: String,
    pub intent: Intent,
    #[serde(rename = "verified_model_admissions")]
    pub model_admissions: Option<u64>,
    #[serde(rename = "committed_wall_ms")]
    pub wall_ms: Option<u64>,
    #[serde(rename = "verified_tokens")]
    pub tokens: Option<u64>,
    pub token_completeness: TokenCompleteness,
    #[serde(rename = "per_member_reported_usage")]
    pub retained: Vec<RetainedUsage>,
    pub limits: Option<Limits>,
    pub remaining: Option<Remaining>,
    pub blockers: Vec<Blocker>,
}
impl Observation {
    fn new(workflow: &str, intent: &Intent) -> Self {
        Self {
            schema_version: 1,
            workflow: workflow.into(),
            intent: intent.clone(),
            model_admissions: None,
            wall_ms: None,
            tokens: None,
            token_completeness: TokenCompleteness::Unknown,
            retained: vec![],
            limits: None,
            remaining: None,
            blockers: vec![],
        }
    }
    pub(crate) fn ready(&self) -> bool {
        self.blockers.is_empty()
    }
    pub(crate) fn reason_code(&self) -> Option<&'static str> {
        self.blockers.first().map(|b| b.reason_code.as_str())
    }
    pub(super) fn block(&mut self, reason: Reason) {
        self.exhausted_or_block(reason, None, None, None, None);
    }
    fn exhausted_or_block(
        &mut self,
        reason: Reason,
        metric: Option<Metric>,
        observed: Option<u64>,
        limit: Option<u64>,
        requested: Option<u64>,
    ) {
        let family = match &self.intent {
            Intent::Delegated { family, .. } => Some(family.clone()),
            Intent::Independent { .. } => None,
        };
        let blocker = Blocker {
            family,
            workflow: self.workflow.clone(),
            reason_code: reason,
            metric,
            observed,
            limit,
            requested,
            next: reason.next(),
        };
        if !self.blockers.contains(&blocker) {
            self.blockers.push(blocker);
        }
        self.blockers.sort_by(|a, b| {
            (a.reason_code.priority(), &a.family, a.metric, &a.workflow).cmp(&(
                b.reason_code.priority(),
                &b.family,
                b.metric,
                &b.workflow,
            ))
        });
    }
    fn compute_remaining_and_limits(&mut self, incoming: u64) {
        let Some(limits) = self.limits.clone() else {
            return;
        };
        self.remaining = Some(Remaining {
            model_admissions: self
                .model_admissions
                .map(|n| limits.max_model_admissions.saturating_sub(n)),
            wall_ms: self.wall_ms.map(|n| limits.max_wall_ms.saturating_sub(n)),
        });
        // A zero incoming charge is metadata-only. It may explain retained
        // remaining=0 without turning an intent refresh into a paid admission.
        if incoming == 0 {
            return;
        }
        if let Some(used) = self.model_admissions {
            if used
                .checked_add(incoming)
                .is_none_or(|n| n > limits.max_model_admissions)
            {
                self.exhausted_or_block(
                    Reason::FamilyResourceExhausted,
                    Some(Metric::ModelAdmissions),
                    Some(used),
                    Some(limits.max_model_admissions),
                    Some(incoming),
                );
            }
        }
        if let Some(used) = self.wall_ms {
            if used >= limits.max_wall_ms {
                self.exhausted_or_block(
                    Reason::FamilyResourceExhausted,
                    Some(Metric::WallMs),
                    Some(used),
                    Some(limits.max_wall_ms),
                    None,
                );
            }
        }
    }
}

/// Explain all independently knowable blockers. Missing authority stops only
/// checks that depend on it; it is never interpreted as empty family readiness.
pub(crate) fn observe(
    conn: &Connection,
    workflow: &str,
    intent: &Intent,
    incoming: u64,
) -> Result<Observation> {
    let mut result = Observation::new(workflow, intent);
    if matches!(intent, Intent::Independent { .. }) && !required(conn, workflow)? {
        return Ok(result);
    }
    for name in [
        "workflow_family_budget",
        "workflow_family_member",
        "workflow_family_policy",
        "workflow_family_scope",
    ] {
        if !table(conn, name)? {
            result.block(Reason::FamilyAuthorityUnknown);
            return Ok(result);
        }
    }
    match intent {
        Intent::Independent { request_ref } => {
            let root: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_family_budget WHERE root_workflow=?1)",
                [workflow],
                |r| r.get(0),
            )?;
            let allowed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_family_member m JOIN command_request r ON r.request_id=m.independent_ref WHERE m.workflow=?1 AND m.independent_ref=?2 AND json_extract(r.admission,'$.workflow')=?1)",params![workflow,request_ref],|r|r.get(0))?;
            if root || !allowed {
                result.block(Reason::FamilyExecutionScopeUnknown);
            }
        }
        Intent::Delegated {
            family,
            manifest_ref,
            policy_ref,
            request_ref,
        } => {
            let group:Option<(String,String,Option<String>,Option<String>,String,u64)>=conn.query_row(
                "SELECT root_workflow,state,manifest_ref,policy_ref,original_request_ref,expected_children FROM workflow_family_budget WHERE family=?1",[family],
                |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            let Some((root, state, manifest, active, request, count)) = group else {
                result.block(Reason::FamilyAuthorityUnknown);
                return Ok(result);
            };
            let member:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_family_member WHERE family=?1 AND workflow=?2)",params![family,workflow],|r|r.get(0))?;
            if !member || &request != request_ref {
                result.block(Reason::FamilyExecutionScopeUnknown);
            }
            if root != workflow && owner_control::read(conn, &root)?.is_some_and(|c| c.paused) {
                result.block(Reason::OwnerPaused);
            }
            if state == "pending" {
                result.block(Reason::FamilyMembershipPending);
            } else if state != "bound" {
                result.block(Reason::FamilyAuthorityUnknown);
            }
            let members: u64 = conn.query_row(
                "SELECT count(*) FROM workflow_family_member WHERE family=?1",
                [family],
                |r| r.get(0),
            )?;
            if !(2..=6).contains(&count)
                || (state == "bound"
                    && (members != count + 1 || manifest.as_deref() != Some(manifest_ref)))
            {
                result.block(Reason::FamilyAuthorityUnknown);
            }
            if let Some(active) = active {
                if &active != policy_ref {
                    result.block(Reason::FamilyExecutionScopeUnknown);
                }
                let raw:Option<String>=conn.query_row("SELECT payload FROM workflow_family_policy WHERE reference=?1 AND family=?2",params![active,family],|r|r.get(0)).optional()?;
                match raw {
                    None => result.block(Reason::FamilyBudgetPolicyMissing),
                    Some(raw) => match serde_json::from_str::<Limits>(&raw) {
                        Ok(limits) if limits.validate().is_ok() => result.limits = Some(limits),
                        _ => result.block(Reason::FamilyBudgetPolicyUnsupported),
                    },
                }
            } else {
                result.block(Reason::FamilyBudgetPolicyMissing);
            }
            let usage = usage::aggregate(conn, family)?;
            let membership_complete = state == "bound"
                && (2..=6).contains(&count)
                && members == count + 1
                && manifest.as_deref() == Some(manifest_ref);
            // A partial/empty member set is not a complete family ledger.
            // Preserve its reported facts, but never certify its SUM as zero.
            if membership_complete {
                result.model_admissions = usage.model_admissions;
                result.wall_ms = usage.wall_ms;
            }
            result.retained = usage.retained;
            if result.model_admissions.is_none() || result.wall_ms.is_none() {
                result.block(Reason::FamilySpendUnknown);
            }
            result.compute_remaining_and_limits(incoming);
        }
    }
    Ok(result)
}
