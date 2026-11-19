use super::*;
#[test]
fn deployment_refuses_unproven_or_failed_publish_smoke() {
    for status in [
        None,
        Some("alias_mismatch"),
        Some("push_refused"),
        Some("inspection_timeout"),
    ] {
        let services = match status {
            None => serde_json::json!({}),
            Some(status) => {
                serde_json::json!({"publish":{"status":status,"permits_attempt":false}})
            }
        };
        let report:Report=serde_json::from_value(serde_json::json!({"status":"configuration-matched","matches":true,"live_verified":false,"worker_running":true,"expected_active":true,"services":services})).unwrap();
        assert!(
            report.preflight().is_err(),
            "deployment authorized an unproven publish path: {status:?}"
        );
    }
}

#[test]
fn deployment_accepts_matching_configuration_and_observed_push_smoke_only() {
    let report:Report=serde_json::from_value(serde_json::json!({"status":"configuration-matched","matches":true,"live_verified":false,"worker_running":true,"expected_active":true,
        "services":{"publish":{"status":"publish-ready","permits_attempt":true,"live_verified":false}}})).unwrap();
    assert!(report.preflight().is_ok());
}
