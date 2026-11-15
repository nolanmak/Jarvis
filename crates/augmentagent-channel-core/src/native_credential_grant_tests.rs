//! Readiness must reject every refresh declaration the native launcher rejects.
use super::*;

#[test]
fn refresh_grants_are_validated_before_api_key_shortcuts() {
    for route in ["environment", "file", "subscription"] {
        for imported in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let credentials = root.path().join("credentials");
            std::fs::create_dir(&credentials).unwrap();
            let auth = if imported {
                credentials.join("codex.json")
            } else {
                root.path().join("owner-auth.json")
            };
            let value = if route == "file" {
                serde_json::json!({"OPENAI_API_KEY":"fixture-private-key"})
            } else {
                serde_json::json!({"tokens":{"access_token":"opaque","refresh_token":"fixture-refresh","id_token":"fixture-id"}})
            };
            write(&auth, value.to_string().as_bytes()).unwrap();
            let path = root.path().join("policy.json");
            let policy = serde_json::json!({"version":1,"runtime_config":"/runtime.json","guest_runtime":"/guest",
                "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
                "providers":{"codex":{"gateway":{},"auth_file":auth,"auth_relative":".codex/auth.json",
                    "refresh_auth":true,"environment":["CODEX_API_KEY"]}}});
            write(&path, policy.to_string().as_bytes()).unwrap();
            let before = std::fs::read(&auth).unwrap();
            let report = credentials_at(Some(&path), "codex", 1000, |_| {
                (route == "environment").then(|| "fixture-env-key".into())
            });
            assert_eq!(report.permits_attempt(), imported,
                "refresh grant outside imported controller credentials must fail before provider dispatch: route={route}, status={:?}",report.status);
            assert_eq!(
                std::fs::read(&auth).unwrap(),
                before,
                "readiness must never refresh or import credentials"
            );
            assert!(!report.live_verified);
            assert!(!serde_json::to_string(&report).unwrap().contains("fixture-"));
        }
    }
}

#[test]
fn fileless_refresh_grant_cannot_pass_environment_authentication() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("policy.json");
    write(
        &path,
        serde_json::json!({"version":1,"runtime_config":"/runtime.json","guest_runtime":"/guest",
        "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
        "providers":{"codex":{"gateway":{},"auth_relative":".codex/auth.json","refresh_auth":true,
            "environment":["CODEX_API_KEY"]}}})
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let report = credentials_at(Some(&path), "codex", 1000, |_| {
        Some("fixture-env-key".into())
    });
    assert!(
        !report.permits_attempt(),
        "a refresh grant requires an explicitly imported controller credential"
    );
}

#[test]
fn invalid_launch_limits_cannot_pass_readiness_with_usable_credentials() {
    for (field, value) in [
        ("memory_mb", 511u64),
        ("memory_mb", 4097),
        ("max_sessions", 0),
        ("max_state_bytes", 1024),
    ] {
        let root = tempfile::tempdir().unwrap();
        let auth = root.path().join("auth.json");
        write(&auth, br#"{"OPENAI_API_KEY":"fixture-key"}"#).unwrap();
        let guest = root.path().join("guest");
        std::fs::create_dir(&guest).unwrap();
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700)).unwrap();
        let artifact = guest.join("artifact");
        write(&artifact, b"fixture").unwrap();
        use sha2::{Digest, Sha256};
        write(
            &guest.join("manifest.json"),
            serde_json::json!({"version":1,"files":[
            {"path":"artifact","sha256":format!("{:x}",Sha256::digest(b"fixture"))}]})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let path = root.path().join("policy.json");
        let mut policy = serde_json::json!({"version":1,"runtime_config":"/runtime.json","guest_runtime":guest,
            "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
            "providers":{"codex":{"gateway":{},"auth_file":auth,"auth_relative":".codex/auth.json"}}});
        policy[field] = value.into();
        write(&path, policy.to_string().as_bytes()).unwrap();
        let report = credentials_at(Some(&path), "codex", 1000, |_| None);
        assert_eq!(
            report.status,
            CredentialStatus::PolicyInvalid,
            "{field}={value} fails native launch"
        );
        let runtime = runtime::inspect_at(Some(&path));
        assert_eq!(
            runtime.status, "policy-invalid",
            "invalid launch declaration must precede runtime validation"
        );
        assert!(
            !root.path().join("state.lock").exists(),
            "readiness must not create admission state"
        );
    }
}

#[test]
fn routed_auth_cannot_bypass_native_launch_declarations() {
    const CHILD: &str = "NATIVE_ROUTED_GRANT_FIXTURE";
    const TEST: &str =
        "native_vm::readiness::grant_tests::routed_auth_cannot_bypass_native_launch_declarations";
    if std::env::var_os(CHILD).is_some() {
        let allowed = super::super::declared_router_auth("codex").unwrap_or(false);
        assert_eq!(allowed, std::env::var(CHILD).unwrap() == "allowed");
        return;
    }
    for scenario in ["read-only", "bad-grant", "imported-grant", "invalid-limits"] {
        let root = tempfile::tempdir().unwrap();
        let credentials = root.path().join("credentials");
        std::fs::create_dir(&credentials).unwrap();
        let auth = credentials.join("codex.json");
        write(&auth, br#"{"OPENAI_API_KEY":"fixture"}"#).unwrap();
        let mut policy = serde_json::json!({"version":1,"runtime_config":"/runtime.json","guest_runtime":"/guest",
            "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
            "providers":{"codex":{"gateway":{},"auth_relative":".codex/auth.json",
                "environment":["AUGMENTAGENT_ROUTER_API_KEY"]}}});
        match scenario {
            "bad-grant" => policy["providers"]["codex"]["refresh_auth"] = true.into(),
            "imported-grant" => {
                policy["providers"]["codex"]["refresh_auth"] = true.into();
                policy["providers"]["codex"]["auth_file"] = serde_json::to_value(auth).unwrap();
            }
            "invalid-limits" => policy["memory_mb"] = 511.into(),
            _ => (),
        }
        let path = root.path().join("policy.json");
        write(&path, policy.to_string().as_bytes()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path())
            .env("AUGMENTAGENT_NATIVE_VM_POLICY", &path)
            .env(
                CHILD,
                if matches!(scenario, "read-only" | "imported-grant") {
                    "allowed"
                } else {
                    "blocked"
                },
            )
            .status()
            .unwrap();
        assert!(status.success(), "{scenario}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!root.path().join("state.lock").exists());
    }
}

#[test]
fn runtime_distinguishes_missing_and_invalid_selected_policy() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("policy.json");
    assert_eq!(runtime::inspect_at(Some(&path)).status, "policy-missing");
    write(&path, b"invalid-json").unwrap();
    assert_eq!(runtime::inspect_at(Some(&path)).status, "policy-invalid");
    assert!(!root.path().join("state.lock").exists());
}
