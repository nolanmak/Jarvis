//! Feed the binding final ruling into the actual resume and sweep predicates.
use super::*;
pub(crate) fn binding_ruling_finishes_without_unanimity(
    evidence: crate::autoship_review::ReviewEvidence,
) {
    let head = evidence.binding.head.clone();
    let review = IndependentReview {
        provider: Some(augmentagent_channel_core::ProviderKind::Codex),
        model: Some("fixture".into()),
        available: true,
        diff_ok: false,
        system_ok: false,
        notes: "Both original reviewers retain contradictory findings".into(),
        why_unavailable: None,
        structured: Some(evidence),
    };
    assert!(
        review.approved(),
        "resume demanded unanimous reviewer LGTMs despite binding adjudication"
    );
    assert!(review.status().contains("approved by final adjudication"));
    let record = OpenedPr {
        pr: 1604,
        head: head.clone(),
        verdict: review.recorded_verdict(),
    };
    let approval = recorded_approval(Some(&record), 1604, Some(&head));
    assert!(approval.reviewed);
    let mut candidate = SweepCandidate {
        pr: 1604,
        issue: 1709,
        ours: Some(true),
        loop_authored: true,
        mergeable: Some(true),
        checks_green: Some(false),
        review_approved: approval.reviewed,
        policy: MergePolicy {
            automerge_enabled: true,
            complexity: Complexity::Medium,
            codex_approved: approval.codex,
            reviews_approved: approval.reviewed,
            research_filed: false,
            receipt_gated_file: None,
            lgtm_overrides_receipt: None,
            issue_author: "owner".into(),
            repo_owner: Some("owner".into()),
            automerge_authors: None,
        },
    };
    for checks in [Some(false), None] {
        candidate.checks_green = checks;
        assert!(
            matches!(sweep_verdict(&candidate), SweepVerdict::Skip(_)),
            "adjudication waived required CI"
        );
    }
    candidate.checks_green = Some(true);
    assert_eq!(sweep_verdict(&candidate), SweepVerdict::Merge);
    assert!(
        !recorded_approval(Some(&record), 1604, Some("new-head")).reviewed,
        "old final ruling approved a new head"
    );
}

#[test]
fn isolated_claude_capacity_survives_both_writer_brands() {
    use augmentagent_channel_core::ProviderKind::{Claude, Codex};
    let selected = select_session_isolated_reviewer(
        &[Codex, Claude],
        Ok(Some(vec![Codex, Claude])),
        |provider| {
            if provider == Claude {
                ReviewerStatus::Ready
            } else {
                ReviewerStatus::NotConfigured("scripted Codex outage".into())
            }
        },
    );
    assert_eq!(
        selected,
        Ok(Claude),
        "isolated native capacity was permanently exhausted by two writer brand names"
    );
}
#[test]
fn isolated_review_reports_recoverable_capacity_after_both_brands_write() {
    use augmentagent_channel_core::ProviderKind::{Claude, Codex};
    let missing =
        select_session_isolated_reviewer(&[Codex, Claude], Ok(Some(vec![Codex, Claude])), |_| {
            ReviewerStatus::NotConfigured("scripted outage".into())
        });
    assert!(
        matches!(missing, Err(ReviewUnavailable::NoCapacity { .. })),
        "provider recovery was mislabeled permanent writer exhaustion: {missing:?}"
    );
    let reset = chrono::DateTime::from_timestamp(1_900_000_000, 0).unwrap();
    let latched =
        select_session_isolated_reviewer(&[Codex, Claude], Ok(Some(vec![Codex, Claude])), |_| {
            ReviewerStatus::Latched(Some(reset))
        });
    assert!(
        matches!(latched, Err(ReviewUnavailable::Latched { .. })),
        "cooldown cannot wake a permanent writer-exhaustion verdict: {latched:?}"
    );
}
