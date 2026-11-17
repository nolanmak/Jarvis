use super::*;
#[test]
fn relocate_cli_has_supported_dry_run_entry() {
    use clap::Parser;
    let parsed = Cli::try_parse_from([
        "augmentagent",
        "disk",
        "relocate",
        "--to",
        "/tmp/owned-build-volume-fixture",
        "--dry-run",
    ]);
    assert!(
        parsed.is_ok(),
        "supported relocation command missing: {}",
        parsed.err().unwrap()
    );
}
#[test]
fn managed_wrapper_uses_volume_and_preserves_specific_parent_override() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/tests/build-volume-wrapper.test.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn build_volume_production_verification_requires_all_roles_and_old_space_rise() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/tests/build-volume-verification.test.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
