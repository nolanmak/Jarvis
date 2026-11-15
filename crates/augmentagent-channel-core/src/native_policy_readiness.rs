//! Shared, read-only declaration checks for readiness and native launch.
//! These checks grant no resources, refresh credentials or verify live auth.
use super::*;

pub(super) fn limits(policy: &Policy) -> Result<()> {
    ensure!(
        policy.version == 1 && (512..=4096).contains(&policy.memory_mb),
        "invalid native VM policy"
    );
    ensure!(
        policy.max_state_bytes >= SESSION_BYTES && policy.max_sessions > 0,
        "native state budget is not declared"
    );
    Ok(())
}

pub(super) fn refresh_grant(policy: &Policy, provider: &str, profile: &Provider) -> Result<()> {
    if profile.refresh_auth {
        let expected = policy
            .state_root
            .canonicalize()?
            .join("credentials")
            .join(format!("{provider}.json"));
        ensure!(
            profile.auth_file.as_ref() == Some(&expected),
            "OAuth refresh requires an explicitly imported controller credential"
        );
    }
    Ok(())
}
