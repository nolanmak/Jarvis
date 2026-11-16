//! A budget refusal changes controller scheduling, not resource charges,
//! authorization, stage evidence or the meaningful-progress clock.
use super::*;

const OBSERVATION_MS: i64 = 300_000;
const MAX_RECORD: usize = 65_536;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wait {
    pub schema_version: u32,
    pub generation: i64,
    pub binding: Binding,
    pub prior_next: NextAction,
    pub observation: Observation,
}
fn active(stage: Stage) -> bool {
    !matches!(
        stage,
        Stage::Held | Stage::Merged | Stage::Retired | Stage::Merge
    )
}
fn validate(wait: &Wait, workflow: &str) -> Result<()> {
    wait.binding.validate()?;
    ensure!(
        wait.schema_version == 1
            && wait.generation > 0
            && wait.observation.schema_version == 1
            && wait.observation.workflow == workflow
            && !wait.observation.ready()
            && wait
                .observation
                .blockers
                .iter()
                .all(|b| b.workflow == workflow && b.next.eligible_at_ms.is_none()),
        "family budget wait evidence is invalid"
    );
    Ok(())
}
fn latest(conn: &Connection, workflow: &str) -> Result<Option<Wait>> {
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
        let blocker = observation
            .blockers
            .first()
            .context("missing family budget blocker")?;
        let same_wait = latest(&tx, &lease.workflow)?.filter(|prior| {
            prior.generation == lease.generation
                && prior.binding == *binding
                && prior.observation.intent == observation.intent
                && cp.next.reason == blocker.reason_code.as_str()
                && cp.next.action == blocker.next.action
        });
        if let Some(prior) = &same_wait {
            if serde_json::to_value(&prior.observation)? == serde_json::to_value(observation)? {
                return Ok(());
            }
        }
        let wait = Wait {
            schema_version: 1,
            generation: lease.generation,
            binding: binding.clone(),
            prior_next: same_wait
                .as_ref()
                .map_or_else(|| cp.next.clone(), |prior| prior.prior_next.clone()),
            observation: observation.clone(),
        };
        validate(&wait, &lease.workflow)?;
        let payload = serde_json::to_string(&wait)?;
        ensure!(
            payload.len() <= MAX_RECORD,
            "family budget wait evidence exceeds bound"
        );
        let eligible = if same_wait.is_some() {
            cp.next.eligible_at_ms
        } else {
            now.saturating_add(OBSERVATION_MS)
        };
        cp.next = NextAction {
            reason: blocker.reason_code.as_str().into(),
            action: blocker.next.action.clone(),
            eligible_at_ms: eligible,
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
        if !active(snapshot.checkpoint.stage) {
            return Ok(None);
        }
        let Some(wait) = latest(&self.conn, workflow)? else {
            return Ok(None);
        };
        let blocker = wait
            .observation
            .blockers
            .first()
            .context("missing family budget blocker")?;
        Ok((wait.generation == snapshot.generation
            && wait.binding == snapshot.checkpoint.binding
            && snapshot.checkpoint.next.reason == blocker.reason_code.as_str()
            && snapshot.checkpoint.next.action == blocker.next.action)
            .then_some(wait))
    }
}
