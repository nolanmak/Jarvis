//! #1464 — the send-ward CAS helpers now refuse a draft still carrying an
//! `[[UNVERIFIED: …]]` marker. A caller that treats their `Err` as a plain
//! internal failure hands the owner an opaque message, and one added later
//! could route a flagged draft somewhere unguarded. A hand-written list of
//! callers proves neither, so this generates the inventory from `crates/*/src`
//! and pins it: every production caller is enumerated, and each must handle
//! the typed refusal by name.
use std::path::{Path, PathBuf};

/// The guarded helpers. The leading dot keeps `reschedule_action` /
/// `unschedule_action` — which move a row AWAY from a transport and are
/// deliberately unguarded — out.
const GUARDED: [&str; 3] =
    [".claim_action_for_send(", ".claim_due_action_for_send(", ".schedule_action("];

/// Every production file allowed to call one. A new entry means a new path
/// from a persisted draft to a real transport: it owes the refusal handling
/// asserted below and a rail test in its own crate.
const CALLERS: [&str; 4] = [
    "crates/augmentagent-channel-email/src/scheduled.rs",
    "crates/augmentagent-channel-slack/src/contact/mod.rs",
    "crates/augmentagent-cli/src/imessage_send.rs",
    "crates/augmentagent-cli/src/main.rs",
];

/// The source outside every `#[cfg(test)] mod … {}` block: test callers
/// `unwrap()`, and panicking on an unexpected refusal is what we want from
/// them. `store.rs` calls the helpers only from its own tests, so its absence
/// from the inventory is this stripper's canary.
fn production(text: &str) -> String {
    let (mut out, mut depth, mut cfg_test) = (String::new(), 0i32, false);
    for line in text.lines() {
        let delta = line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if depth > 0 {
            depth += delta;
        } else if cfg_test && line.contains("mod ") && line.contains('{') {
            depth = delta;
        } else {
            out.push_str(line);
            out.push('\n');
        }
        cfg_test = line.trim() == "#[cfg(test)]";
    }
    out
}

fn sources(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

#[test]
fn every_production_caller_of_a_send_ward_cas_handles_the_refusal() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }

    let mut found = Vec::new();
    for path in files {
        let text = production(&std::fs::read_to_string(&path).unwrap());
        if !GUARDED.iter().any(|helper| text.contains(helper)) {
            continue;
        }
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
        // Either it matches the variant, or it funnels the error into the CLI
        // helper that ships the store's own owner-facing wording.
        assert!(
            text.contains("StoreError::UnverifiedPlaceholder")
                || text.contains("claim_failure_message"),
            "{rel} claims a draft for send but never mentions \
             StoreError::UnverifiedPlaceholder: a flagged draft would be \
             refused with an opaque message, or worse, retried"
        );
        found.push(rel);
    }
    found.sort();
    assert_eq!(found, CALLERS, "the production callers of {GUARDED:?} changed (#1464)");
}
