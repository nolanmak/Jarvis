use std::process::Command;

fn main() {
    let output = Command::new("cargo")
        .args(["test", "-p", "augmentagent-channel-linkedin", "register_warning", "--", "--nocapture"])
        .output()
        .expect("run LinkedIn register-card observations");
    let text = String::from_utf8_lossy(&output.stdout);
    let result = [
        text.contains("linkedin_code_register_mismatch_warning_visible"),
        text.contains("linkedin_classic_register_mismatch_warning_visible"),
        text.contains("linkedin_register_mismatch_card_pending"),
        text.contains("linkedin_register_mismatch_approval_eligible"),
        text.contains("linkedin_register_owner_override_suppressed"),
        text.contains("linkedin_register_matching_or_insufficient_suppressed"),
    ];
    println!("{}", serde_json::to_string(&result).unwrap());
}
