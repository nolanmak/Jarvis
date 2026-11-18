//! Actual path consumers, isolated from the host's deployment environment.
use super::*;

#[test]
fn volume_routes_gate_verifier_and_scratch_with_specific_override_precedence() {
    const TEST: &str = "self_improve::acceptance::build_volume_resolution_tests::volume_routes_gate_verifier_and_scratch_with_specific_override_precedence";
    if let Some(root) = std::env::var_os("BUILD_VOLUME_RESOLUTION_CHILD") {
        let root = PathBuf::from(root);
        let gate = crate::build_storage::effective_target(&root.join("repo"));
        std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
        let verifier = verifier_cache_sibling(&root.join("repo"), &gate, &"a".repeat(64)).unwrap();
        std::fs::write(root.join("resolved.json"),serde_json::to_vec(&serde_json::json!({
            "gate":gate,"sessions":augmentagent_channel_core::build_volume::paths().sessions,"verifier":verifier,"scratch":augmentagent_channel_core::build_scratch::scratch_dir()})).unwrap()).unwrap();
        return;
    }
    let mut failures = Vec::new();
    for mode in [0, 1, 2] {
        let specific = mode == 1;
        let root = tempfile::tempdir().unwrap();
        let volume = root.path().join("volume");
        std::fs::create_dir(&volume).unwrap();
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", TEST])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path().join("home"))
            .env("AUGMENTAGENT_BUILD_VOLUME", &volume)
            .env("BUILD_VOLUME_RESOLUTION_CHILD", root.path());
        let expected_gate = if specific {
            let gate = root.path().join("override/gate");
            cmd.env("AUGMENTAGENT_GATE_TARGET_DIR", &gate);
            gate
        } else {
            volume.join("augmentagent-gate-target")
        };
        if mode == 2 {
            cmd.env(
                "AUGMENTAGENT_BUILD_PARENT",
                root.path().join("session-override"),
            )
            .env(
                "AUGMENTAGENT_BUILD_SCRATCH_DIR",
                root.path().join("scratch-override"),
            );
        }
        let expected_sessions = if mode == 2 {
            root.path().join("session-override")
        } else {
            volume.join("augmentagent-session-targets")
        };
        let expected_scratch = if mode == 2 {
            root.path().join("scratch-override")
        } else {
            volume.join("codex-vm")
        };
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("resolved.json")).unwrap())
                .unwrap();
        for (key, expected) in [
            ("gate", expected_gate),
            ("sessions", expected_sessions),
            (
                "verifier",
                volume
                    .join("augmentagent-verifier-targets")
                    .join(format!("augmentagent-verifier-{}", "a".repeat(64))),
            ),
            ("scratch", expected_scratch),
        ] {
            if actual[key] != serde_json::to_value(&expected).unwrap() {
                failures.push(format!(
                    "specific={specific} {key}: actual={} expected={}",
                    actual[key],
                    expected.display()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
