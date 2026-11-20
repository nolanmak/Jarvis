use super::*;
use crate::autoship_workflow::{Binding, Checkpoint, NextAction};

#[test]
fn all_active_retry_stages_rotate_using_durable_activity() {
    for (stage, reason, action) in [
        (Stage::Scope, "scope_deferred", "retry_scope"),
        (Stage::Build, "resource_capacity", "await_capacity"),
        (
            Stage::Verify,
            "verification_infrastructure_retry",
            "retry_verification",
        ),
        (Stage::Publish, "publishing_revision", "push"),
        (
            Stage::Scope,
            "acceptance_registration_retry",
            "rescope_executable_acceptance",
        ),
    ] {
        let snapshot = Snapshot {
            workflow: "owner/repo#1".into(),
            branch: "candidate".into(),
            pr: None,
            checkpoint: Checkpoint {
                stage,
                binding: Binding {
                    head: "head".into(),
                    contract: "contract".into(),
                    evidence_hash: "evidence".into(),
                },
                evidence: vec!["preserved".into()],
                next: NextAction {
                    reason: reason.into(),
                    action: action.into(),
                    eligible_at_ms: 0,
                },
            },
            attempts: 3,
            wall_ms: 0,
            model_calls: 4,
            tokens: 0,
            heartbeat_at_ms: 700,
            progress_at_ms: 100,
            lease_until_ms: 0,
            generation: 3,
        };
        assert_eq!(
            retry_age(Some(&snapshot)),
            700,
            "{stage:?}: retry must yield to eligible peers"
        );
    }
}

/// #2373: twelve acceptance rows at `acceptance_strategy_reviewer_capacity`
/// (Scope, attempts >= 3) were due for hours yet never picked, because a
/// repeated retry always sorted behind any fresh peer and the picker takes the
/// first viable candidate. A due capacity-wait row must win within a bounded
/// number of ticks even while fresh peers keep arriving.
#[test]
fn due_capacity_wait_row_is_selected_within_bounded_ticks_against_busy_peers() {
    const TICK_MS: i64 = 30 * 60_000;
    const MAX_TICKS: i64 = 6;
    let t0 = 1_800_000_000_000;
    let waiting = Snapshot {
        workflow: "owner/repo#2348".into(),
        branch: "candidate".into(),
        pr: None,
        checkpoint: Checkpoint {
            stage: Stage::Scope,
            binding: Binding { head: "head".into(), contract: "contract".into(), evidence_hash: "evidence".into() },
            evidence: vec![],
            next: NextAction {
                reason: "acceptance_strategy_reviewer_capacity".into(),
                action: "escalate_acceptance".into(),
                eligible_at_ms: t0 + 300_000,
            },
        },
        attempts: 3,
        wall_ms: 0,
        model_calls: 0,
        tokens: 0,
        heartbeat_at_ms: t0,
        progress_at_ms: t0,
        lease_until_ms: 0,
        generation: 3,
    };
    // A busy box: every tick has a fresh, never-attempted human peer and a
    // fresh research peer. The waiting row is never re-selected by a peer.
    let mut picked_at = None;
    for tick in 1..=MAX_TICKS {
        let now = t0 + tick * TICK_MS;
        let mut ranked = vec![
            (rank(None, false, false, now), "fresh-human"),
            (rank(None, true, false, now), "fresh-research"),
            (rank(Some(&waiting), false, false, now), "capacity-wait"),
        ];
        ranked.sort_by_key(|(key, _)| *key);
        if ranked[0].1 == "capacity-wait" {
            picked_at = Some(tick);
            break;
        }
    }
    assert!(picked_at.is_some(), "a due capacity-wait row starved for {MAX_TICKS} ticks behind fresh peers");
    // Before the starvation bound, ordinary retries still yield to fresh work.
    let early = t0 + TICK_MS;
    assert!(rank(None, false, false, early) < rank(Some(&waiting), false, false, early));
}
