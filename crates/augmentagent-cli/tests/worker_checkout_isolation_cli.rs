//! Run updater source-isolation fixtures through normal CLI CI discovery.
use std::process::Command;

#[test]
fn updater_refuses_dirty_owner_checkout_before_helpers() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/tests/updater_dirty_checkout_test.py"
        ))
        .output()
        .expect("run real updater in disposable repositories");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
