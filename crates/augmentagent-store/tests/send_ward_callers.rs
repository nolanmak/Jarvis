//! #1464 — the three send-ward CAS helpers gained a new `Err` variant
//! (`UnverifiedPlaceholder`), so EVERY production caller has to handle it
//! rather than fold it into a generic `{context}: {e}`. The caller set is
//! enumerated from the workspace sources here instead of from a prose list,
//! so a caller that pre-dates the guard — or one added later — fails this
//! test unless it routes the refusal through an explicit arm (#1464 review).

use std::collections::BTreeSet;
use std::path::Path;

/// `schedule_action(` also matches `reschedule_action(`/`unschedule_action(`,
/// which are not send-ward. Over-matching is the safe direction: it can only
/// pull more files into the set that must show an explicit arm.
const GUARDED: [&str; 3] =
    ["claim_action_for_send(", "schedule_action(", "claim_due_action_for_send("];

/// Every production caller, and the symbol its refusal arm routes through.
const EXPECTED: [(&str, &str); 4] = [
    ("augmentagent-channel-email/src/scheduled.rs", "UnverifiedPlaceholder"),
    ("augmentagent-channel-slack/src/contact/mod.rs", "UnverifiedPlaceholder"),
    ("augmentagent-cli/src/imessage_send.rs", "claim_failure_message"),
    ("augmentagent-cli/src/main.rs", "claim_failure_message"),
];

/// Drop every inline `#[cfg(test)] mod … { … }` body; a `mod …;` declaration
/// is left alone, since the file it names is scanned on its own. Calls from a
/// test are fine as `unwrap()` — an unexpected refusal must be loud there.
/// These files compile, so brace counting suffices to find each body's end.
fn without_test_modules(src: &str) -> String {
    let (mut out, mut rest) = (String::with_capacity(src.len()), src);
    while let Some(at) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(open) = rest.find('{') else { break };
        if rest[..open].contains(';') {
            out.push_str(&rest[..open + 1]);
            rest = &rest[open + 1..];
            continue;
        }
        let mut depth = 0usize;
        rest = &rest[rest[open..]
            .char_indices()
            .find_map(|(i, c)| {
                depth = depth.wrapping_add_signed(match c { '{' => 1, '}' => -1, _ => return None });
                (depth == 0).then_some(open + i + 1)
            })
            .unwrap_or(rest.len())..];
    }
    out.push_str(rest);
    out
}

#[test]
fn every_production_caller_of_a_send_ward_cas_handles_the_refusal() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").canonicalize().unwrap();
    let (mut found, mut queue) = (BTreeSet::new(), vec![crates.clone()]);
    while let Some(path) = queue.pop() {
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests" || n == "target") { continue }
            queue.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        let rel = path.strip_prefix(&crates).unwrap().to_string_lossy().to_string();
        // The defining crate is where the guard lives, not a caller of it.
        if path.extension().is_none_or(|e| e != "rs") || rel.starts_with("augmentagent-store/") {
            continue;
        }
        let src = without_test_modules(&std::fs::read_to_string(&path).unwrap());
        if !GUARDED.iter().any(|needle| src.contains(needle)) { continue }
        let handler = EXPECTED.iter().find(|(f, _)| *f == rel).map(|(_, h)| *h);
        let handler = handler.unwrap_or_else(|| panic!(
            "{rel} calls a send-ward CAS helper but is not in EXPECTED: decide how it \
             must report an UnverifiedPlaceholder refusal"));
        assert!(src.contains(handler),
            "{rel} must route an UnverifiedPlaceholder refusal through {handler}");
        found.insert(rel);
    }
    let expected: BTreeSet<String> = EXPECTED.iter().map(|(f, _)| f.to_string()).collect();
    assert_eq!(found, expected, "send-ward CAS caller set changed");
}
