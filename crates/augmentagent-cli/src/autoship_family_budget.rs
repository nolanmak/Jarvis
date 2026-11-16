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
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RetainedUsage {
    pub workflow: String,
    pub model_admissions: u64,
    pub wall_ms: u64,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Observation {
    pub schema_version: u32,
    pub reason_code: Option<String>,
    pub model_admissions: Option<u64>,
    pub wall_ms: Option<u64>,
    // Native admission and elapsed records do not prove token completeness.
    pub tokens: Option<u64>,
    pub retained: Vec<RetainedUsage>,
    pub limits: Option<Limits>,
}
impl Observation {
    fn unknown(reason: &str) -> Self {
        Self {
            schema_version: 1,
            reason_code: Some(reason.into()),
            model_admissions: None,
            wall_ms: None,
            tokens: None,
            retained: vec![],
            limits: None,
        }
    }
    pub(crate) fn ready(&self) -> bool {
        self.reason_code.is_none()
    }
}
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

/// Shared read-only budget predicate. Callers must also enforce actual authority,
/// manifest/dependencies, child holds and capacity. incoming=0 observes/explains;
/// it does not authorize publication/merge or block unpaid reconciliation.
pub(crate) fn observe(
    conn: &Connection,
    workflow: &str,
    intent: &Intent,
    incoming: u64,
) -> Result<Observation> {
    match intent {
        Intent::Independent { request_ref } => {
            ensure!(
                table(conn, "workflow_family_budget")?,
                "family_authority_unknown"
            );
            let root: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_family_budget WHERE root_workflow=?1)",
                [workflow],
                |r| r.get(0),
            )?;
            let allowed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_family_member m JOIN command_request r ON r.request_id=m.independent_ref WHERE m.workflow=?1 AND m.independent_ref=?2 AND json_extract(r.admission,'$.workflow')=?1)",params![workflow,request_ref],|r|r.get(0))?;
            ensure!(!root && allowed, "family_execution_scope_unknown");
            Ok(Observation {
                schema_version: 1,
                reason_code: None,
                model_admissions: None,
                wall_ms: None,
                tokens: None,
                retained: vec![],
                limits: None,
            })
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
                return Ok(Observation::unknown("family_authority_unknown"));
            };
            let blocked = |reason: &str| -> Result<Observation> {
                let mut result = Observation::unknown(reason);
                result.retained = usage::reported(conn, family)?;
                Ok(result)
            };
            let member:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_family_member WHERE family=?1 AND workflow=?2)",params![family,workflow],|r|r.get(0))?;
            if !member || &request != request_ref {
                return blocked("family_execution_scope_unknown");
            }
            if root != workflow && owner_control::read(conn, &root)?.is_some_and(|c| c.paused) {
                return blocked("owner_paused");
            }
            // Membership comes first: pending policy must not vanish in an
            // active-policy-only lookup.
            if state != "bound" {
                return blocked("family_membership_pending");
            }
            let members: u64 = conn.query_row(
                "SELECT count(*) FROM workflow_family_member WHERE family=?1",
                [family],
                |r| r.get(0),
            )?;
            if !(2..=6).contains(&count)
                || members != count + 1
                || manifest.as_deref() != Some(manifest_ref)
            {
                return blocked("family_authority_unknown");
            }
            let Some(active) = active else {
                return blocked("family_budget_policy_missing");
            };
            if &active != policy_ref {
                return blocked("family_execution_scope_unknown");
            }
            let raw: Option<String> = conn
                .query_row(
                    "SELECT payload FROM workflow_family_policy WHERE reference=?1 AND family=?2",
                    params![active, family],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                return blocked("family_budget_policy_missing");
            };
            let limits: Limits = match serde_json::from_str::<Limits>(&raw) {
                Ok(p) if p.validate().is_ok() => p,
                _ => return blocked("family_budget_policy_unsupported"),
            };
            let mut result = usage::aggregate(conn, family)?;
            result.limits = Some(limits.clone());
            if result.ready()
                && incoming > 0
                && (result
                    .model_admissions
                    .and_then(|n| n.checked_add(incoming))
                    .is_none_or(|n| n > limits.max_model_admissions)
                    || result.wall_ms.is_none_or(|n| n >= limits.max_wall_ms))
            {
                result.reason_code = Some("family_resource_exhausted".into());
            }
            Ok(result)
        }
    }
}

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
    if let Intent::Delegated { family, .. } = &scope.intent {
        let root: String = tx.query_row(
            "SELECT root_workflow FROM workflow_family_budget WHERE family=?1",
            [family],
            |r| r.get(0),
        )?;
        ensure!(
            scope.root_control_revision == Some(revision(tx, &root)?),
            "family_execution_scope_unknown"
        );
    }
    let status = observe(tx, &lease.workflow, &scope.intent, incoming)?;
    ensure!(
        status.ready(),
        "{}",
        status
            .reason_code
            .as_deref()
            .unwrap_or("family_authority_unknown")
    );
    Ok(Some(format!("sha256:{:x}", Sha256::digest(raw.as_bytes()))))
}
