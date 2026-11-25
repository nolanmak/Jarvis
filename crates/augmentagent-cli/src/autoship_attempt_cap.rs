//! #2467 — per-issue attempt and spend caps, the give-up close-out, and the
//! global daily model-call budget.
//!
//! A try is one scope→build→verify cycle that ends without a merge, or one
//! exhausted acceptance-strategy cycle. Both are derived from the durable
//! workflow history, so workflows that predate the cap are counted the same
//! way. Spend is the workflow's charged model calls. Both count from the last
//! time an owner cleared a give-up (an issue edit or removing the
//! `agent-gave-up` label).
//!
//! When either cap is reached, productive claims stop at once and the
//! model-free give-up sweep closes the workflow out: one issue comment, the
//! `agent-gave-up` label, then retirement, which releases its native sessions
//! through the ordinary terminal path. The close-out is terminal, never a
//! hold: nothing here waits on a person.
//!
//! The global budget counts every charged model call in the rolling 24 hours.
//! While it is spent no productive claim and no new model admission is made;
//! merge reconciliation and finalize claim without spending and keep running.
use super::*;

/// Full tries per issue before autoship gives up on it.
pub(crate) const DEFAULT_MAX_TRIES: u32 = 3;
/// Model calls per issue before autoship gives up on it.
pub(crate) const DEFAULT_MAX_ISSUE_MODEL_CALLS: u64 = 300;
/// Model calls across every workflow per rolling 24 hours.
pub(crate) const DEFAULT_GLOBAL_DAILY_MODEL_CALLS: u64 = 1_500;
const MAX_TRIES_ENV: &str = "AUTOSHIP_MAX_TRIES";
const MAX_ISSUE_CALLS_ENV: &str = "AUTOSHIP_MAX_ISSUE_MODEL_CALLS";
const GLOBAL_DAILY_ENV: &str = "AUTOSHIP_GLOBAL_DAILY_MODEL_CALLS";

/// Give-up reasons. Each is also the retired checkpoint's reason.
pub(crate) const ATTEMPTS_REASON: &str = "attempt_cap_reached";
pub(crate) const CALLS_REASON: &str = "model_call_cap_reached";
/// The acceptance ladder already published observable children once.
pub(crate) const DECOMPOSITION_SPENT_REASON: &str = "observable_decomposition_spent";
/// Checkpoint action of a stop the give-up sweep closes out. Not a wait.
pub(crate) const GIVE_UP_ACTION: &str = "give_up";
/// Reason of the checkpoint a cleared give-up starts again from.
pub(crate) const CLEARED_REASON: &str = "give_up_cleared";
const DECOMPOSED_REASON: &str = "acceptance_decomposed_into_observable_children";
const CYCLE_KIND: &str = "acceptance_strategy_cycle_exhausted";
const BLOCKERS: usize = 3;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS workflow_give_up(
        workflow TEXT PRIMARY KEY REFERENCES workflow(id), reason TEXT NOT NULL,
        summary TEXT NOT NULL, request TEXT, state TEXT NOT NULL, decided_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS workflow_give_up_clear(
        workflow TEXT PRIMARY KEY REFERENCES workflow(id), cleared_at INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS workflow_event_kind_at ON workflow_event(kind,at_ms);
    CREATE INDEX IF NOT EXISTS workflow_event_workflow_kind ON workflow_event(workflow,kind);";

fn positive<T: std::str::FromStr + PartialOrd + Default>(env: &str, default: T) -> T {
    std::env::var(env)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .filter(|v| *v > T::default())
        .unwrap_or(default)
}
/// Configurable; a malformed or zero value cannot disable a cap.
pub(crate) fn max_tries() -> u32 {
    positive(MAX_TRIES_ENV, DEFAULT_MAX_TRIES)
}
pub(crate) fn max_issue_model_calls() -> u64 {
    positive(MAX_ISSUE_CALLS_ENV, DEFAULT_MAX_ISSUE_MODEL_CALLS)
}
pub(crate) fn global_daily_model_calls() -> u64 {
    positive(GLOBAL_DAILY_ENV, DEFAULT_GLOBAL_DAILY_MODEL_CALLS)
}

/// The caps one decision is made against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    pub tries: u32,
    pub issue_calls: u64,
    pub global_calls: u64,
}
impl Caps {
    pub(crate) fn configured() -> Self {
        Self { tries: max_tries(), issue_calls: max_issue_model_calls(), global_calls: global_daily_model_calls() }
    }
}

/// What an issue has used since its last cleared give-up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Usage {
    /// Scope→build→verify cycles started.
    pub builds: u32,
    /// Exhausted acceptance-strategy cycles, with their strategy names.
    pub ladder_cycles: u32,
    pub strategies: Vec<String>,
    pub calls: u64,
    /// Furthest stage reached.
    pub furthest: Option<Stage>,
    /// Most recent distinct blocker reasons, newest first.
    pub blockers: Vec<String>,
    pub since_ms: i64,
}
impl Usage {
    pub(crate) fn tries(&self) -> u32 {
        self.builds.saturating_add(self.ladder_cycles)
    }
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))?)
}

