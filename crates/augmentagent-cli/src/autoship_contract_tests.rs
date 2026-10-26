use super::*;

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

fn contract() -> AcceptanceContract {
    AcceptanceContract {
        schema_version: SCHEMA_VERSION,
        revision: 1,
        issue: Some(1598),
        pull_request: None,
        criteria: vec![Criterion {
            id: "C1".into(),
            requirement: "Ready state is returned".into(),
            verifier_ids: vec!["ready".into()],
        }],
        allowed_paths: vec![PathScope::Directory("src".into())],
        verifiers: vec![Verifier {
            id: "ready".into(),
            command: VerifierCommand::CargoTest {
                package: "fixture".into(),
                target: CargoTarget::Lib,
                test: "tests::ready".into(),
            },
            pinned_files: vec!["verifier.rs".into()],
            failure_witness: "expected ready".into(),
        }],
    }
}
fn registered() -> (tempfile::TempDir, RegisteredContract) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("verifier.rs"),
        "assert_eq!(ready(), true, \"expected ready\");",
    )
    .unwrap();
    let registered = RegisteredContract::register(
        dir.path(),
        contract(),
        BASE.into(),
        "rust-1-fixture-env-v1".into(),
    )
    .unwrap();
    (dir, registered)
}
fn observation(code: Option<i32>, text: &str) -> Observation {
    Observation {
        exit_code: code,
        stdout: text.into(),
        stderr: String::new(),
        interrupted: false,
        output_complete: true,
    }
}
fn red() -> Observation {
    observation(
        Some(101),
        "test tests::ready ... FAILED\nexpected ready\ntest result: FAILED. 0 passed; 1 failed",
    )
}
fn green() -> Observation {
    observation(
        Some(0),
        "test tests::ready ... ok\ntest result: ok. 1 passed; 0 failed",
    )
}
fn receipt(reg: &RegisteredContract, dir: &Path, head: &str, obs: &Observation) -> VerifierReceipt {
    reg.observe(dir, "ready", reg.binding(head).unwrap(), obs)
        .unwrap()
}

#[test]
fn accepts_only_actual_red_to_green_for_same_registered_inputs() {
    let (dir, reg) = registered();
    let before = receipt(&reg, dir.path(), BASE, &red());
    let after = receipt(&reg, dir.path(), HEAD, &green());
    assert_eq!(
        reg.baseline_result(&[before.clone()]).unwrap(),
        BaselineResult::Reproduced
    );
    assert!(reg
        .candidate_passes(&[before], &[after], &reg.binding(HEAD).unwrap())
        .unwrap());
}

#[test]
fn baseline_pass_is_not_proof_of_obsolescence_or_success() {
    let (dir, reg) = registered();
    let before = receipt(&reg, dir.path(), BASE, &green());
    assert_eq!(
        reg.baseline_result(&[before.clone()]).unwrap(),
        BaselineResult::AlreadyPasses(vec!["ready".into()])
    );
    let after = receipt(&reg, dir.path(), HEAD, &green());
    assert!(!reg
        .candidate_passes(&[before], &[after], &reg.binding(HEAD).unwrap())
        .unwrap());
}

#[test]
fn compile_errors_missing_tests_and_wrong_assertions_do_not_count_as_red() {
    let (dir, reg) = registered();
    for output in [
        "error: expected ready",
        "running 0 tests\ntest result: ok. 0 passed",
        "test tests::other ... FAILED\nexpected ready",
        "test tests::ready ... FAILED\nnetwork unavailable",
    ] {
        let result = receipt(&reg, dir.path(), BASE, &observation(Some(101), output));
        assert_eq!(result.result, CheckResult::UnexpectedFailure, "{output}");
        assert!(matches!(
            reg.baseline_result(&[result]).unwrap(),
            BaselineResult::NotReproduced(_)
        ));
    }
}

