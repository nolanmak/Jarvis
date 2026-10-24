//! Records the Git commit so #1551 latency rows can be tied to a deployed build.
//! Best effort: without Git or a checkout the variable is left empty.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-env-changed=AUGMENTAGENT_PHONE_BUILD_COMMIT");
    if let Some(head) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={head}/HEAD");
    }
    let commit = std::env::var("AUGMENTAGENT_PHONE_BUILD_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_default();
    println!("cargo:rustc-env=AUGMENTAGENT_PHONE_BUILD_COMMIT={commit}");
}