fn cleared_at(conn: &Connection, workflow: &str) -> Result<i64> {
    if !table_exists(conn, "workflow_give_up_clear")? {
        return Ok(0);
    }
    Ok(conn
        .query_row("SELECT cleared_at FROM workflow_give_up_clear WHERE workflow=?1", [workflow], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}

fn in_try(stage: Stage) -> bool {
    matches!(stage, Stage::Build | Stage::Verify | Stage::Review | Stage::Publish | Stage::AwaitChecks | Stage::Merge)
}
fn rank(stage: Stage) -> u8 {
    match stage {
        Stage::Intake => 0,
        Stage::Scope => 1,
        Stage::Held | Stage::Reconcile | Stage::Retired => 2,
        Stage::Build => 3,
        Stage::Verify => 4,
        Stage::Review => 5,
        Stage::Publish => 6,
        Stage::AwaitChecks => 7,
        Stage::Merge | Stage::Merged => 8,
    }
}

/// Durable usage of `workflow` since its last cleared give-up.
pub(crate) fn usage(conn: &Connection, workflow: &str) -> Result<Usage> {
    let since = cleared_at(conn, workflow)?;
    let mut usage = Usage { since_ms: since, ..Usage::default() };
    usage.calls = if since == 0 {
        conn.query_row("SELECT model_calls FROM workflow WHERE id=?1", [workflow], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0)
            .max(0) as u64
    } else {
        conn.query_row(
            "SELECT COALESCE(SUM(c.model_calls),0) FROM workflow_event e
             JOIN resource_charge c ON c.workflow=e.workflow AND c.id=e.payload
             WHERE e.workflow=?1 AND e.kind='resource_charged' AND e.at_ms>?2",
            params![workflow, since],
            |r| r.get::<_, i64>(0),
        )?
        .max(0) as u64
    };
    let mut q = conn.prepare(
        "SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='checkpoint' AND at_ms>?2 ORDER BY seq",
    )?;
    let rows = q.query_map(params![workflow, since], |r| r.get::<_, String>(0))?;
    let mut trying = false;
    let mut reasons = Vec::new();
    for raw in rows {
        let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw?) else { continue };
        if in_try(cp.stage) {
            if !trying {
                usage.builds = usage.builds.saturating_add(1);
                trying = true;
            }
        } else if matches!(cp.stage, Stage::Intake | Stage::Scope) {
            trying = false;
        }
        if usage.furthest.is_none_or(|f| rank(cp.stage) > rank(f)) {
            usage.furthest = Some(cp.stage);
        }
        if !matches!(cp.next.reason.as_str(), "registered" | CLEARED_REASON) {
            reasons.push(cp.next.reason);
        }
    }
    for reason in reasons.into_iter().rev() {
        if usage.blockers.len() == BLOCKERS {
            break;
        }
        if !usage.blockers.contains(&reason) {
            usage.blockers.push(reason);
        }
    }
    let mut q = conn.prepare(
        "SELECT r.response FROM scope_stage_response r WHERE r.workflow=?1
           AND instr(r.response,'\"kind\":\"acceptance_strategy_cycle_exhausted\"')>0
           AND (?2=0 OR EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=r.workflow
                AND e.kind='scope_stage_response_recorded' AND e.at_ms>?2
                AND json_extract(e.payload,'$.key')=r.key))",
    )?;
    let rows = q.query_map(params![workflow, since], |r| r.get::<_, String>(0))?;
    for raw in rows {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw?) else { continue };
        if value["kind"] != CYCLE_KIND {
            continue;
        }
        usage.ladder_cycles = usage.ladder_cycles.saturating_add(1);
        if let Some(name) = value["strategy"].as_str() {
            usage.strategies.push(name.to_owned());
        }
    }
    Ok(usage)
}

/// Whether the acceptance ladder already published observable children since
/// the last cleared give-up. Decomposition runs at most once per issue.
pub(crate) fn decomposed(conn: &Connection, workflow: &str) -> Result<bool> {
    let since = cleared_at(conn, workflow)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='checkpoint' AND at_ms>?2
           AND json_extract(payload,'$.next.reason')=?3)",
        params![workflow, since, DECOMPOSED_REASON],
        |r| r.get(0),
    )?)
}

