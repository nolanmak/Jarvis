//! #1300 — the executable Slack parity matrix (`docs/slack-parity-matrix.json`).
//!
//! The checked-in matrix must pass every check against this source tree and
//! the Slack capability tables. Each failure mode is then proven by breaking
//! a copy of the real matrix (or the capability lookup) on purpose. Nothing
//! here talks to Slack or runs a named test; the CI step runs those.

use std::path::{Path, PathBuf};

use augmentagent_channel_slack::parity::{
    cargo_commands, check, parse, render_text, report, Blocker, CheckContext, HostEvidence,
    ParityMatrix, RowStatus, TestRef, ViolationKind, EPIC_COVERAGE, MATRIX_PATH,
};
use augmentagent_channel_slack::surface::SupportStatus;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn matrix() -> ParityMatrix {
    let path = repo_root().join(MATRIX_PATH);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    parse(&text).expect("the checked-in matrix parses")
}

fn kinds(matrix: &ParityMatrix) -> Vec<(ViolationKind, String)> {
    let root = repo_root();
    check(matrix, &CheckContext::slack(&root))
        .into_iter()
        .map(|v| (v.kind, format!("{} {}", v.subject, v.detail)))
        .collect()
}

fn row_mut<'a>(
    matrix: &'a mut ParityMatrix,
    id: &str,
) -> &'a mut augmentagent_channel_slack::parity::ParityRow {
    matrix
        .rows
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("no row {id}"))
}

fn has(found: &[(ViolationKind, String)], kind: ViolationKind, subject: &str) -> bool {
    found.iter().any(|(k, s)| *k == kind && s.contains(subject))
}

#[test]
fn the_checked_in_matrix_passes_every_check() {
    let found = kinds(&matrix());
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn every_epic_row_has_exactly_one_matrix_row_with_its_owning_issues() {
    let m = matrix();
    assert_eq!(EPIC_COVERAGE.len(), 20);
    assert_eq!(m.rows.len(), EPIC_COVERAGE.len());
    for epic in EPIC_COVERAGE.iter() {
        let rows: Vec<_> = m
            .rows
            .iter()
            .filter(|row| row.capability == epic.capability)
            .collect();
        assert_eq!(rows.len(), 1, "{}", epic.capability);
        assert_eq!(rows[0].issues, epic.issues, "{}", epic.capability);
    }
}

// --- failure modes, each on a deliberately broken copy -------------------

#[test]
fn a_missing_epic_row_fails() {
    let mut m = matrix();
    m.rows.retain(|row| row.id != "notifications");
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::MissingEpicRow, "Digests, reminders"),
        "{found:#?}"
    );
}

#[test]
fn a_row_the_epic_does_not_have_or_with_other_issues_fails() {
    let mut m = matrix();
    row_mut(&mut m, "scheduling").issues = vec![1291, 9999];
    let mut extra = m.rows[0].clone();
    extra.id = "made-up".into();
    extra.capability = "Teleportation".into();
    m.rows.push(extra);
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::IssueMismatch, "scheduling"),
        "{found:#?}"
    );
    assert!(
        has(&found, ViolationKind::UnknownRow, "made-up"),
        "{found:#?}"
    );
}

#[test]
fn a_row_with_no_named_test_fails() {
    let mut m = matrix();
    row_mut(&mut m, "approvals").tests.clear();
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::NoNamedTest, "approvals"),
        "{found:#?}"
    );
}

#[test]
fn a_named_test_that_is_not_in_the_source_tree_fails() {
    let mut m = matrix();
    let row = row_mut(&mut m, "approvals");
    row.tests[0].name = "a_test_nobody_wrote".into();
    row.tests.push(TestRef {
        package: "augmentagent-channel-slack".into(),
        target: "test:no_such_test_binary".into(),
        name: "skip_discards_the_draft_and_the_card_says_so".into(),
    });
    row.tests.push(TestRef {
        package: "augmentagent-no-such-crate".into(),
        target: "lib".into(),
        name: "anything".into(),
    });
    let found = kinds(&m);
    let missing: Vec<_> = found
        .iter()
        .filter(|(k, _)| *k == ViolationKind::TestNotFound)
        .collect();
    assert_eq!(missing.len(), 3, "{found:#?}");
    assert!(has(
        &found,
        ViolationKind::TestNotFound,
        "a_test_nobody_wrote"
    ));
    assert!(has(
        &found,
        ViolationKind::TestNotFound,
        "no_such_test_binary"
    ));
    assert!(has(
        &found,
        ViolationKind::TestNotFound,
        "augmentagent-no-such-crate"
    ));
}

