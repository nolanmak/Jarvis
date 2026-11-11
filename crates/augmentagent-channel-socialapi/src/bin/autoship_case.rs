use std::process::Command;

fn main() {
    let output = Command::new("cargo")
        .args(["test", "-p", "augmentagent-channel-socialapi", "register_warning", "--", "--nocapture"])
        .output()
        .expect("run SocialAPI register-card observations");
    let text = String::from_utf8_lossy(&output.stdout);
    let result = [
        text.contains("socialapi_register_mismatch_warning_visible"),
        text.contains("socialapi_register_mismatch_card_pending"),
        text.contains("socialapi_register_mismatch_approval_eligible"),
        text.contains("socialapi_register_owner_override_suppressed"),
        text.contains("socialapi_register_matching_or_insufficient_suppressed"),
    ];
    println!("{}", serde_json::to_string(&result).unwrap());
}