/// Issue workflows (`owner/repo#N`) carry the per-issue caps; PR adoptions
/// and diagnostic namespaces only the global budget.
pub(crate) fn issue_number(workflow: &str) -> Option<u64> {
    let (repo, number) = workflow.rsplit_once('#')?;
    (!repo.is_empty() && !repo.contains("/pr/")).then_some(())?;
    number.parse().ok().filter(|n| *n > 0)
}

/// Charged model calls across every workflow in the rolling 24 hours.
pub(crate) fn global_charges(conn: &Connection, now: i64) -> Result<Vec<(i64, u64)>> {
    let mut q = conn.prepare(
        "SELECT e.at_ms,c.model_calls FROM workflow_event e
         JOIN resource_charge c ON c.workflow=e.workflow AND c.id=e.payload
         WHERE e.kind='resource_charged' AND e.at_ms>?1 AND c.model_calls>0",
    )?;
    let rows = q
        .query_map([now - retry_policy::DAY_MS], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(|(at, calls)| (at, u64::try_from(calls).unwrap_or(0))).collect())
}

/// The global budget as health and admission read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalModelBudget {
    pub used: u64,
    pub cap: u64,
    /// When the rolling window frees a call again; `None` while calls remain.
    pub reopens_at_ms: Option<i64>,
}
impl GlobalModelBudget {
    pub fn remaining(&self) -> u64 {
        self.cap.saturating_sub(self.used)
    }
    pub fn exhausted(&self) -> bool {
        self.used >= self.cap
    }
}
pub(crate) fn global_budget(conn: &Connection, now: i64, cap: u64) -> Result<GlobalModelBudget> {
    let charges = global_charges(conn, now)?;
    let used = charges.iter().map(|(_, c)| *c).sum();
    Ok(GlobalModelBudget { used, cap, reopens_at_ms: retry_policy::budget_reopens_at(&charges, now, cap) })
}

fn give_up_row_exists(conn: &Connection, workflow: &str) -> Result<bool> {
    if !table_exists(conn, "workflow_give_up")? {
        return Ok(false);
    }
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_give_up WHERE workflow=?1)", [workflow], |r| r.get(0))?)
}

/// Why a NEW model admission for `workflow` is refused, if it is: the global
/// budget is spent, the issue is given up, or its spend cap is reached.
pub(crate) fn admission_refusal(conn: &Connection, workflow: &str, now: i64, caps: Caps) -> Result<Option<String>> {
    let global = global_budget(conn, now, caps.global_calls)?;
    if global.exhausted() {
        return Ok(Some(format!(
            "global_model_budget_exhausted: {} of {} model calls used in the rolling 24 hours",
            global.used, global.cap
        )));
    }
    if issue_number(workflow).is_none() {
        return Ok(None);
    }
    if give_up_row_exists(conn, workflow)? {
        return Ok(Some("issue_given_up: autoship gave up on this issue".into()));
    }
    let usage = usage(conn, workflow)?;
    Ok((usage.calls >= caps.issue_calls).then(|| {
        format!("{CALLS_REASON}: {} of {} model calls spent on this issue", usage.calls, caps.issue_calls)
    }))
}

/// The give-up reason `workflow` has reached at `checkpoint`, if any. A try
/// in flight is allowed to finish; the attempt cap closes the issue only
/// when it would start another. Merge-path stages never give up.
pub(crate) fn give_up_reason(conn: &Connection, workflow: &str, checkpoint: &Checkpoint, caps: Caps) -> Result<Option<String>> {
    if issue_number(workflow).is_none()
        || matches!(checkpoint.stage, Stage::Merged | Stage::Retired | Stage::Publish | Stage::AwaitChecks | Stage::Merge)
    {
        return Ok(None);
    }
    if checkpoint.next.action == GIVE_UP_ACTION {
        return Ok(Some(checkpoint.next.reason.clone()));
    }
    let usage = usage(conn, workflow)?;
    if usage.calls >= caps.issue_calls {
        return Ok(Some(CALLS_REASON.into()));
    }
    if usage.tries() >= caps.tries && matches!(checkpoint.stage, Stage::Intake | Stage::Scope) {
        return Ok(Some(ATTEMPTS_REASON.into()));
    }
    Ok(None)
}

