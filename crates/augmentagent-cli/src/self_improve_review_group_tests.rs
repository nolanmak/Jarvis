//! Scripted transport fixtures exercise the production group adapter. They do
//! not create a native VM/provider session or establish real review authority.
use super::*;
use std::sync::Mutex;

struct ScriptedGroup {
    binding: aligned::ReviewBinding,
    role: aligned::ReviewRole,
    typed: bool,
    defect: bool,
    initial_finding: bool,
    calls: Mutex<usize>,
}
fn finding(id: &str, claim: &str) -> aligned::Finding {
    aligned::Finding {
        id: id.into(),
        criterion_id: "C1".into(),
        evidence_hash: aligned::digest(claim.as_bytes()),
        claim: claim.into(),
        requested_change: "Return the configured cap without a panic or overflow".into(),
        conflicts_with: BTreeSet::new(),
        repair_rounds: 0,
        repair_target: Some(aligned::RepairTarget::Candidate {
            paths: BTreeSet::from(["logic.rs".into()]),
        }),
    }
}
#[async_trait::async_trait]
impl GroupTransport for ScriptedGroup {
    async fn observe_group(
        &self,
        provider: augmentagent_channel_core::ProviderKind,
        opts: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> Result<(String, String)> {
        assert_eq!(provider, augmentagent_channel_core::ProviderKind::Codex);
        assert!(opts.restrict_env);
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        assert!(*calls <= 2, "the adapter exceeded its observation bound");
        let mut response =
            serde_json::json!({"binding":self.binding,"decision":"incomplete","findings":[]});
        if *calls == 1 {
            if self.initial_finding {
                response["findings"] = serde_json::json!([finding(
                    "panic",
                    "The original assigned source panics unconditionally"
                )]);
            }
            if self.typed {
                response["missing_sources"] = serde_json::json!([
                    {"path":"logic.rs","criterion_ids":["C1"],"question":"What configured cap must the caller preserve?","selector":{"kind":"rust_item","name":"policy_limit"}},
                    {"path":"logic.rs","criterion_ids":["C1"],"question":"Does application preserve that cap for input 5?","selector":{"kind":"rust_item","name":"apply_limit"}}
                ]);
            } else {
                response["missing_paths"] = serde_json::json!(["logic.rs"]);
            }
        } else {
            let request: serde_json::Value = serde_json::from_str(prompt)?;
            let source = request["full_file_evidence"]
                .as_object()
                .unwrap()
                .values()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::ensure!(source.contains("fn policy_limit()") && source.contains("fn apply_limit("),
                "coherent recovery must deliver both related immutable items in the same actual reviewer request");
            anyhow::ensure!(
                source.contains("original assigned evidence"),
                "recovery dropped original assigned evidence"
            );
            anyhow::ensure!(
                prompt.len() + opts.system_prompt.len() <= aligned::MAX_PROMPT_BYTES,
                "actual serialized recovery request exceeds the existing limit"
            );
            if self.defect {
                anyhow::ensure!(
                    source.contains("input + policy_limit()"),
                    "the seeded cross-item defect was not supplied"
                );
                response["decision"] = "changes_requested".into();
                response["findings"] = serde_json::json!([finding(
                    "cap-overflow",
                    "apply_limit(5) returns 8 although policy_limit is 3"
                )]);
            } else {
                anyhow::ensure!(
                    source.contains("input.min(policy_limit())"),
                    "the fixed cross-item behavior was not supplied"
                );
                response["decision"] = "approve".into();
            }
        }
        Ok((
            response.to_string(),
            format!("scripted-fixture-{:?}-{}", self.role, *calls),
        ))
    }
}
fn git_fixture(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
async fn run_group_case(
    role: aligned::ReviewRole,
    typed: bool,
    large: bool,
    defect: bool,
    initial_finding: bool,
) -> (Result<CompletedGroup>, usize) {
    let root = tempfile::tempdir().unwrap();
    git_fixture(root.path(), &["init", "-q"]);
    let padding = if large {
        "x".repeat(aligned::MAX_PROMPT_BYTES + 4096)
    } else {
        "small".into()
    };
    let implementation = if defect {
        "input + policy_limit()"
    } else {
        "input.min(policy_limit())"
    };
    let source=format!("pub fn policy_limit() -> i32 {{ 3 }}\n/*{padding}*/\npub fn apply_limit(input:i32) -> i32 {{ {implementation} }}\n");
    assert_eq!(source.len() > aligned::MAX_PROMPT_BYTES, large);
    std::fs::write(root.path().join("logic.rs"), source).unwrap();
    let original_body = if initial_finding {
        "// original assigned evidence\nmod logic; fn main() { panic!(\"forced regression\") }"
    } else {
        "// original assigned evidence\nmod logic; fn main() { println!(\"{}\", logic::apply_limit(5)); }"
    };
    std::fs::write(root.path().join("changed.rs"), original_body).unwrap();
    // Verify the fixture's cross-item behavior with the real compiler before
    // any scripted observation; fixture mistakes are not protocol RED.
    let executable = tempfile::tempdir().unwrap();
    let mut compiler = std::process::Command::new("rustc");
    compiler.env_clear();
    for name in ["PATH", "HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        if let Some(value) = std::env::var_os(name) {
            compiler.env(name, value);
        }
    }
    let compiled = compiler
        .args(["--edition=2021"])
        .arg(root.path().join("changed.rs"))
        .arg("-o")
        .arg(executable.path().join("observed"))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let observed = std::process::Command::new(executable.path().join("observed"))
        .env_clear()
        .output()
        .unwrap();
    if initial_finding {
        assert!(!observed.status.success());
        assert!(String::from_utf8_lossy(&observed.stderr).contains("forced regression"));
    } else {
        assert!(observed.status.success());
        assert_eq!(
            String::from_utf8(observed.stdout).unwrap().trim(),
            if defect { "8" } else { "3" }
        );
    }
    git_fixture(root.path(), &["add", "logic.rs", "changed.rs"]);
    git_fixture(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "immutable fixture",
        ],
    );
    let head = git_fixture(root.path(), &["rev-parse", "HEAD"]);
    let binding = aligned::ReviewBinding {
        head: head.clone(),
        base: head,
        environment_id: "fixture-environment".into(),
        contract_hash: "c".repeat(64),
        evidence_hash: "d".repeat(64),
    };
    let contract = BTreeMap::from([(
        "C1".into(),
        "Cap every returned value at policy_limit without panicking".into(),
    )]);
    let inputs = evidence_plan::Inputs {
        binding: &binding,
        issue: "Enforce configured cap",
        contract: &contract,
        prior: &[],
        system_bytes: 1024,
        controller_acceptance: None,
    };
    let original = inputs
        .request(
            BTreeMap::from([("original-scope".into(), original_body.into())]),
            false,
            None,
        )
        .unwrap();
    let actor = aligned::Actor {
        provider: "codex".into(),
        backend: "native".into(),
        model: "fixture".into(),
        session_id: format!("fixture-role-{role:?}"),
    };
    let builders = [aligned::Actor {
        provider: "claude".into(),
        backend: "native".into(),
        model: "fixture".into(),
        session_id: "fixture-writer".into(),
    }];
    let origin = aligned::ReviewOrigin::RecordedBuilders;
    let opts = codex_review_opts(root.path().to_path_buf(), "Scripted protocol fixture");
    let transport = ScriptedGroup {
        binding: binding.clone(),
        role,
        typed,
        defect,
        initial_finding,
        calls: Mutex::new(0),
    };
    let result = execute_evidence_group(
        EvidenceGroupInput {
            root: root.path(),
            inputs: &inputs,
            original_request: &original,
            evidence_shards: &original.files,
            actor: &actor,
            builders: &builders,
            origin: &origin,
            role,
            provider: augmentagent_channel_core::ProviderKind::Codex,
            opts: &opts,
            group_index: 0,
            workflow: None,
        },
        &transport,
    )
    .await;
    let calls = *transport.calls.lock().unwrap();
    (result, calls)
}