#[test]
fn interrupted_or_truncated_output_cannot_attest_success_or_red() {
    let (dir, reg) = registered();
    let mut interrupted = green();
    interrupted.interrupted = true;
    let mut truncated = red();
    truncated.output_complete = false;
    let missing_status = observation(None, "test tests::ready ... ok");
    for obs in [interrupted, truncated, missing_status] {
        assert_eq!(
            receipt(&reg, dir.path(), BASE, &obs).result,
            CheckResult::InfrastructureFailure
        );
    }
}

#[test]
fn weakening_verifier_after_registration_is_rejected() {
    let (dir, reg) = registered();
    std::fs::write(dir.path().join("verifier.rs"), "assert!(true);").unwrap();
    assert!(reg.verify_integrity(dir.path()).is_err());
    assert!(reg
        .observe(dir.path(), "ready", reg.binding(HEAD).unwrap(), &green())
        .is_err());
}

#[test]
fn contract_and_test_receipts_survive_serialization_without_losing_binding() {
    let (dir, reg) = registered();
    let restored: RegisteredContract =
        serde_json::from_slice(&serde_json::to_vec(&reg).unwrap()).unwrap();
    restored.verify_integrity(dir.path()).unwrap();
    assert_eq!(restored, reg);
    let mut encoded = serde_json::to_value(reg).unwrap();
    encoded["contract"]["criteria"][0]["requirement"] = "different target".into();
    let changed: RegisteredContract = serde_json::from_value(encoded).unwrap();
    assert!(changed.verify_integrity(dir.path()).is_err());
}

#[test]
fn changes_to_head_base_environment_or_contract_invalidate_evidence() {
    let (dir, reg) = registered();
    let before = receipt(&reg, dir.path(), BASE, &red());
    let after = receipt(&reg, dir.path(), HEAD, &green());
    for field in ["head", "base", "environment_id", "contract_hash"] {
        let mut stale = after.clone();
        match field {
            "head" => stale.binding.head = BASE.into(),
            "base" => stale.binding.base = HEAD.into(),
            "environment_id" => stale.binding.environment_id = "different".into(),
            _ => stale.binding.contract_hash = "different".into(),
        }
        assert!(
            reg.candidate_passes(&[before.clone()], &[stale], &reg.binding(HEAD).unwrap())
                .is_err(),
            "{field}"
        );
    }
}

#[test]
fn duplicate_and_missing_receipts_fail_closed() {
    let (dir, reg) = registered();
    assert!(reg.baseline_result(&[]).is_err());
    let r = receipt(&reg, dir.path(), BASE, &red());
    assert!(reg.baseline_result(&[r.clone(), r]).is_err());
}

#[test]
fn command_arguments_cannot_introduce_shell_or_extra_cargo_flags() {
    for name in [
        "--workspace",
        "foo; rm -rf x",
        "$(touch x)",
        "foo\n--ignored",
        "",
    ] {
        let command = VerifierCommand::CargoTest {
            package: name.into(),
            target: CargoTarget::Lib,
            test: "test".into(),
        };
        assert!(command.invocation().is_err());
    }
    let cmd = contract().verifiers[0].command.invocation().unwrap();
    assert_eq!(cmd.program, "cargo");
    assert_eq!(
        cmd.args,
        [
            "test",
            "--locked",
            "-p",
            "fixture",
            "--lib",
            "tests::ready",
            "--",
            "--exact",
            "--test-threads=1"
        ]
    );
}

#[test]
fn malformed_missing_or_unmapped_criteria_require_rescoping() {
    let mut c = contract();
    c.criteria.clear();
    assert!(c.validate().is_err());
    let mut c = contract();
    c.criteria[0].verifier_ids.clear();
    assert!(c.validate().is_err());
    let mut c = contract();
    c.criteria[0].verifier_ids = vec!["missing".into()];
    assert!(c.validate().is_err());
    let mut c = contract();
    c.criteria.push(c.criteria[0].clone());
    assert!(c.validate().is_err());
    let mut c = contract();
    c.schema_version += 1;
    assert!(c.validate().is_err());
}

