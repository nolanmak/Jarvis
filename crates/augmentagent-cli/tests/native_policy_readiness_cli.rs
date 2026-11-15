//! Keep the packaged production-entry-point regression in the normal CLI CI suite.
use std::process::Command;

#[test]
fn readiness_rejects_unlaunchable_native_declarations_without_side_effects() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/tests/native_policy_readiness_cli.py"
        ))
        .env(
            "AUGMENTAGENT_TEST_BINARY",
            env!("CARGO_BIN_EXE_augmentagent"),
        )
        .output()
        .expect("run isolated packaged readiness fixtures");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
