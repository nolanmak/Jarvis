//! S-owned budget accounting, never command authentication or lineage authority.
//! Only trusted T services may publish verified policy/intent. No public/model API
//! calls these writers. Missing data cannot create a grant; no limits default.
use super::*;
use std::collections::BTreeSet;

#[path = "autoship_family_budget_store.rs"]
pub(crate) mod store;
#[path = "autoship_family_budget_usage.rs"]
mod usage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub schema_version: u32,
    pub max_model_admissions: u64,
    pub max_wall_ms: u64,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.max_model_admissions > 0 && self.max_wall_ms > 0,
            "family_budget_policy_unsupported"
        );
        i64::try_from(self.max_model_admissions)?;
        i64::try_from(self.max_wall_ms)?;
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Intent {
    Independent {
        request_ref: String,
    },
    Delegated {
        family: String,
        manifest_ref: String,
        policy_ref: String,
        request_ref: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RetainedUsage {
    pub workflow: String,
    pub model_admissions: u64,
    pub wall_ms: u64,
}
#[path = "autoship_family_budget_observation.rs"]
pub(crate) mod observation;
pub(crate) use observation::Observation;
#[path = "autoship_family_budget_wait.rs"]
pub(crate) mod wait;

/// A valid protected funding frame reached the atomic budget predicate and was
/// refused. The controller may persist its facts; it cannot treat it as spend.
#[derive(Debug)]
pub(crate) struct AdmissionRefused {
    pub observation: Observation,
}
impl std::fmt::Display for AdmissionRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            self.observation
                .reason_code()
                .unwrap_or("family_authority_unknown"),
        )
    }
}
impl std::error::Error for AdmissionRefused {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Scope {
    schema_version: u32,
    intent: Intent,
    verified_authority_ref: String,
    root_control_revision: Option<i64>,
}
fn table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}
fn required(conn: &Connection, workflow: &str) -> Result<bool> {
    let marked:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('workflow') WHERE name='family_budget_required')",[],|r|r.get(0))?;
    if marked
        && conn
            .query_row(
                "SELECT family_budget_required FROM workflow WHERE id=?1",
                [workflow],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false)
    {
        return Ok(true);
    }
    if !table(conn, "workflow_family_member")? {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_family_member WHERE workflow=?1)",
        [workflow],
        |r| r.get(0),
    )?)
}
fn revision(conn: &Connection, root: &str) -> Result<i64> {
    Ok(owner_control::read(conn, root)?
        .map(|c| c.revision)
        .unwrap_or(0))
}

/// Shared budget-only predicate; actual authority and custody remain separate.
pub(crate) use observation::observe;

/// Called only for a NEW positive charge, after existing receipt verification.
/// Existing receipt replay and late wall/token accounting never enter this gate.
pub(super) fn require_new_admission(
    tx: &Transaction<'_>,
    lease: &Lease,
    incoming: u64,
    native: bool,
) -> Result<Option<String>> {
    if !required(tx, &lease.workflow)? {
        return Ok(None);
    }
    ensure!(
        table(tx, "workflow_family_budget")? && table(tx, "workflow_family_member")?,
        "family_authority_unknown"
    );
    let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_family_member m JOIN workflow_family_budget f ON f.family=m.family WHERE m.workflow=?1 AND f.state='pending')",[&lease.workflow],|r|r.get(0))?;
    // No legacy/unscoped caller can implicitly declare independently funded work.
    ensure!(
        table(tx, "workflow_family_scope")?,
        if pending {
            "family_membership_pending"
        } else {
            "family_execution_scope_unknown"
        }
    );
    let raw: Option<String> = tx
        .query_row(
            "SELECT payload FROM workflow_family_scope WHERE workflow=?1 AND generation=?2",
            params![lease.workflow, lease.generation],
            |r| r.get(0),
        )
        .optional()?;
    let raw = raw.context(if pending {
        "family_membership_pending"
    } else {
        "family_execution_scope_unknown"
    })?;
    let scope: Scope = serde_json::from_str(&raw).context("family_execution_scope_unknown")?;
    ensure!(
        scope.schema_version == 1 && !scope.verified_authority_ref.is_empty(),
        "family_execution_scope_unknown"
    );
    ensure!(native, "family_native_attribution_required");
    let status = observe_scope(tx, &lease.workflow, &scope, incoming)?;
    if !status.ready() {
        return Err(AdmissionRefused {
            observation: status,
        }
        .into());
    }
    Ok(Some(format!("sha256:{:x}", Sha256::digest(raw.as_bytes()))))
}

/// A decoded funding frame can be observed without granting a lease or charge.
/// Keep ancestor control revision diagnostics identical at selection/admission.
fn observe_scope(conn: &Connection, workflow: &str, scope: &Scope, incoming: u64) -> Result<Observation> {
    let mut status = observe(conn, workflow, &scope.intent, incoming)?;
    if let Intent::Delegated { family, .. } = &scope.intent {
        let root: Option<String> = conn.query_row(
            "SELECT root_workflow FROM workflow_family_budget WHERE family=?1",
            [family], |row| row.get(0)).optional()?;
        match root {
            Some(root) if scope.root_control_revision != Some(revision(conn, &root)?) =>
                status.block(observation::Reason::FamilyExecutionScopeUnknown),
            None => status.block(observation::Reason::FamilyAuthorityUnknown),
            _ => {},
        }
    }
    Ok(status)
}
