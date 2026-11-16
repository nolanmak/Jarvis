//! Hosted check handoff for a freshly integrated, reviewed candidate.
use super::*;

/// C owns evidence validation and failure classification. A passing observation
/// only discharges this wait for the reviewed head; M still owns final mutation.
/// Keep CI waits outside integration_* so pending checks do not schedule another
/// integration job or invalidate the independent review of the same tree.
pub(super) async fn wait_reason(root: &Path, pr: u64, head: &str) -> Option<&'static str> {
    let observation = match ci_repair::observe(root, pr).await {
        Ok(observation) => observation,
        Err(_) => return Some("hosted_ci_transport_unknown"),
    };
    if observation.head != head {
        return Some("ci_integrated_head_changed");
    }
    if observation.state == "passed" {
        return None;
    }
    Some(
        observation
            .dependency
            .unwrap_or(if observation.state == "failed" {
                "ci_integrated_failed"
            } else {
                "hosted_ci_incomplete"
            }),
    )
}

#[cfg(test)]
#[path = "self_improve_integration_checks_tests.rs"]
mod tests;
