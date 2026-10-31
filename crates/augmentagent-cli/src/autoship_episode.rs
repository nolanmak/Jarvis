//! Episode progress is verified findings closed and completed outcomes, not churn.
use crate::autoship_workflow::Binding;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Candidate {
    pub binding: Binding,
    pub tests_passed: bool,
    pub guards_passed: bool,
    pub verified_criteria: u32,
    pub open_findings: BTreeSet<String>,
    pub lines_changed: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Observation {
    /// Controller-issued receipt, persisted before starting another round.
    pub receipt: String,
    /// Contract + baseline + environment. A changed candidate head is not a new budget.
    pub epoch: String,
    pub candidate: Candidate,
    pub review_complete: bool,
    pub contradictory_requirements: bool,
    pub infrastructure_failed: bool,
    /// Set only from this workflow's confirmed remote merge receipt.
    pub autonomous_merge_observed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Decision {
    Complete,
    CompleteEvidence,
    RepairInfrastructure,
    Adjudicate,
    Repair,
    ReadyToMerge,
    FreshSession { best_head: String },
    Rescope { best_head: String },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Episode {
    pub epoch: String,
    pub best: Option<Candidate>,
    pub previous_findings: Option<BTreeSet<String>>,
    pub rounds: u32,
    pub findings_closed: u32,
    pub zero_progress_repairs: u32,
    pub fresh_sessions: u32,
    pub autonomous_merges: u32,
}
impl Candidate {
    fn score(&self) -> (bool, bool, u32, std::cmp::Reverse<usize>) {
        (
            self.guards_passed,
            self.tests_passed,
            self.verified_criteria,
            std::cmp::Reverse(self.open_findings.len()),
        )
    }
}
impl Episode {
    pub fn observe(&mut self, observation: &Observation) -> Result<Decision> {
        ensure!(
            !observation.receipt.is_empty() && !observation.epoch.is_empty(),
            "episode receipt and epoch required"
        );
        ensure!(
            observation.candidate.open_findings.len() <= 128,
            "episode findings must stay bounded"
        );
        ensure!(
            !observation.candidate.binding.head.is_empty(),
            "candidate commit required"
        );
        if self.epoch != observation.epoch {
            *self = Self {
                epoch: observation.epoch.clone(),
                ..Default::default()
            };
        }
        self.rounds = self.rounds.saturating_add(1);
        if observation.autonomous_merge_observed {
            ensure!(
                observation.candidate.tests_passed
                    && observation.candidate.guards_passed
                    && observation.review_complete
                    && observation.candidate.open_findings.is_empty(),
                "merge outcome lacks completion evidence"
            );
            self.autonomous_merges = self.autonomous_merges.saturating_add(1);
            return Ok(Decision::Complete);
        }
        if observation.infrastructure_failed {
            return Ok(Decision::RepairInfrastructure);
        }
        if !observation.review_complete {
            return Ok(Decision::CompleteEvidence);
        }
        let candidate = &observation.candidate;
        if self
            .best
            .as_ref()
            .is_none_or(|best| candidate.score() > best.score())
        {
            self.best = Some(candidate.clone());
        }
        if observation.contradictory_requirements {
            return Ok(Decision::Adjudicate);
        }
        if !candidate.tests_passed || !candidate.guards_passed {
            return Ok(Decision::Repair);
        }
        let closed = self
            .previous_findings
            .as_ref()
            .map(|prior| prior.difference(&candidate.open_findings).count() as u32);
        self.previous_findings = Some(candidate.open_findings.clone());
        if candidate.open_findings.is_empty() {
            self.zero_progress_repairs = 0;
            return Ok(Decision::ReadyToMerge);
        }
        if let Some(closed) = closed {
            self.findings_closed = self.findings_closed.saturating_add(closed);
            self.zero_progress_repairs = if closed == 0 {
                self.zero_progress_repairs.saturating_add(1)
            } else {
                0
            };
        }
        if self.zero_progress_repairs < 2 {
            return Ok(Decision::Repair);
        }
        let best_head = self
            .best
            .as_ref()
            .expect("candidate observed")
            .binding
            .head
            .clone();
        if self.fresh_sessions == 0 {
            self.fresh_sessions = 1;
            self.zero_progress_repairs = 0;
            Ok(Decision::FreshSession { best_head })
        } else {
            Ok(Decision::Rescope { best_head })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation(n: u32, findings: &[&str]) -> Observation {
        Observation {
            receipt: format!("receipt-{n}"),
            epoch: "contract/base/environment".into(),
            candidate: Candidate {
                binding: Binding {
                    head: format!("commit-{n}"),
                    contract: "contract".into(),
                    evidence_hash: format!("evidence-{n}"),
                },
                tests_passed: true,
                guards_passed: true,
                verified_criteria: 2,
                open_findings: findings.iter().map(|s| s.to_string()).collect(),
                lines_changed: n * 100,
            },
            review_complete: true,
            contradictory_requirements: false,
            infrastructure_failed: false,
            autonomous_merge_observed: false,
        }
    }
    #[test]
    fn churn_does_not_hide_stagnation_and_fresh_session_keeps_best_verified_candidate() {
        let mut state = Episode::default();
        assert_eq!(
            state.observe(&observation(1, &["F1"])).unwrap(),
            Decision::Repair
        );
        assert_eq!(
            state.observe(&observation(2, &["F1"])).unwrap(),
            Decision::Repair
        );
        assert_eq!(
            state.observe(&observation(3, &["F1"])).unwrap(),
            Decision::FreshSession {
                best_head: "commit-1".into()
            }
        );
        assert_eq!(
            state.observe(&observation(4, &["F1"])).unwrap(),
            Decision::Repair
        );
        assert_eq!(
            state.observe(&observation(5, &["F1"])).unwrap(),
            Decision::Rescope {
                best_head: "commit-1".into()
            }
        );
        assert_eq!(state.fresh_sessions, 1);
    }
    #[test]
    fn contradictions_and_incomplete_evidence_never_consume_fresh_builder_budget() {
        let mut state = Episode::default();
        let mut obs = observation(1, &["F1"]);
        obs.contradictory_requirements = true;
        assert_eq!(state.observe(&obs).unwrap(), Decision::Adjudicate);
        obs.review_complete = false;
        assert_eq!(state.observe(&obs).unwrap(), Decision::CompleteEvidence);
        obs.infrastructure_failed = true;
        assert_eq!(state.observe(&obs).unwrap(), Decision::RepairInfrastructure);
        assert_eq!(state.fresh_sessions, 0);
    }
    #[test]
    fn lgtm_and_small_diffs_are_not_shipping_outcomes() {
        let mut state = Episode::default();
        let mut obs = observation(1, &[]);
        assert_eq!(state.observe(&obs).unwrap(), Decision::ReadyToMerge);
        assert_eq!(state.autonomous_merges, 0);
        obs.autonomous_merge_observed = true;
        assert_eq!(state.observe(&obs).unwrap(), Decision::Complete);
        assert_eq!(state.autonomous_merges, 1);
    }
    #[test]
    fn actual_closed_findings_reset_stagnation_and_compact_state_survives_many_rounds() {
        let mut state = Episode::default();
        state.observe(&observation(1, &["F1", "F2"])).unwrap();
        state.observe(&observation(2, &["F2"])).unwrap();
        assert_eq!(state.findings_closed, 1);
        assert_eq!(state.zero_progress_repairs, 0);
        for n in 3..103 {
            state.observe(&observation(n, &["F2"])).unwrap();
        }
        assert!(serde_json::to_vec(&state).unwrap().len() < 4096);
        assert_eq!(state.best.unwrap().binding.head, "commit-2");
    }
    #[test]
    fn restart_replays_do_not_spend_another_fresh_session_and_old_leases_cannot_advance() {
        use crate::autoship_workflow::{Checkpoint, NextAction, Stage, WorkflowStore};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.sqlite");
        let first = observation(1, &["F1"]);
        WorkflowStore::open(&path)
            .unwrap()
            .register("repo#1", "fix/one", first.candidate.binding.clone(), 0)
            .unwrap();
        let mut old_lease = None;
        for n in 1..=5 {
            let mut store = WorkflowStore::open(&path).unwrap();
            let now = i64::from(n) * 10_000;
            let lease = store
                .claim("repo#1", &format!("worker-{n}"), now, 1000)
                .unwrap()
                .unwrap();
            let obs = observation(n, &["F1"]);
            store
                .checkpoint(
                    &lease,
                    &Checkpoint {
                        stage: Stage::Review,
                        binding: obs.candidate.binding.clone(),
                        evidence: vec![],
                        next: NextAction {
                            reason: "reviewed".into(),
                            action: "decide".into(),
                            eligible_at_ms: now,
                        },
                    },
                    now,
                )
                .unwrap();
            let decision = store.observe_episode(&lease, &obs, now).unwrap();
            assert_eq!(store.observe_episode(&lease, &obs, now).unwrap(), decision);
            if n == 3 {
                assert_eq!(
                    decision,
                    Decision::FreshSession {
                        best_head: "commit-1".into()
                    }
                );
            }
            if n == 5 {
                assert_eq!(
                    decision,
                    Decision::Rescope {
                        best_head: "commit-1".into()
                    }
                );
            }
            let mut rewritten = obs.clone();
            rewritten.candidate.lines_changed += 1;
            assert!(store.observe_episode(&lease, &rewritten, now).is_err());
            if let Some(old) = old_lease {
                assert!(store.observe_episode(&old, &obs, now).is_err());
            }
            old_lease = Some(lease);
        }
    }
}