#[tokio::test]
async fn legacy_whole_file_recovery_keeps_original_findings_after_later_approval() {
    let (result, calls) =
        run_group_case(aligned::ReviewRole::Diff, false, false, false, true).await;
    let group = result.unwrap();
    assert_eq!(calls, 2);
    assert_eq!(group.findings.len(), 1);
    assert_eq!(group.findings[0].id, "panic");
    assert!(group
        .recovered_files
        .values()
        .any(|s| s.contains("fn policy_limit()")));
}

#[tokio::test]
async fn related_large_source_items_reach_one_request_and_reject_the_seeded_defect() {
    for role in evidence_plan::ROLES {
        let (result, calls) = run_group_case(role, true, true, true, false).await;
        let group = result
            .expect("large-file dependency recovery must supply both requested items coherently");
        assert_eq!(calls, 2);
        assert_eq!(group.findings.len(), 1);
        assert_eq!(group.findings[0].id, "cap-overflow");
        assert_eq!(group.native_sessions.len(), 2);
    }
}

#[tokio::test]
async fn related_large_source_items_allow_a_complete_fixed_group_without_dropping_scope() {
    for role in evidence_plan::ROLES {
        let (result, calls) = run_group_case(role, true, true, false, false).await;
        let group = result
            .expect("coherent immutable source recovery must support the fixed candidate too");
        assert_eq!(calls, 2);
        assert!(group.findings.is_empty());
        assert_eq!(group.native_sessions.len(), 2);
    }
}