#[test]
fn a_helper_function_or_an_ignored_test_is_not_a_passing_test() {
    let mut m = matrix();
    let row = row_mut(&mut m, "transport");
    // A real `#[ignore]` live probe: it never runs in CI.
    row.tests.push(TestRef {
        package: "augmentagent-channel-slack".into(),
        target: "test:transport_socket".into(),
        name: "live_socket_mode_hello_roundtrip".into(),
    });
    // A real function in the same file that is not a test.
    row.tests.push(TestRef {
        package: "augmentagent-channel-slack".into(),
        target: "test:surface_contract".into(),
        name: "workspace".into(),
    });
    let found = kinds(&m);
    assert!(
        has(
            &found,
            ViolationKind::TestIgnored,
            "live_socket_mode_hello_roundtrip"
        ),
        "{found:#?}"
    );
    assert!(
        has(&found, ViolationKind::TestNotFound, "workspace"),
        "{found:#?}"
    );
}

#[test]
fn a_supported_row_over_a_capability_the_table_does_not_support_fails() {
    let mut m = matrix();
    let row = row_mut(&mut m, "live-voice");
    row.status = RowStatus::Supported;
    row.blocker = None;
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::StatusAheadOfCapability, "live-voice"),
        "{found:#?}"
    );
}

#[test]
fn a_blocked_row_whose_capabilities_are_all_supported_fails() {
    let mut m = matrix();
    let row = row_mut(&mut m, "approvals");
    row.status = RowStatus::Blocked;
    row.blocker = Some(Blocker {
        issue: 1289,
        reason: "pretend".into(),
    });
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::StatusBehindCapability, "approvals"),
        "{found:#?}"
    );
}

#[test]
fn flipping_the_capability_table_without_the_matrix_fails_both_ways() {
    let m = matrix();
    let root = repo_root();
    let flipped = |name: &str| match name {
        "voice" | "live_voice" => Some(SupportStatus::Supported),
        "voice_clip" => Some(SupportStatus::Unsupported),
        other => augmentagent_channel_slack::parity::capability_status(other),
    };
    let mut ctx = CheckContext::slack(&root);
    ctx.capabilities = &flipped;
    let found: Vec<_> = check(&m, &ctx)
        .into_iter()
        .map(|v| (v.kind, format!("{} {}", v.subject, v.detail)))
        .collect();
    assert!(
        has(&found, ViolationKind::StatusBehindCapability, "live-voice"),
        "{found:#?}"
    );
    assert!(
        has(
            &found,
            ViolationKind::StatusAheadOfCapability,
            "voice-clips"
        ),
        "{found:#?}"
    );
}

#[test]
fn an_unknown_capability_or_one_no_row_covers_fails() {
    let mut m = matrix();
    row_mut(&mut m, "notifications").capabilities = vec!["teleport".into()];
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::UnknownCapability, "teleport"),
        "{found:#?}"
    );
    assert!(
        has(&found, ViolationKind::UncoveredCapability, "notifications"),
        "{found:#?}"
    );
}

#[test]
fn blocked_rows_need_a_reason_and_an_issue_and_live_rows_need_a_reason() {
    let mut m = matrix();
    row_mut(&mut m, "live-voice").blocker = None;
    row_mut(&mut m, "voice-clips").blocker = Some(Blocker {
        issue: 0,
        reason: " ".into(),
    });
    row_mut(&mut m, "approvals").blocker = Some(Blocker {
        issue: 1289,
        reason: "a supported row carrying a blocker".into(),
    });
    row_mut(&mut m, "acceptance").live_reason = None;
    let found = kinds(&m);
    for id in ["live-voice", "voice-clips", "approvals"] {
        assert!(
            has(&found, ViolationKind::BlockerMismatch, id),
            "{id}: {found:#?}"
        );
    }
    assert!(
        has(&found, ViolationKind::LiveReasonMissing, "acceptance"),
        "{found:#?}"
    );
}

#[test]
fn an_unknown_status_is_rejected_when_the_matrix_is_parsed() {
    let text = std::fs::read_to_string(repo_root().join(MATRIX_PATH))
        .unwrap()
        .replacen("\"status\": \"supported\"", "\"status\": \"verified\"", 1);
    assert!(parse(&text).is_err());
}

// --- host evidence --------------------------------------------------------