/// Whether a productive claim of `workflow` at `checkpoint` is refused. Merge
/// reconciliation and finalize never ask: they use their own claim purpose.
pub(crate) fn productive_claim_refused(conn: &Connection, workflow: &str, checkpoint: &Checkpoint, now: i64, caps: Caps) -> Result<bool> {
    if matches!(checkpoint.stage, Stage::AwaitChecks | Stage::Merge) {
        return Ok(false);
    }
    Ok(admission_refusal(conn, workflow, now, caps)?.is_some() || give_up_reason(conn, workflow, checkpoint, caps)?.is_some())
}

/// One durable give-up decision and how far its close-out got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GiveUp {
    pub workflow: String,
    pub issue: u64,
    pub reason: String,
    pub usage: Usage,
    /// The issue's request hash when decided or first observed after it.
    pub request: Option<String>,
    pub state: GiveUpState,
    pub decided_at: i64,
    pub pr: Option<u64>,
    pub total_calls: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GiveUpState {
    Decided,
    /// The comment was sent; whether GitHub recorded it is unknown.
    Commenting,
    Commented,
    Labelled,
    Retired,
}
impl GiveUpState {
    fn name(self) -> &'static str {
        match self {
            Self::Decided => "decided",
            Self::Commenting => "commenting",
            Self::Commented => "commented",
            Self::Labelled => "labelled",
            Self::Retired => "retired",
        }
    }
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "decided" => Self::Decided,
            "commenting" => Self::Commenting,
            "commented" => Self::Commented,
            "labelled" => Self::Labelled,
            "retired" => Self::Retired,
            _ => bail!("unknown give-up state {raw}"),
        })
    }
}

/// Marker that identifies one give-up's comment, for crash reconciliation.
pub(crate) fn comment_marker(give_up: &GiveUp) -> String {
    format!("<!-- autoship-give-up:{}:{} -->", give_up.workflow, give_up.decided_at)
}

/// The detail the give-up comment carries: what was tried, the last
/// blockers, and the calls spent. Machine reason codes only.
pub(crate) fn comment_detail(give_up: &GiveUp, caps: Caps) -> String {
    let u = &give_up.usage;
    let why = match give_up.reason.as_str() {
        CALLS_REASON => format!("this issue reached its model-call cap ({} of {}).", u.calls, caps.issue_calls),
        ATTEMPTS_REASON => format!("this issue used all {} of its tries without a merge.", caps.tries),
        DECOMPOSITION_SPENT_REASON => "acceptance recovery already split this issue into observable children once, and it failed again.".into(),
        other => format!("`{other}`."),
    };
    let mut tried = format!(
        "{} scope→build→verify cycle(s) (furthest stage: {})",
        u.builds,
        u.furthest.map(|s| serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default())
            .unwrap_or_else(|| "intake".into())
    );
    if u.ladder_cycles > 0 {
        tried.push_str(&format!(", {} acceptance strategy cycle(s) ({})", u.ladder_cycles, u.strategies.join(", ")));
    }
    if let Some(pr) = give_up.pr {
        tried.push_str(&format!("; PR #{pr} is left open for a human"));
    }
    let blockers = if u.blockers.is_empty() {
        "none recorded".to_owned()
    } else {
        u.blockers.iter().map(|b| format!("`{b}`")).collect::<Vec<_>>().join(", ")
    };
    format!(
        "Autoship stopped working on this issue: {why}\n\n\
         - Tried: {tried}. {} of {} tries used.\n\
         - Last blockers: {blockers}.\n\
         - Model calls spent: {} since the last reset ({} in total; cap {}).\n\n\
         The workflow is retired and its native sessions are released. Editing the issue or removing the \
         `agent-gave-up` label puts it back in the pool with a fresh count.\n{}",
        u.tries(),
        caps.tries,
        u.calls,
        give_up.total_calls,
        caps.issue_calls,
        comment_marker(give_up)
    )
}

