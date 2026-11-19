//! Give the finite owner recovery backlog deterministic turns ahead of ordinary
//! work and free original-evidence probes. This grants no execution authority.
use super::*;

pub(super) fn prioritize(
    mut candidates: Vec<RestIssue>,
    pending: &std::collections::BTreeSet<u64>,
) -> Vec<RestIssue> {
    // Stable partition preserves the existing due/lease/continuity checks and
    // durable fairness order within both groups. A current-policy claim removes
    // its workflow from this queue rather than repeatedly jumping ahead.
    candidates.sort_by_key(|issue| !pending.contains(&issue.number));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_recovery_precedes_other_recovery_and_preserves_peer_fairness() {
        let rows = [9, 7, 8, 6].map(|number| RestIssue {
            number,
            title: "Synthetic widget".into(),
            body: "Synthetic request".into(),
            author: "fixture-owner".into(),
            association: "OWNER".into(),
            research_filed: false,
        });
        let pending = [7, 6].into_iter().collect();
        assert_eq!(
            prioritize(rows.into(), &pending)
                .into_iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![7, 6, 9, 8]
        );
    }
}
