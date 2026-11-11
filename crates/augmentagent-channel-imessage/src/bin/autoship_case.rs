use std::process::Command;

fn main() {
    let output = Command::new("cargo")
        .args(["test", "-p", "augmentagent-channel-imessage", "register_warning", "--", "--nocapture"])
        .output()
        .expect("run iMessage register-card observations");
    let text = String::from_utf8_lossy(&output.stdout);
    let result = [
        text.contains("imessage_register_mismatch_warning_visible"),
        text.contains("imessage_register_mismatch_card_pending"),
        text.contains("imessage_register_mismatch_approval_eligible"),
        text.contains("imessage_register_owner_override_suppressed"),
        text.contains("imessage_register_matching_or_insufficient_suppressed"),
    ];
    println!("{}", serde_json::to_string(&result).unwrap());
}