fn read_give_up(conn: &Connection, workflow: &str) -> Result<Option<GiveUp>> {
    if !table_exists(conn, "workflow_give_up")? {
        return Ok(None);
    }
    let row: Option<(String, String, Option<String>, String, i64)> = conn
        .query_row(
            "SELECT reason,summary,request,state,decided_at FROM workflow_give_up WHERE workflow=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((reason, summary, request, state, decided_at)) = row else { return Ok(None) };
    let summary: serde_json::Value = serde_json::from_str(&summary)?;
    Ok(Some(GiveUp {
        workflow: workflow.into(),
        issue: issue_number(workflow).context("give-up row is not an issue workflow")?,
        reason,
        usage: serde_json::from_value(summary["usage"].clone())?,
        request,
        state: GiveUpState::parse(&state)?,
        decided_at,
        pr: summary["pr"].as_u64(),
        total_calls: summary["total_calls"].as_u64().unwrap_or(0),
    }))
}

impl WorkflowStore {
    /// The global rolling-day model budget.
    pub fn global_model_budget(&self, now: i64) -> Result<GlobalModelBudget> {
        global_budget(&self.conn, now, global_daily_model_calls())
    }
    /// Usage of one workflow since its last cleared give-up.
    pub(crate) fn attempt_usage(&self, workflow: &str) -> Result<Usage> {
        usage(&self.conn, workflow)
    }
    pub(crate) fn observable_decomposition_spent(&self, workflow: &str) -> Result<bool> {
        decomposed(&self.conn, workflow)
    }
    /// Why a new model admission is refused, if it is.
    pub(crate) fn paid_admission_refusal(&self, workflow: &str, now: i64) -> Result<Option<String>> {
        admission_refusal(&self.conn, workflow, now, Caps::configured())
    }
    pub(crate) fn give_up(&self, workflow: &str) -> Result<Option<GiveUp>> {
        read_give_up(&self.conn, workflow)
    }

    /// Unleased, unfinished issue workflows of `repository` that reached a
    /// cap or whose close-out is unfinished, oldest decision first.
    pub(crate) fn give_ups_due(&self, repository: &str, now: i64, caps: Caps) -> Result<Vec<(String, String)>> {
        let repository = repository.to_ascii_lowercase();
        let mut q = self.conn.prepare(
            "SELECT w.id,w.checkpoint,g.reason,g.state FROM workflow w LEFT JOIN workflow_give_up g ON g.workflow=w.id
             WHERE instr(w.id,?1||'#')=1 AND w.lease_until<=?2 ORDER BY COALESCE(g.decided_at,w.progress_at),w.id",
        )?;
        let rows = q
            .query_map(params![repository, now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut due = Vec::new();
        for (id, raw, decided, state) in rows {
            if let (Some(reason), Some(state)) = (decided, state) {
                if state != GiveUpState::Retired.name() {
                    due.push((id, reason));
                }
                continue;
            }
            let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
            if owner_control::read(&self.conn, &id)?.is_some_and(|c| c.paused) {
                continue;
            }
            if let Some(reason) = give_up_reason(&self.conn, &id, &checkpoint, caps)? {
                due.push((id, reason));
            }
        }
        Ok(due)
    }

    /// Record the give-up decision once. From here on productive claims and
    /// new model admissions for the workflow are refused.
    pub(crate) fn decide_give_up(&mut self, workflow: &str, reason: &str, request: Option<&str>, now: i64) -> Result<GiveUp> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_give_up WHERE workflow=?1)", [workflow], |r| r.get(0))?;
        if !exists {
            ensure!(issue_number(workflow).is_some(), "only issue workflows give up");
            let (pr, total): (Option<i64>, i64) =
                tx.query_row("SELECT pr,model_calls FROM workflow WHERE id=?1", [workflow], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let summary = serde_json::json!({"usage": usage(&tx, workflow)?, "pr": pr, "total_calls": total});
            tx.execute(
                "INSERT INTO workflow_give_up(workflow,reason,summary,request,state,decided_at,updated_at) VALUES(?1,?2,?3,?4,'decided',?5,?5)",
                params![workflow, reason, summary.to_string(), request, now],
            )?;
            let generation: i64 = tx.query_row("SELECT generation FROM workflow WHERE id=?1", [workflow], |r| r.get(0))?;
            event(&tx, workflow, generation, "give_up_decided", &serde_json::json!({"reason":reason,"summary":summary}).to_string(), now)?;
        }
        tx.commit()?;
        read_give_up(&self.conn, workflow)?.context("give-up decision missing")
    }

    pub(crate) fn set_give_up_state(&mut self, workflow: &str, state: GiveUpState, now: i64) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE workflow_give_up SET state=?2,updated_at=?3 WHERE workflow=?1",
            params![workflow, state.name(), now],
        )?;
        ensure!(changed == 1, "give-up decision missing");
        Ok(())
    }
    /// Record the request hash first observed after the decision.
    pub(crate) fn note_give_up_request(&mut self, workflow: &str, request: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE workflow_give_up SET request=?2 WHERE workflow=?1 AND request IS NULL",
            params![workflow, request],
        )?;
        Ok(())
    }

    /// Retire a given-up workflow. Terminal: the native owners are queued for
    /// the ordinary terminal retirement and no lease is held. `false` while a
    /// lease or an unresolved operation still holds it.
    pub(crate) fn retire_given_up(&mut self, workflow: &str, now: i64) -> Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reason: Option<String> =
            tx.query_row("SELECT reason FROM workflow_give_up WHERE workflow=?1", [workflow], |r| r.get(0)).optional()?;
        let reason = reason.context("give-up decision missing")?;
        let (raw, leased, generation): (String, i64, i64) = tx.query_row(
            "SELECT checkpoint,lease_until,generation FROM workflow WHERE id=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        if checkpoint.stage == Stage::Retired {
            tx.execute("UPDATE workflow_give_up SET state='retired',updated_at=?2 WHERE workflow=?1", params![workflow, now])?;
            tx.commit()?;
            return Ok(true);
        }
        ensure!(checkpoint.stage != Stage::Merged, "merged workflow cannot give up");
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)", [workflow], |r| r.get(0))?;
        if leased > now || pending || merge_allowance::merge_reconciliation_needed(&tx, workflow)? {
            return Ok(false);
        }
        checkpoint.stage = Stage::Retired;
        checkpoint.next = NextAction { reason: reason.clone(), action: "complete".into(), eligible_at_ms: 0 };
        let evidence = format!("give-up:{reason}");
        if checkpoint.evidence.len() < 128 && !checkpoint.evidence.contains(&evidence) {
            checkpoint.evidence.push(evidence);
            checkpoint.evidence.sort();
        }
        let raw = serde_json::to_string(&checkpoint)?;
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1", params![workflow, raw, now])?;
        event(&tx, workflow, generation, "checkpoint", &raw, now)?;
        tx.execute(
            "UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'terminal_workflow') WHERE workflow=?1 AND retired_at IS NULL",
            [workflow],
        )?;
        tx.execute("UPDATE workflow_give_up SET state='retired',updated_at=?2 WHERE workflow=?1", params![workflow, now])?;
        tx.commit()?;
        Ok(true)
    }

    /// Given-up workflows of `repository` whose close-out reached the label,
    /// the ones an issue edit or a label removal can clear.
    pub(crate) fn clearable_give_ups(&self, repository: &str) -> Result<Vec<GiveUp>> {
        let repository = repository.to_ascii_lowercase();
        let mut q = self.conn.prepare(
            "SELECT workflow FROM workflow_give_up WHERE instr(workflow,?1||'#')=1 AND state IN ('labelled','retired') ORDER BY decided_at",
        )?;
        let ids = q.query_map([repository], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().filter_map(|id| read_give_up(&self.conn, id).transpose()).collect()
    }

    /// An owner cleared the give-up: the caps count afresh from `now` and a
    /// retired give-up starts again from intake, so the picker can take it.
    pub(crate) fn clear_give_up(&mut self, workflow: &str, why: &str, now: i64) -> Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reason: Option<String> =
            tx.query_row("SELECT reason FROM workflow_give_up WHERE workflow=?1", [workflow], |r| r.get(0)).optional()?;
        let Some(reason) = reason else { return Ok(false) };
        let (raw, leased, generation): (String, i64, i64) = tx.query_row(
            "SELECT checkpoint,lease_until,generation FROM workflow WHERE id=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if leased > now {
            return Ok(false);
        }
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        tx.execute("DELETE FROM workflow_give_up WHERE workflow=?1", [workflow])?;
        tx.execute(
            "INSERT INTO workflow_give_up_clear(workflow,cleared_at) VALUES(?1,?2) ON CONFLICT(workflow) DO UPDATE SET cleared_at=excluded.cleared_at",
            params![workflow, now],
        )?;
        if checkpoint.stage == Stage::Retired && checkpoint.next.reason == reason {
            checkpoint.stage = Stage::Intake;
            checkpoint.evidence.clear();
            checkpoint.next = NextAction { reason: CLEARED_REASON.into(), action: "scope".into(), eligible_at_ms: now };
            let raw = serde_json::to_string(&checkpoint)?;
            tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1", params![workflow, raw, now])?;
            event(&tx, workflow, generation, "checkpoint", &raw, now)?;
        }
        event(&tx, workflow, generation, "give_up_cleared", &serde_json::json!({"reason":reason,"why":why}).to_string(), now)?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "owner/repo#2467";
    const CAPS: Caps = Caps { tries: 3, issue_calls: 300, global_calls: 1_500 };

    fn store() -> (tempfile::TempDir, WorkflowStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        (dir, store)
    }
    fn binding() -> Binding {
        Binding { head: "a".repeat(40), contract: "contract".into(), evidence_hash: "env".into() }
    }
    fn step(store: &mut WorkflowStore, id: &str, stage: Stage, reason: &str, at: i64) {
        let lease = store.claim(id, "fixture", at, 1_000).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = stage;
        cp.next = NextAction { reason: reason.into(), action: "fixture".into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, at).unwrap();
        store.release(&lease, at).unwrap();
    }
    fn charge(store: &mut WorkflowStore, id: &str, receipt: &str, calls: u64, at: i64) {
        let lease = store.claim(id, "fixture", at, 1_000).unwrap().unwrap();
        store.charge(&lease, receipt, ResourceCharge { model_calls: calls, tokens: 1, wall_ms: 1 }, at).unwrap();
        store.release(&lease, at).unwrap();
    }
    /// One failed try: scope, build, verify, review, back to scope.
    fn failed_try(store: &mut WorkflowStore, id: &str, at: i64) {
        step(store, id, Stage::Scope, "scoped", at);
        step(store, id, Stage::Build, "building", at + 1);
        step(store, id, Stage::Verify, "verifying", at + 2);
        step(store, id, Stage::Build, "repairing", at + 3);
        step(store, id, Stage::Review, "review-rejected", at + 4);
        step(store, id, Stage::Scope, "rescope_remaining_findings", at + 5);
    }

    #[test]
    fn caps_have_defaults_and_cannot_be_disabled() {
        assert!(max_tries() > 0 && max_issue_model_calls() > 0 && global_daily_model_calls() > 0);
        assert_eq!((DEFAULT_MAX_TRIES, DEFAULT_MAX_ISSUE_MODEL_CALLS, DEFAULT_GLOBAL_DAILY_MODEL_CALLS), (3, 300, 1_500));
        assert_eq!(issue_number("owner/repo#7"), Some(7));
        assert_eq!(issue_number("owner/repo/pr/7"), None);
        assert_eq!(issue_number("autoship-observation/day"), None);
    }

    /// A try is a scope→build→verify cycle: repairs inside one cycle do not
    /// count again, and the cap closes the issue only between tries.
    #[test]
    fn tries_are_counted_durably_from_history_and_the_cap_waits_for_the_try_in_flight() {
        let (_dir, mut store) = store();
        store.register(ID, "branch", binding(), 1).unwrap();
        for k in 0..2 {
            failed_try(&mut store, ID, 10 + k * 10);
        }
        step(&mut store, ID, Stage::Build, "building", 40);
        let usage = store.attempt_usage(ID).unwrap();
        assert_eq!((usage.builds, usage.tries()), (3, 3));
        assert_eq!(usage.furthest, Some(Stage::Review));
        let building = store.snapshot(ID).unwrap().unwrap().checkpoint;
        assert_eq!(give_up_reason(&store.conn, ID, &building, CAPS).unwrap(), None, "the third try is still running");
        step(&mut store, ID, Stage::Scope, "own_pr_rescope_exhausted", 50);
        let scoped = store.snapshot(ID).unwrap().unwrap().checkpoint;
        assert_eq!(give_up_reason(&store.conn, ID, &scoped, CAPS).unwrap().as_deref(), Some(ATTEMPTS_REASON));
        assert!(productive_claim_refused(&store.conn, ID, &scoped, 60, CAPS).unwrap());
        assert_eq!(store.attempt_usage(ID).unwrap().blockers[0], "own_pr_rescope_exhausted");
        // Reopened after a restart, the count is the same.
        let path = _dir.path().join("workflow.db");
        drop(store);
        assert_eq!(WorkflowStore::open(&path).unwrap().attempt_usage(ID).unwrap().tries(), 3);
    }

    #[test]
    fn the_spend_cap_refuses_new_admissions_but_never_the_merge_path() {
        let (_dir, mut store) = store();
        store.register(ID, "branch", binding(), 1).unwrap();
        charge(&mut store, ID, "scope", 299, 2);
        assert_eq!(admission_refusal(&store.conn, ID, 3, CAPS).unwrap(), None);
        charge(&mut store, ID, "build", 1, 3);
        assert!(admission_refusal(&store.conn, ID, 4, CAPS).unwrap().unwrap().starts_with(CALLS_REASON));
        step(&mut store, ID, Stage::AwaitChecks, "checks_pending", 5);
        let waiting = store.snapshot(ID).unwrap().unwrap().checkpoint;
        assert!(!productive_claim_refused(&store.conn, ID, &waiting, 6, CAPS).unwrap(), "finalize must keep running");
        assert_eq!(give_up_reason(&store.conn, ID, &waiting, CAPS).unwrap(), None);
    }

    #[test]
    fn the_global_budget_is_a_rolling_window_across_every_workflow() {
        let (_dir, mut store) = store();
        let caps = Caps { global_calls: 10, ..CAPS };
        for (k, at) in [(1, 1_000), (2, 2_000)] {
            let id = format!("owner/repo#{k}");
            store.register(&id, "branch", binding(), at).unwrap();
            charge(&mut store, &id, "work", 5, at);
        }
        let now = 3_000;
        let budget = global_budget(&store.conn, now, caps.global_calls).unwrap();
        assert_eq!((budget.used, budget.remaining(), budget.exhausted()), (10, 0, true));
        assert_eq!(budget.reopens_at_ms, Some(1_000 + retry_policy::DAY_MS), "the oldest charge ages out first");
        store.register("owner/repo#3", "branch", binding(), now).unwrap();
        let fresh = store.snapshot("owner/repo#3").unwrap().unwrap().checkpoint;
        assert!(productive_claim_refused(&store.conn, "owner/repo#3", &fresh, now, caps).unwrap(), "no new paid work");
        assert!(admission_refusal(&store.conn, "owner/repo/pr/9", now, caps).unwrap().unwrap().starts_with("global_model_budget_exhausted"));
        let later = 1_000 + retry_policy::DAY_MS + 1;
        assert_eq!(global_budget(&store.conn, later, caps.global_calls).unwrap().used, 5);
        assert!(!productive_claim_refused(&store.conn, "owner/repo#3", &fresh, later, caps).unwrap());
    }

    #[test]
    fn retirement_is_terminal_releases_native_owners_and_an_owner_clear_restarts_the_count() {
        let (_dir, mut store) = store();
        store.register(ID, "branch", binding(), 1).unwrap();
        let lease = store.claim(ID, "worker", 2, 1_000).unwrap().unwrap();
        store.register_native_owner(&lease, "/tmp/audit", 2).unwrap();
        store.charge(&lease, "paid", ResourceCharge { model_calls: 400, tokens: 1, wall_ms: 1 }, 2).unwrap();
        store.release(&lease, 3).unwrap();
        let reason = store.give_ups_due("owner/repo", 4, CAPS).unwrap();
        assert_eq!(reason, vec![(ID.to_owned(), CALLS_REASON.to_owned())]);
        let decided = store.decide_give_up(ID, CALLS_REASON, Some("request-a"), 5).unwrap();
        assert_eq!(store.decide_give_up(ID, CALLS_REASON, None, 6).unwrap(), decided, "decided once");
        assert_eq!(decided.usage.calls, 400);
        assert!(comment_detail(&decided, CAPS).contains("400 since the last reset"));
        store.set_give_up_state(ID, GiveUpState::Labelled, 7).unwrap();
        assert!(store.retire_given_up(ID, 8).unwrap());
        let retired = store.snapshot(ID).unwrap().unwrap();
        assert_eq!((retired.checkpoint.stage, retired.checkpoint.next.reason.as_str(), retired.checkpoint.next.action.as_str()),
            (Stage::Retired, CALLS_REASON, "complete"));
        assert_eq!(retired.lease_until_ms, 0);
        assert_eq!(store.pending_native_retirements(9, 16).unwrap().len(), 1, "native sessions are released");
        assert!(store.give_ups_due("owner/repo", 10, CAPS).unwrap().is_empty(), "closed out once");
        assert!(store.claim_productive(ID, "next", 11, 100).unwrap().is_none());
        assert_eq!(store.clearable_give_ups("owner/repo").unwrap().len(), 1);
        assert!(store.clear_give_up(ID, "label_removed", 12).unwrap());
        let revived = store.snapshot(ID).unwrap().unwrap();
        assert_eq!((revived.checkpoint.stage, revived.checkpoint.next.reason.as_str()), (Stage::Intake, CLEARED_REASON));
        assert_eq!(store.attempt_usage(ID).unwrap(), Usage { since_ms: 12, ..Usage::default() });
        assert!(store.give_up(ID).unwrap().is_none());
        assert!(store.claim_productive(ID, "next", 13, 100).unwrap().is_some(), "cleared issue is claimable again");
    }

    #[test]
    fn a_leased_or_mid_operation_workflow_is_not_retired_until_it_drains() {
        let (_dir, mut store) = store();
        store.register(ID, "branch", binding(), 1).unwrap();
        store.decide_give_up(ID, ATTEMPTS_REASON, None, 2).unwrap();
        let lease = store.claim(ID, "worker", 3, 1_000).unwrap().unwrap();
        assert!(!store.retire_given_up(ID, 4).unwrap());
        store.release(&lease, 5).unwrap();
        assert!(store.retire_given_up(ID, 6).unwrap());
    }
}
