//! Transaction-level writers for trusted T services. The caller must authenticate
//! current owner/repository authority and prove publication, scope and compatible
//! original child grants. Reference strings alone never establish those facts.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingFamily {
    pub family: String,
    pub repository: String,
    pub original_request_ref: String,
    pub source_ref: String,
    pub plan_ref: String,
    pub expected_children: u8,
    pub binding: Binding,
}
fn reference(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 512 && value.bytes().all(|b| b.is_ascii_graphic()),
        "invalid protected family reference"
    );
    Ok(())
}
fn canonical(repository: &str, workflow: &str) -> bool {
    for (prefix, pr) in [
        (format!("{repository}#"), false),
        (format!("{repository}/pr/"), true),
    ] {
        if let Some(number) = workflow
            .strip_prefix(&prefix)
            .and_then(|s| s.parse::<u64>().ok())
        {
            return if pr {
                pr_workflow_id(repository, number)
            } else {
                workflow_id(repository, number)
            }
            .is_ok_and(|id| id == workflow);
        }
    }
    false
}
fn initialize(tx: &Transaction<'_>) -> Result<()> {
    let count:i64=tx.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('workflow_family_budget','workflow_family_member','workflow_family_policy','workflow_family_scope')",[],|r|r.get(0))?;
    let column:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('workflow') WHERE name='family_budget_required')",[],|r|r.get(0))?;
    if count == 4 {
        ensure!(column, "family_authority_unknown");
        return Ok(());
    }
    ensure!(count == 0, "family_authority_unknown");
    if column {
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow WHERE family_budget_required=1)",
                [],
                |r| r.get::<_, bool>(0)
            )?,
            "family_authority_unknown"
        );
    } else {
        tx.execute_batch("ALTER TABLE workflow ADD COLUMN family_budget_required INTEGER NOT NULL DEFAULT 0 CHECK(family_budget_required IN (0,1));")?;
    }
    tx.execute_batch("CREATE TABLE workflow_family_budget (
        family TEXT PRIMARY KEY, root_workflow TEXT NOT NULL REFERENCES workflow(id), repository TEXT NOT NULL,
        original_request_ref TEXT NOT NULL, expected_children INTEGER NOT NULL CHECK(expected_children BETWEEN 2 AND 6),
        state TEXT NOT NULL CHECK(state IN ('pending','bound')), manifest_ref TEXT, policy_ref TEXT, payload TEXT NOT NULL,
        UNIQUE(repository,original_request_ref));
        CREATE TABLE workflow_family_member (
        family TEXT NOT NULL REFERENCES workflow_family_budget(family), workflow TEXT NOT NULL REFERENCES workflow(id),
        position INTEGER NOT NULL CHECK(position BETWEEN 0 AND 6), publication_ref TEXT, independent_ref TEXT,
        PRIMARY KEY(family,workflow), UNIQUE(family,position));
        CREATE TABLE workflow_family_policy (
        reference TEXT PRIMARY KEY, family TEXT NOT NULL REFERENCES workflow_family_budget(family),
        predecessor TEXT, verified_owner_authority_ref TEXT NOT NULL, payload TEXT NOT NULL);
        CREATE TABLE workflow_family_scope (
        workflow TEXT NOT NULL REFERENCES workflow(id), generation INTEGER NOT NULL, payload TEXT NOT NULL,
        PRIMARY KEY(workflow,generation));")?;
    Ok(())
}
fn root(
    tx: &Transaction<'_>,
    lease: &Lease,
    binding: &Binding,
    family: &str,
    now: i64,
) -> Result<(String, u8, String)> {
    require_binding(tx, lease, binding, now)?;
    let row:(String,String,u8,String)=tx.query_row("SELECT root_workflow,repository,expected_children,state FROM workflow_family_budget WHERE family=?1",[family],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(row.0 == lease.workflow, "family root lease mismatch");
    Ok((row.1, row.2, row.3))
}
/// Before public creation: pending root, no child/job/policy authority created.
pub(crate) fn begin(
    tx: &Transaction<'_>,
    lease: &Lease,
    input: &PendingFamily,
    now: i64,
) -> Result<()> {
    require_binding(tx, lease, &input.binding, now)?;
    let repository = submission::normalized_repository(&input.repository)?;
    ensure!(
        repository == input.repository
            && canonical(&repository, &lease.workflow)
            && lease.workflow.contains('#'),
        "invalid family root repository"
    );
    ensure!(
        (2..=6).contains(&input.expected_children),
        "family child count unsupported"
    );
    for value in [
        &input.family,
        &input.original_request_ref,
        &input.source_ref,
        &input.plan_ref,
    ] {
        reference(value)?;
    }
    initialize(tx)?;
    let payload = serde_json::to_string(input)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT root_workflow,payload FROM workflow_family_budget WHERE family=?1",
            [&input.family],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((old_root, old)) = old {
        ensure!(
            old_root == lease.workflow && old == payload,
            "immutable family conflict"
        );
        return Ok(());
    }
    ensure!(
        !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_family_member WHERE workflow=?1 AND position>0)",
            [&lease.workflow],
            |r| r.get::<_, bool>(0)
        )?,
        "recursive family forbidden"
    );
    tx.execute(
        "INSERT INTO workflow_family_budget VALUES (?1,?2,?3,?4,?5,'pending',NULL,NULL,?6)",
        params![
            input.family,
            lease.workflow,
            repository,
            input.original_request_ref,
            input.expected_children,
            payload
        ],
    )?;
    tx.execute(
        "INSERT INTO workflow_family_member VALUES (?1,?2,0,NULL,NULL)",
        params![input.family, lease.workflow],
    )?;
    tx.execute(
        "UPDATE workflow SET family_budget_required=1 WHERE id=?1",
        [&lease.workflow],
    )?;
    Ok(())
}
/// T must verify the publication receipt and any original independent authority
/// before staging. This API cannot infer them from a public marker/author name.
pub(crate) fn stage_member(
    tx: &Transaction<'_>,
    lease: &Lease,
    binding: &Binding,
    family: &str,
    position: u8,
    workflow: &str,
    publication_ref: &str,
    independent_ref: Option<&str>,
    now: i64,
) -> Result<()> {
    let (repository, count, state) = root(tx, lease, binding, family, now)?;
    ensure!(
        (1..=count).contains(&position) && canonical(&repository, workflow),
        "invalid family member"
    );
    reference(publication_ref)?;
    if let Some(request) = independent_ref {
        reference(request)?;
        ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM command_request WHERE request_id=?1 AND json_extract(admission,'$.workflow')=?2)",params![request,workflow],|r|r.get::<_,bool>(0))?,"independent child grant missing");
    }
    let prior:Option<(u8,String,Option<String>)>=tx.query_row("SELECT position,publication_ref,independent_ref FROM workflow_family_member WHERE family=?1 AND workflow=?2",params![family,workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some(prior) = prior {
        ensure!(
            prior
                == (
                    position,
                    publication_ref.into(),
                    independent_ref.map(str::to_owned)
                ),
            "immutable family member conflict"
        );
        return Ok(());
    }
    ensure!(state == "pending", "immutable family membership");
    ensure!(
        !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_family_budget WHERE root_workflow=?1)",
            [workflow],
            |r| r.get::<_, bool>(0)
        )?,
        "recursive family forbidden"
    );
    ensure!(
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1)",
            [workflow],
            |r| r.get::<_, bool>(0)
        )?,
        "family member unknown"
    );
    tx.execute(
        "INSERT INTO workflow_family_member VALUES (?1,?2,?3,?4,?5)",
        params![family, workflow, position, publication_ref, independent_ref],
    )?;
    tx.execute(
        "UPDATE workflow SET family_budget_required=1 WHERE id=?1",
        [workflow],
    )?;
    Ok(())
}
pub(crate) fn finalize(
    tx: &Transaction<'_>,
    lease: &Lease,
    binding: &Binding,
    family: &str,
    manifest_ref: &str,
    now: i64,
) -> Result<()> {
    let (_, count, _) = root(tx, lease, binding, family, now)?;
    reference(manifest_ref)?;
    let actual: u8 = tx.query_row(
        "SELECT count(*) FROM workflow_family_member WHERE family=?1",
        [family],
        |r| r.get(0),
    )?;
    ensure!(actual == count + 1, "family_membership_pending");
    let old: Option<String> = tx.query_row(
        "SELECT manifest_ref FROM workflow_family_budget WHERE family=?1",
        [family],
        |r| r.get(0),
    )?;
    ensure!(
        old.as_deref().is_none_or(|old| old == manifest_ref),
        "immutable family manifest conflict"
    );
    tx.execute(
        "UPDATE workflow_family_budget SET state='bound',manifest_ref=?2 WHERE family=?1",
        params![family, manifest_ref],
    )?;
    Ok(())
}
/// T must already have persisted/authenticated the exact explicit owner policy.
/// This internal writer is not an owner-authentication endpoint. No defaults.
pub(crate) fn activate_policy(
    tx: &Transaction<'_>,
    lease: &Lease,
    binding: &Binding,
    family: &str,
    expected: Option<&str>,
    verified_owner_ref: &str,
    limits: &Limits,
    now: i64,
) -> Result<String> {
    root(tx, lease, binding, family, now)?;
    reference(verified_owner_ref)?;
    limits.validate()?;
    let payload = serde_json::to_string(limits)?;
    let policy_ref = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            family,
            expected,
            verified_owner_ref,
            &payload
        ))?)
    );
    let active: Option<String> = tx.query_row(
        "SELECT policy_ref FROM workflow_family_budget WHERE family=?1",
        [family],
        |r| r.get(0),
    )?;
    if active.as_deref() == Some(&policy_ref) {
        return Ok(policy_ref);
    }
    ensure!(
        active.as_deref() == expected,
        "family policy predecessor conflict"
    );
    tx.execute(
        "INSERT INTO workflow_family_policy VALUES (?1,?2,?3,?4,?5)",
        params![policy_ref, family, expected, verified_owner_ref, payload],
    )?;
    tx.execute(
        "UPDATE workflow_family_budget SET policy_ref=?2 WHERE family=?1",
        params![family, policy_ref],
    )?;
    Ok(policy_ref)
}
/// Bind already verified funding intent to this lease generation. Changing it
/// requires a new lease; policy/pause revisions cannot silently expand a frame.
pub(crate) fn record_intent(
    tx: &Transaction<'_>,
    lease: &Lease,
    intent: &Intent,
    verified_authority_ref: &str,
    now: i64,
) -> Result<()> {
    require_lease(tx, lease, now)?;
    reference(verified_authority_ref)?;
    ensure!(
        required(tx, &lease.workflow)?,
        "family scope requires a protected family fence"
    );
    let status = observe(tx, &lease.workflow, intent, 0)?;
    ensure!(
        status.ready(),
        "{}",
        status
            .reason_code
            .as_deref()
            .unwrap_or("family_authority_unknown")
    );
    let revision = match intent {
        Intent::Independent { .. } => None,
        Intent::Delegated { family, .. } => {
            let root: String = tx.query_row(
                "SELECT root_workflow FROM workflow_family_budget WHERE family=?1",
                [family],
                |r| r.get(0),
            )?;
            Some(super::revision(tx, &root)?)
        }
    };
    let scope = Scope {
        schema_version: 1,
        intent: intent.clone(),
        verified_authority_ref: verified_authority_ref.into(),
        root_control_revision: revision,
    };
    let payload = serde_json::to_string(&scope)?;
    let old: Option<String> = tx
        .query_row(
            "SELECT payload FROM workflow_family_scope WHERE workflow=?1 AND generation=?2",
            params![lease.workflow, lease.generation],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        ensure!(old == payload, "immutable family execution scope conflict");
        return Ok(());
    }
    tx.execute(
        "INSERT INTO workflow_family_scope VALUES (?1,?2,?3)",
        params![lease.workflow, lease.generation, payload],
    )?;
    Ok(())
}

impl WorkflowStore {
    /// Internal T service boundary: keep lineage, policy and child admissions in
    /// one IMMEDIATE transaction. This does not authenticate the caller. Any
    /// returned error rolls back the complete batch, including newly staged
    /// family records; only a successful trusted operation commits.
    pub(crate) fn with_family_transaction<T>(
        &mut self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Budget-only read for trusted command scheduling/status. A positive
    /// result does not grant authority, capacity or an execution lease.
    pub(crate) fn family_budget_observation(
        &self,
        workflow: &str,
        intent: &Intent,
        incoming: u64,
    ) -> Result<Observation> {
        observe(&self.conn, workflow, intent, incoming)
    }
}