#[test]
fn no_row_claims_real_host_evidence_yet() {
    let m = matrix();
    let hosts: Vec<&str> = m.hosts.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(hosts, ["linux", "macos-arm64", "macos-x86_64"]);
    for row in &m.rows {
        let keys: Vec<&str> = row.host_evidence.keys().map(String::as_str).collect();
        assert_eq!(keys, ["linux", "macos-arm64", "macos-x86_64"], "{}", row.id);
        assert!(
            row.host_evidence.values().all(Option::is_none),
            "{}: real-host evidence is recorded only from the runbook acceptance script",
            row.id
        );
    }
    // Linux credentials persist since #1325; only real-host acceptance remains.
    let linux = &m.hosts[0];
    assert!(linux.blockers.is_empty(), "{:?}", linux.blockers);
    // No provider call can run under the CLI supervisor on macOS until #1252.
    for mac in &m.hosts[1..] {
        assert!(mac.blockers.iter().any(|b| b.issue == 1252), "{}", mac.id);
    }
}

fn evidence() -> HostEvidence {
    HostEvidence {
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        os_version: "macOS 15.4".into(),
        architecture: "arm64".into(),
        commands: vec!["augmentagent status --json".into()],
        artifact: "acceptance/macos-arm64/step-06.log".into(),
        result: "pass".into(),
        date: "2026-10-01".into(),
    }
}

#[test]
fn host_evidence_must_be_complete_and_for_a_declared_host() {
    let mut m = matrix();
    row_mut(&mut m, "approvals")
        .host_evidence
        .insert("macos-arm64".into(), Some(evidence()));
    assert!(kinds(&m).is_empty(), "complete evidence is accepted");

    let mut partial = evidence();
    partial.commit = "abc123".into();
    partial.result = "todo".into();
    row_mut(&mut m, "scheduling")
        .host_evidence
        .insert("linux".into(), Some(partial));
    row_mut(&mut m, "files-in")
        .host_evidence
        .insert("windows".into(), None);
    row_mut(&mut m, "files-in").host_evidence.remove("linux");
    let found = kinds(&m);
    assert!(
        has(&found, ViolationKind::EvidenceIncomplete, "scheduling"),
        "{found:#?}"
    );
    assert!(
        has(&found, ViolationKind::HostMismatch, "files-in"),
        "{found:#?}"
    );
    assert!(!has(&found, ViolationKind::EvidenceIncomplete, "approvals"));
}

// --- the shared behavior suite -------------------------------------------

#[test]
fn every_shared_scenario_runs_for_slack_and_names_each_other_surface() {
    let m = matrix();
    let ids: Vec<&str> = m.shared_scenarios.iter().map(|s| s.id.as_str()).collect();
    for needed in [
        "native-session-conformance",
        "approval-sync",
        "owner-command-registry",
        "durable-delivery",
    ] {
        assert!(ids.contains(&needed), "{needed} missing from {ids:?}");
    }
    for scenario in &m.shared_scenarios {
        assert!(
            !scenario.surfaces["slack"].tests.is_empty(),
            "{}",
            scenario.id
        );
        assert!(
            !scenario.surfaces["discord"].tests.is_empty(),
            "{}",
            scenario.id
        );
        let whatsapp = &scenario.surfaces["whatsapp"];
        assert!(
            !whatsapp.tests.is_empty() || whatsapp.gap.is_some(),
            "{}",
            scenario.id
        );
    }

    let mut broken = m.clone();
    broken.shared_scenarios[0].surfaces.remove("discord");
    let slack = broken.shared_scenarios[1]
        .surfaces
        .get_mut("slack")
        .unwrap();
    slack.tests.clear();
    slack.gap = Some(Blocker {
        issue: 1289,
        reason: "a gap never stands in for the Slack side".into(),
    });
    let found = kinds(&broken);
    assert!(
        has(
            &found,
            ViolationKind::ScenarioSurfaceMissing,
            "native-session-conformance/discord"
        ),
        "{found:#?}"
    );
    assert!(
        has(
            &found,
            ViolationKind::ScenarioSurfaceMissing,
            "approval-sync/slack"
        ),
        "{found:#?}"
    );
}

// --- report and CI commands ----------------------------------------------

#[test]
fn the_report_lists_every_row_and_every_open_blocker() {
    let m = matrix();
    let text = render_text(&report(&m, &[]));
    assert!(text.contains("Matrix check: ok"), "{text}");
    for row in &m.rows {
        assert!(text.contains(&row.id), "{} missing:\n{text}", row.id);
    }
    for needle in [
        "supported        notifications",
        "supported        voice-clips",
        "blocked          live-voice",
        "unverified-live  acceptance",
        "#1295",
        "#1297",
        "#1298",
        "host macos-arm64 (#1252)",
        "Open blockers",
        "host-acceptance linux",
        "real-host acceptance not recorded",
        "docs/SLACK-RUNBOOK.md section 11",
        "approval-sync/whatsapp",
    ] {
        assert!(text.contains(needle), "`{needle}` missing:\n{text}");
    }
    // Deterministic.
    assert_eq!(text, render_text(&report(&m, &[])));
}