#[test]
fn symlinked_or_escaping_harness_cannot_be_registered() {
    let (dir, reg) = registered();
    for path in [
        "../verifier.rs",
        "/tmp/verifier.rs",
        "./verifier.rs",
        "src//verifier.rs",
        "src/../verifier.rs",
        "src\\verifier.rs",
    ] {
        let mut c = contract();
        c.verifiers[0].pinned_files = vec![path.into()];
        assert!(RegisteredContract::register(dir.path(), c, BASE.into(), "env".into()).is_err());
    }
    #[cfg(unix)]
    {
        std::fs::rename(
            dir.path().join("verifier.rs"),
            dir.path().join("original.rs"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("original.rs"),
            dir.path().join("verifier.rs"),
        )
        .unwrap();
        assert!(reg.verify_integrity(dir.path()).is_err());
    }
}

#[test]
fn scope_uses_path_boundaries_and_alignment_cannot_hide_unrelated_hunks() {
    let c = contract();
    let good = ChangedHunk::new("src/main.rs", b"+ready()").unwrap();
    let unrelated = ChangedHunk::new("src-elsewhere/main.rs", b"+other()").unwrap();
    let mapping = AlignmentMapping {
        hunk_id: good.id.clone(),
        criterion_ids: vec!["C1".into()],
        supporting_reason: None,
    };
    let report =
        check_alignment(&c, &[good.clone(), unrelated.clone()], &[mapping.clone()]).unwrap();
    assert!(!report.complete());
    assert_eq!(report.outside_scope, ["src-elsewhere/main.rs"]);
    assert_eq!(report.unmapped_hunks, [unrelated.id]);
    assert!(check_alignment(&c, &[good], &[mapping]).unwrap().complete());
}

#[test]
fn missing_criteria_and_empty_patches_cannot_align() {
    let mut c = contract();
    c.criteria.push(Criterion {
        id: "C2".into(),
        requirement: "another behavior".into(),
        verifier_ids: vec!["ready".into()],
    });
    let hunk = ChangedHunk::new("src/main.rs", b"+ready()").unwrap();
    let mapping = AlignmentMapping {
        hunk_id: hunk.id.clone(),
        criterion_ids: vec!["C1".into()],
        supporting_reason: None,
    };
    assert_eq!(
        check_alignment(&c, &[hunk], &[mapping])
            .unwrap()
            .uncovered_criteria,
        ["C2"]
    );
    assert!(check_alignment(&c, &[], &[]).is_err());
}

#[test]
fn unknown_hunks_or_criteria_cannot_forge_alignment() {
    let c = contract();
    let hunk = ChangedHunk::new("src/main.rs", b"+ready()").unwrap();
    let mut mapping = AlignmentMapping {
        hunk_id: "made-up".into(),
        criterion_ids: vec!["C1".into()],
        supporting_reason: None,
    };
    assert!(check_alignment(&c, &[hunk.clone()], &[mapping.clone()]).is_err());
    mapping.hunk_id = hunk.id.clone();
    mapping.criterion_ids = vec!["made-up".into()];
    assert!(check_alignment(&c, &[hunk], &[mapping]).is_err());
}

#[test]
fn constraint_text_is_literal_and_never_executed() {
    let constraints = vec![
        Constraint::ForbiddenPath {
            id: "private".into(),
            source: "https://github.com/example/repo/issues/1".into(),
            scope: PathScope::Directory("wiki".into()),
        },
        Constraint::ForbiddenAddedText {
            id: "literal".into(),
            source: "https://github.com/example/repo/issues/2".into(),
            literal: "$(touch /tmp/should-never-run)".into(),
        },
    ];
    assert_eq!(
        constraint_failures(
            &constraints,
            &["wiki/index.md".into()],
            &["+ $(touch /tmp/should-never-run)".into()]
        )
        .unwrap(),
        ["private", "literal"]
    );
    let shell = r#"{"kind":"shell_predicate","id":"x","source":"https://github.com/example/repo/issues/1","command":"touch x"}"#;
    assert!(serde_json::from_str::<Constraint>(shell).is_err());
}

#[test]
fn real_unittest_baseline_and_candidate_prove_behavior_without_mutating_evaluator() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test_guard.py"), "import unittest\nfrom behavior import ready\nclass Guard(unittest.TestCase):\n def test_ready(self):\n  self.assertTrue(ready(), 'expected ready')\n").unwrap();
    std::fs::write(
        dir.path().join("behavior.py"),
        "def ready():\n return False\n",
    )
    .unwrap();
    let mut c = contract();
    c.verifiers[0].command = VerifierCommand::PythonUnittest {
        test: "test_guard.Guard.test_ready".into(),
    };
    c.verifiers[0].pinned_files = vec!["test_guard.py".into()];
    let reg = RegisteredContract::register(dir.path(), c, BASE.into(), "python-fixture-v1".into())
        .unwrap();
    let run = || {
        let invocation = reg.contract().verifiers[0].command.invocation().unwrap();
        let output = std::process::Command::new(invocation.program)
            .args(invocation.args)
            .current_dir(dir.path())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        Observation {
            exit_code: output.status.code(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            interrupted: false,
            output_complete: true,
        }
    };
    let baseline = receipt(&reg, dir.path(), BASE, &run());
    assert_eq!(baseline.result, CheckResult::IntendedFailure);
    std::fs::write(
        dir.path().join("behavior.py"),
        "def ready():\n return True\n",
    )
    .unwrap();
    let candidate = receipt(&reg, dir.path(), HEAD, &run());
    assert_eq!(candidate.result, CheckResult::Passed);
    assert!(reg
        .candidate_passes(&[baseline], &[candidate], &reg.binding(HEAD).unwrap())
        .unwrap());
}

#[test]
fn legacy_issue_json_and_real_pull_request_identity_are_distinct() {
    let legacy = contract();
    let json = serde_json::to_value(&legacy).unwrap();
    assert_eq!(json["issue"], 1598);
    assert!(json.get("pull_request").is_none());
    let loaded: AcceptanceContract = serde_json::from_value(json).unwrap();
    assert_eq!(loaded.source().unwrap(), AcceptanceSource::Issue(1598));
    let mut imported = legacy.clone();
    imported.issue = None;
    imported.pull_request = Some(1598);
    imported.validate().unwrap();
    assert_eq!(
        imported.source().unwrap(),
        AcceptanceSource::PullRequest(1598)
    );
    assert_ne!(
        imported.source().unwrap().key(),
        loaded.source().unwrap().key()
    );
    let imported_json = serde_json::to_value(&imported).unwrap();
    assert!(imported_json.get("issue").is_none());
    assert_eq!(imported_json["pull_request"], 1598);
    imported.issue = Some(1598);
    assert!(imported.validate().is_err());
    imported.issue = None;
    imported.pull_request = Some(0);
    assert!(imported.validate().is_err());
    imported.pull_request = None;
    assert!(imported.validate().is_err());
}

#[test]
fn authorized_revision_binds_distinct_baseline_and_candidate_environments() {
    let (dir, old) = registered();
    let mut next = contract();
    next.revision += 1;
    let reg = RegisteredContract::register_transition(
        dir.path(),
        next,
        BASE.into(),
        "old-env".into(),
        "new-env".into(),
    )
    .unwrap();
    assert_ne!(old.contract_hash(), reg.contract_hash());
    let red = receipt(&reg, dir.path(), BASE, &red());
    let green = receipt(&reg, dir.path(), HEAD, &green());
    assert_eq!(red.binding.environment_id, "old-env");
    assert_eq!(green.binding.environment_id, "new-env");
    assert!(reg
        .candidate_passes(&[red], &[green], &reg.binding(HEAD).unwrap())
        .unwrap());
    let mut bytes = serde_json::to_value(&reg).unwrap();
    bytes["candidate_environment_id"] = serde_json::json!("forged-env");
    let forged: RegisteredContract = serde_json::from_value(bytes).unwrap();
    assert!(forged.verify_integrity(dir.path()).is_err());
}
