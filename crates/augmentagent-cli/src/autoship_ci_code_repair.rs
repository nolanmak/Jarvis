//! Hosted CI code failures are repaired, never waited out as infrastructure (#2315).
//!
//! A compile error, failing test or stale-against-main head on autoship's own
//! PR used to be wrapped as "shared CI prerequisite unavailable" and parked in
//! `verification_infrastructure_retry` forever, because nothing ever merged
//! main or rebuilt the candidate. Code failures instead go to repair: the
//! resume path merges current main (merge commit), rebuilds and tests in the
//! verifier, repairs and pushes. Repeated rounds on one unchanged head pace
//! themselves with the #2301 backoff and come back on the existing
//! `review_main_changed / reconcile_main` cue. Nothing here is terminal.
use super::retry_policy::backoff_ms;
use super::*;

pub(crate) const EVENT: &str = "hosted_ci_code_repair";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ladder {
    /// Repair now; the value is the admitted round for this head.
    Repair(u32),
    /// Come back at this time on `review_main_changed / reconcile_main`.
    Wait(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Last {
    pub admitted: bool,
    pub eligible_at_ms: i64,
}

/// One rung of the never-stuck ladder for an unchanged head. `admitted` is the
/// number of repair rounds already run for it; `last` its newest decision.
/// The first round runs immediately when the caller can still act on it; every
/// later round waits a capped exponential backoff, then runs.
pub(crate) fn decide(admitted: u32, last: Option<Last>, now: i64, immediate: bool) -> Ladder {
    match last {
        Some(Last {
            admitted: false,
            eligible_at_ms,
        }) if now < eligible_at_ms => Ladder::Wait(eligible_at_ms),
        Some(Last {
            admitted: false, ..
        }) => Ladder::Repair(admitted + 1),
        _ if immediate && admitted == 0 => Ladder::Repair(1),
        // #2430: after an admitted round, back off from that round, not from
        // this (possibly much later) check. A late resume past the backoff
        // runs the next round now instead of waiting the full backoff again.
        Some(Last {
            admitted: true,
            eligible_at_ms: admitted_at,
        }) => {
            let due = admitted_at.saturating_add(backoff_ms(admitted.saturating_sub(1)));
            if now >= due {
                Ladder::Repair(admitted + 1)
            } else {
                Ladder::Wait(due)
            }
        }
        _ => Ladder::Wait(now.saturating_add(backoff_ms(admitted.saturating_sub(1)))),
    }
}

impl WorkflowStore {
    /// Admit or schedule a code-failure repair round for the bound head. A
    /// wait is checkpointed on the main-reconciliation cue, never on the
    /// infrastructure retry, so dispatch, status and fairness see a code
    /// failure that is being repaired.
    pub(crate) fn code_repair_round(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        reason: &str,
        immediate: bool,
        now: i64,
    ) -> Result<Ladder> {
        ensure!(
            !reason.is_empty() && reason.len() <= 128,
            "bounded code repair reason required"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        let admitted: u32 = tx.query_row(
            "SELECT COUNT(*) FROM workflow_event WHERE workflow=?1 AND kind=?2
             AND json_extract(payload,'$.head')=?3 AND json_extract(payload,'$.admitted')=1",
            params![lease.workflow, EVENT, binding.head],
            |r| r.get(0),
        )?;
        let last: Option<Last> = tx
            .query_row(
                "SELECT json_extract(payload,'$.admitted'),json_extract(payload,'$.eligible_at_ms')
                 FROM workflow_event WHERE workflow=?1 AND kind=?2
                 AND json_extract(payload,'$.head')=?3 ORDER BY seq DESC LIMIT 1",
                params![lease.workflow, EVENT, binding.head],
                |r| {
                    Ok(Last {
                        admitted: r.get::<_, i64>(0)? == 1,
                        eligible_at_ms: r.get(1)?,
                    })
                },
            )
            .optional()?;
        let decision = decide(admitted, last, now, immediate);
        let record = |admitted: bool, round: u32, eligible: i64| -> Result<()> {
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                EVENT,
                &serde_json::json!({"version":1,"head":binding.head,"reason":reason,
                    "admitted":admitted,"round":round,"eligible_at_ms":eligible})
                .to_string(),
                now,
            )
        };
        match decision {
            Ladder::Repair(round) => record(true, round, now)?,
            Ladder::Wait(eligible) => {
                // A still-pending wait needs no second schedule record.
                if !matches!(last, Some(Last { admitted: false, eligible_at_ms }) if eligible_at_ms == eligible)
                {
                    record(false, admitted + 1, eligible)?;
                }
                let raw: String = tx.query_row(
                    "SELECT checkpoint FROM workflow WHERE id=?1",
                    [&lease.workflow],
                    |r| r.get(0),
                )?;
                let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
                checkpoint.stage = Stage::Reconcile;
                checkpoint.next = NextAction {
                    reason: "review_main_changed".into(),
                    action: "reconcile_main".into(),
                    eligible_at_ms: eligible,
                };
                let checkpoint = serde_json::to_string(&checkpoint)?;
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
            }
        }
        tx.commit()?;
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIN: i64 = 60_000;

    #[test]
    fn first_code_failure_repairs_now_then_rounds_back_off_and_never_park() {
        assert_eq!(decide(0, None, 0, true), Ladder::Repair(1));
        // The same head is still red after a round: wait 30m from that
        // round (#2430), then repair.
        let wait = decide(
            1,
            Some(Last {
                admitted: true,
                eligible_at_ms: 0,
            }),
            10,
            true,
        );
        assert_eq!(wait, Ladder::Wait(30 * MIN));
        assert_eq!(
            decide(
                1,
                Some(Last {
                    admitted: false,
                    eligible_at_ms: 30 * MIN
                }),
                29 * MIN,
                true
            ),
            Ladder::Wait(30 * MIN)
        );
        assert_eq!(
            decide(
                1,
                Some(Last {
                    admitted: false,
                    eligible_at_ms: 30 * MIN
                }),
                30 * MIN,
                true
            ),
            Ladder::Repair(2)
        );
        // Backoff doubles and caps at six hours; the ladder always comes back.
        assert_eq!(
            decide(
                2,
                Some(Last {
                    admitted: true,
                    eligible_at_ms: 0
                }),
                0,
                true
            ),
            Ladder::Wait(60 * MIN)
        );
        assert_eq!(
            decide(
                40,
                Some(Last {
                    admitted: true,
                    eligible_at_ms: 0
                }),
                0,
                true
            ),
            Ladder::Wait(360 * MIN)
        );
        // A caller that cannot act now (exhausted or unchanged repair) waits first.
        assert_eq!(decide(0, None, 5, false), Ladder::Wait(5 + 30 * MIN));
    }

    /// #2430 production shape (#2169 / PR #2305): round 3 was admitted at
    /// 2026-11-21 03:16Z, the next resume came 21 hours later, and the ladder
    /// answered "wait 2 more hours" because the backoff ran from the check,
    /// not from the round. Past the backoff, the next round runs now.
    #[test]
    fn backoff_runs_from_the_admitted_round_not_from_the_late_check() {
        let round3 = 1_795_231_014_339;
        let late = round3 + 21 * 60 * MIN;
        let last = Some(Last { admitted: true, eligible_at_ms: round3 });
        assert_eq!(decide(3, last, late, false), Ladder::Repair(4), "a late resume past the backoff repairs now");
        // Inside the backoff the wait still ends at round3 + backoff, not later.
        assert_eq!(decide(3, last, round3 + MIN, false), Ladder::Wait(round3 + 120 * MIN));
    }

    #[test]
    fn code_repair_wait_is_a_main_reconciliation_not_an_infrastructure_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("w.db")).unwrap();
        let binding = Binding {
            head: "a".repeat(40),
            contract: "b".repeat(64),
            evidence_hash: "c".repeat(64),
        };
        store
            .register("owner/repo#1", "candidate", binding.clone(), 0)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "worker", 1, 10 * 60 * MIN)
            .unwrap()
            .unwrap();
        let mut checkpoint = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        checkpoint.stage = Stage::Reconcile;
        checkpoint.next = NextAction {
            reason: "resume_started".into(),
            action: "reconcile_main".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &checkpoint, 1).unwrap();
        assert_eq!(
            store
                .code_repair_round(&lease, &binding, "hosted_ci_policy_stale", true, 2)
                .unwrap(),
            Ladder::Repair(1)
        );
        let wait = store
            .code_repair_round(&lease, &binding, "hosted_ci_policy_stale", true, 3)
            .unwrap();
        // #2430: the backoff runs from the admitted round (t=2), not the check.
        assert_eq!(wait, Ladder::Wait(2 + 30 * MIN));
        let after = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        assert_eq!(
            (
                after.stage,
                after.next.reason.as_str(),
                after.next.action.as_str(),
                after.next.eligible_at_ms
            ),
            (
                Stage::Reconcile,
                "review_main_changed",
                "reconcile_main",
                2 + 30 * MIN
            )
        );
        // Replaying before the wait elapses keeps the same schedule.
        assert_eq!(
            store
                .code_repair_round(&lease, &binding, "hosted_ci_policy_stale", true, 4)
                .unwrap(),
            wait
        );
        assert_eq!(
            store
                .code_repair_round(
                    &lease,
                    &binding,
                    "hosted_ci_policy_stale",
                    true,
                    2 + 30 * MIN
                )
                .unwrap(),
            Ladder::Repair(2)
        );
        // A new head (a pushed repair) starts its own ladder.
        let moved = Binding {
            head: "d".repeat(40),
            ..binding
        };
        let mut checkpoint = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        checkpoint.binding = moved.clone();
        store.checkpoint(&lease, &checkpoint, 5).unwrap();
        assert_eq!(
            store
                .code_repair_round(&lease, &moved, "ci_failure_not_reproduced", true, 6)
                .unwrap(),
            Ladder::Repair(1)
        );
    }
}