#[test]
fn the_json_report_carries_statuses_blockers_and_the_check_result() {
    let m = matrix();
    let value = serde_json::to_value(report(&m, &[])).unwrap();
    assert_eq!(value["epic"], 1281);
    assert_eq!(value["check"]["ok"], true);
    assert_eq!(value["rows"].as_array().unwrap().len(), 20);
    let summary = &value["summary"];
    let total = summary["supported"].as_u64().unwrap()
        + summary["blocked"].as_u64().unwrap()
        + summary["unverified_live"].as_u64().unwrap();
    assert_eq!(total, 20);
    assert_eq!(summary["blocked"], 1);
    let live = value["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "live-voice")
        .unwrap();
    assert_eq!(live["status"], "blocked");
    assert_eq!(live["blocker"]["issue"], 1298);
    assert_eq!(live["hosts"]["macos-arm64"], "pending");
    let blockers = value["open_blockers"].as_array().unwrap();
    for (kind, subject) in [
        ("blocked", "live-voice"),
        ("unverified-live", "transport"),
        ("host", "macos-arm64"),
        ("host-acceptance", "linux"),
        ("host-acceptance", "macos-x86_64"),
        ("shared-suite-gap", "durable-delivery/whatsapp"),
    ] {
        assert!(
            blockers
                .iter()
                .any(|b| b["kind"] == kind && b["subject"] == subject),
            "{kind} {subject} missing: {blockers:#?}"
        );
    }
}

#[test]
fn a_broken_matrix_is_reported_as_failing_with_each_violation() {
    let mut m = matrix();
    row_mut(&mut m, "approvals").tests.clear();
    let root = repo_root();
    let violations = check(&m, &CheckContext::slack(&root));
    let r = report(&m, &violations);
    assert!(!r.check.ok);
    let text = render_text(&r);
    assert!(text.contains("Matrix check: FAILED (1 problem)"), "{text}");
    assert!(text.contains("no-named-test approvals"), "{text}");
}

#[test]
fn every_named_test_is_run_by_exactly_one_generated_cargo_command() {
    let m = matrix();
    let commands = cargo_commands(&m);
    assert_eq!(commands, cargo_commands(&m), "deterministic");
    assert!(commands.iter().all(|c| c.starts_with("cargo test -p ")));
    assert!(commands.contains(&"cargo test -p augmentagent-approval-discord --lib".to_string()));
    assert!(commands.contains(&"cargo test -p augmentagent-channel-whatsapp --lib".to_string()));
    let all = m
        .rows
        .iter()
        .flat_map(|r| r.tests.iter())
        .chain(m.shared_scenarios.iter().flat_map(|s| {
            s.shared
                .iter()
                .chain(s.surfaces.values().flat_map(|c| c.tests.iter()))
        }));
    for test in all {
        let selector = match test.target.as_str() {
            "lib" => "--lib".to_string(),
            t if t.starts_with("bin:") => format!("--bin {}", &t[4..]),
            t => format!("--test {}", t.trim_start_matches("test:")),
        };
        let prefix = format!("cargo test -p {} {selector} -- ", test.package);
        let running: Vec<_> = commands.iter().filter(|c| c.starts_with(&prefix)).collect();
        assert_eq!(running.len(), 1, "{prefix}");
        assert!(
            running[0].split(' ').any(|w| w == test.name),
            "{} not in {}",
            test.name,
            running[0]
        );
    }
}

// #1297 — the voice-clips row is supported by the serve-path tests.
#[test]
fn the_voice_clips_row_is_supported_by_the_serve_path_tests() {
    let m = matrix();
    let row = m.rows.iter().find(|r| r.id == "voice-clips").unwrap();
    assert_eq!(row.status, RowStatus::Supported);
    assert!(row.blocker.is_none());
    for name in [
        "an_owner_clip_is_a_turn_in_the_same_conversation_with_the_transcript_shown",
        "voice_on_answers_with_audio_plus_the_text_once_and_voice_off_stops_it",
        "credit_exhaustion_402_switches_to_the_other_vendor",
        "clip_is_transcribed_into_turn_text_with_a_transcript_line",
        "spoken_reply_is_an_audio_upload_plus_the_text_mirror_delivered_once",
    ] {
        assert!(
            row.tests.iter().any(|t| t.name == name),
            "{name} missing from voice-clips"
        );
    }
    let live = m.rows.iter().find(|r| r.id == "live-voice").unwrap();
    assert_eq!(live.status, RowStatus::Blocked);
}
