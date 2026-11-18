//! A budget refusal changes controller scheduling, not resource charges,
//! authorization, stage evidence or the meaningful-progress clock.
use super::*;

#[path = "autoship_family_budget_wait_evidence.rs"]
mod evidence;
pub(crate) use evidence::{refusal, ContextRefused};

const OBSERVATION_MS: i64 = 300_000;
const MAX_RECORD: usize = 65_536;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wait {
    pub schema_version: u32,
    pub generation: i64,
    pub binding: Binding,
    pub prior_next: NextAction,
    pub observation: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_reason: Option<observation::Reason>,
}
fn active(stage: Stage) -> bool {
    !matches!(
        stage,
        Stage::Held | Stage::Merged | Stage::Retired | Stage::Merge
    )
}
fn validate(wait: &Wait, workflow: &str) -> Result<()> {
    wait.binding.validate()?;
    ensure!(wait.generation > 0, "invalid family wait generation");
    wait.validate_evidence(workflow)
}
pub(super) fn latest(conn: &Connection, workflow: &str) -> Result<Option<Wait>> {
    let row:Option<(String,i64)>=conn.query_row("SELECT payload,generation FROM workflow_event WHERE workflow=?1 AND kind='family_budget_wait' ORDER BY seq DESC LIMIT 1",
        [workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((raw, generation)) = row else {
        return Ok(None);
    };
    ensure!(
        raw.len() <= MAX_RECORD,
        "family budget wait evidence exceeds bound"
    );
    let wait: Wait = serde_json::from_str(&raw)?;
    validate(&wait, workflow)?;
    ensure!(
        wait.generation == generation,
        "family budget wait generation differs"
    );
    Ok(Some(wait))
}
impl WorkflowStore {
    /// Protected native accounting callback only; caller still verifies teardown
    /// independently before releasing execution custody. The five-minute value
    /// is an unpaid observation deadline, never permission for a paid retry.
    pub(crate) fn defer_family_budget_wait(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        observation: &Observation,
        now: i64,
    ) -> Result<()> {
        self.defer_family_wait(lease, binding, Some(observation), None, now)
    }

    pub(crate) fn defer_family_context_wait(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        reason: observation::Reason,
        now: i64,
    ) -> Result<()> {
        self.defer_family_wait(lease, binding, None, Some(reason), now)
    }

    fn defer_family_wait(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        observation: Option<&Observation>,
        context_reason: Option<observation::Reason>,
        now: i64,
    ) -> Result<()> {
        ensure!(now >= 0, "invalid family wait time");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let mut cp: Checkpoint = serde_json::from_str(&raw)?;
        if !active(cp.stage) || owner_control::read(&tx, &lease.workflow)?.is_some_and(|c| c.paused)
        {
            return Ok(());
        }
        let prior = latest(&tx, &lease.workflow)?.filter(|prior| {
            prior.binding == *binding
                && prior.reason().is_ok_and(|reason| {
                    cp.next.reason == reason.as_str() && cp.next.action == reason.next().action
                })
        });
        let wait = Wait {
            schema_version: if observation.is_some() { 1 } else { 2 },
            generation: lease.generation,
            binding: binding.clone(),
            prior_next: prior
                .as_ref()
                .map_or_else(|| cp.next.clone(), |w| w.prior_next.clone()),
            observation: observation.cloned(),
            context_reason,
        };
        validate(&wait, &lease.workflow)?;
        if let Some(prior) = &prior {
            if prior.generation == lease.generation
                && prior.context_reason == context_reason
                && serde_json::to_value(&prior.observation)?
                    == serde_json::to_value(&wait.observation)?
            {
                return Ok(());
            }
        }
        let payload = serde_json::to_string(&wait)?;
        ensure!(
            payload.len() <= MAX_RECORD,
            "family budget wait evidence exceeds bound"
        );
        let reason = wait.reason()?;
        cp.next = NextAction {
            reason: reason.as_str().into(),
            action: reason.next().action,
            eligible_at_ms: if prior.is_some() {
                cp.next.eligible_at_ms
            } else {
                now.saturating_add(OBSERVATION_MS)
            },
        };
        let checkpoint = serde_json::to_string(&cp)?;
        tx.execute(
            "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
            params![lease.workflow, checkpoint],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "checkpoint",
            &checkpoint,
            now,
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "family_budget_wait",
            &payload,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Active same-generation/binding evidence only; a stale historical record
    /// is not a reason to stop a new run or to claim new authorization.
    pub(crate) fn family_budget_wait(&self, workflow: &str) -> Result<Option<Wait>> {
        let Some(snapshot) = self.snapshot(workflow)? else {
            return Ok(None);
        };
        current(&self.conn, workflow, snapshot.generation, &snapshot.checkpoint)
    }
}

/// Share exact active-wait recognition with transactional lease settlement.
pub(crate) fn current(conn: &Connection, workflow: &str, generation: i64, cp: &Checkpoint) -> Result<Option<Wait>> {
    if !active(cp.stage) { return Ok(None); }
    let Some(wait) = latest(conn, workflow)? else { return Ok(None); };
    let reason = wait.reason()?;
    Ok((wait.generation == generation && wait.binding == cp.binding
        && cp.next.reason == reason.as_str() && cp.next.action == reason.next().action)
        .then_some(wait))
}

/// Same protected-input calculation for selection and claim; never a grant.
pub(super) fn observe_wait(conn: &Connection, workflow: &str, generation: i64, wait: &Wait) -> Result<Option<Observation>> {
    // Use a replacement only when a trusted service already published it for
    // this generation. Never substitute a newly displayed policy for old intent.
    let raw: Option<String> = if table(conn, "workflow_family_scope")? {
        conn.query_row(
            "SELECT payload FROM workflow_family_scope WHERE workflow=?1 AND generation=?2",
            params![workflow, generation],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    // Reconciliation alone may have advanced the generation. Historical scope
    // is only an observation input; it never authorizes a new native dispatch.
    let raw =
        if raw.is_none() && generation != wait.generation && table(conn, "workflow_family_scope")? {
            conn.query_row(
                "SELECT payload FROM workflow_family_scope WHERE workflow=?1 AND generation=?2",
                params![workflow, wait.generation],
                |r| r.get(0),
            )
            .optional()?
        } else {
            raw
        };
    let scope = raw
        .and_then(|raw| serde_json::from_str::<Scope>(&raw).ok())
        .filter(|scope| scope.schema_version == 1 && !scope.verified_authority_ref.is_empty());
    let observation = if let Some(scope) = scope {
        observe_scope(conn, workflow, &scope, 1)?
    } else if let Some(known) = &wait.observation {
        let mut status = observe(conn, workflow, &known.intent, 1)?;
        status.block(observation::Reason::FamilyExecutionScopeUnknown);
        status
    } else {
        return Ok(None);
    };
    Ok(Some(observation))
}

/// Budget-only scheduling. A free reconciliation may rotate the lease without
/// resolving the wait, so match its checkpoint rather than only its generation.
/// Final native dispatch still requires a fresh, fully verified execution frame.
pub(crate) fn prepare_productive_claim(
    tx: &Transaction<'_>,
    workflow: &str,
    generation: i64,
    cp: &mut Checkpoint,
    now: i64,
) -> Result<bool> {
    let Some(wait) = latest(tx, workflow)? else {
        return Ok(true);
    };
    let reason = wait.reason()?;
    if !active(cp.stage)
        || wait.binding != cp.binding
        || cp.next.reason != reason.as_str()
        || cp.next.action != reason.next().action
    {
        return Ok(true);
    }
    let Some(observation) = observe_wait(tx, workflow, generation, &wait)? else {
        return Ok(false);
    };
    if !observation.ready() {
        if serde_json::to_value(Some(&observation))? != serde_json::to_value(&wait.observation)? {
            let blocker = observation
                .blockers
                .first()
                .context("missing family budget blocker")?;
            cp.next = NextAction {
                reason: blocker.reason_code.as_str().into(),
                action: blocker.next.action.clone(),
                eligible_at_ms: cp.next.eligible_at_ms,
            };
            let refreshed = Wait {
                schema_version: 1,
                generation,
                binding: cp.binding.clone(),
                prior_next: wait.prior_next.clone(),
                observation: Some(observation),
                context_reason: None,
            };
            validate(&refreshed, workflow)?;
            let payload = serde_json::to_string(&refreshed)?;
            ensure!(
                payload.len() <= MAX_RECORD,
                "family budget wait evidence exceeds bound"
            );
            let checkpoint = serde_json::to_string(cp)?;
            tx.execute(
                "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
                params![workflow, checkpoint],
            )?;
            event(tx, workflow, generation, "checkpoint", &checkpoint, now)?;
            event(
                tx,
                workflow,
                generation,
                "family_budget_wait",
                &payload,
                now,
            )?;
        }
        return Ok(false);
    }
    cp.next = wait.prior_next;
    let checkpoint = serde_json::to_string(cp)?;
    tx.execute(
        "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
        params![workflow, checkpoint],
    )?;
    event(tx, workflow, generation, "checkpoint", &checkpoint, now)?;
    event(
        tx,
        workflow,
        generation,
        "family_budget_wait_recovered",
        &serde_json::to_string(&observation)?,
        now,
    )?;
    Ok(true)
}
