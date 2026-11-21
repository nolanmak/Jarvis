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
            // #2394: retried past the publication stall window without progress.
            heartbeat_at_ms: 30 * 60_000 + 700,
            progress_at_ms: 100,
            lease_until_ms: 0,
            generation: 3,
        };
        assert_eq!(
            retry_age(Some(&snapshot)),
            30 * 60_000 + 700,
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

/// #2394 production shape (#2329 / PR #2378): a build/review/fix cycle spent
/// six claims, then the publish turn was killed by a worker restart right after
/// checkpointing `publishing_revision / push`. Claims spent before publication
/// are not a stalled publication: the due push goes ahead of fresh work. A
/// publication that keeps retrying without progress still rotates (#2280).
#[test]
fn due_publish_retry_outranks_fresh_work_despite_earlier_claims() {
    let t0 = 1_795_230_305_000;
    let mut publish = Snapshot {
        workflow: "owner/repo#2329".into(),
        branch: "agent-fix/issue-2329".into(),
        pr: Some(2378),
        checkpoint: Checkpoint {
            stage: Stage::Publish,
            binding: Binding { head: "57ee".repeat(10), contract: "contract".into(), evidence_hash: "evidence".into() },
            evidence: vec![],
            next: NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: t0 + 300_000 },
        },
        attempts: 6,
        wall_ms: 0,
        model_calls: 40,
        tokens: 0,
        heartbeat_at_ms: t0,
        progress_at_ms: t0 + 15_000,
        lease_until_ms: t0 + 600_000,
        generation: 6,
    };
    let now = t0 + 32 * 60_000;
    assert!(rank(Some(&publish), false, true, now) < rank(None, false, false, now),
        "a due publication retry must not wait behind fresh issues for earlier build claims");
    assert!(rank(Some(&publish), false, true, now) < rank(None, true, false, now));
    // The same row after its push kept failing for 40 minutes: it rotates.
    publish.heartbeat_at_ms = t0 + 40 * 60_000;
    let later = t0 + 45 * 60_000;
    assert!(rank(None, false, false, later) < rank(Some(&publish), false, true, later),
        "a stalled publication still yields to fresh peers (#2280)");
}
