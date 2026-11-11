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
    requested_path: String,
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
            paths: BTreeSet::from([if id == "panic" {
                "changed.rs".into()
            } else {
                "logic.rs".into()
            }]),
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
                response["missing_paths"] = serde_json::json!([self.requested_path]);
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
    run_group_path_case(role, typed, large, defect, initial_finding, "logic.rs").await
}
async fn run_group_path_case(
    role: aligned::ReviewRole,
    typed: bool,
    large: bool,
    defect: bool,
    initial_finding: bool,
    requested_path: &str,
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
    let mut paths = vec!["add", "logic.rs", "changed.rs"];
    if requested_path != "logic.rs" {
        let protected = root.path().join(requested_path);
        std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
        std::fs::write(protected, "SYNTHETIC_CREDENTIAL_MUST_REMAIN_IN_FIXTURE").unwrap();
        paths.push(requested_path);
    }
    git_fixture(root.path(), &paths);

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
        requested_path: requested_path.into(),
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

#[tokio::test]
async fn credential_and_control_paths_are_refused_before_a_second_observation() {
    for path in [
        ".ssh/credentials.json",
        ".config/auth.json",
        "credentials.json",
        "src/\nsecret.rs",
    ] {
        let (result, calls) =
            run_group_path_case(aligned::ReviewRole::Diff, false, false, false, false, path).await;
        assert!(result.is_err(), "a denied source cannot complete review");
        assert_eq!(
            calls, 1,
            "protected source {path:?} reached a paid recovery observation"
        );
    }
}

struct ProtocolSequence {
    responses: Mutex<std::collections::VecDeque<serde_json::Value>>,
    calls: Mutex<usize>,
    native_id: String,
}
#[async_trait::async_trait]
impl GroupTransport for ProtocolSequence {
    async fn observe_group(
        &self,
        _: augmentagent_channel_core::ProviderKind,
        _: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> Result<(String, String)> {
        *self.calls.lock().unwrap() += 1;
        let mut response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra observation");
        if let Some(raw) = response.as_str() {
            return Ok((raw.into(), self.native_id.clone()));
        }
        response["binding"] = serde_json::from_str::<serde_json::Value>(prompt)?["binding"].clone();
        Ok((response.to_string(), self.native_id.clone()))
    }
}
async fn bounded_protocol_case(
    actor_session: &str,
    native_id: &str,
    responses: Vec<serde_json::Value>,
) -> (Result<CompletedGroup>, usize) {
    bounded_protocol_source_case(actor_session, native_id, responses, "fn source() {}\n").await
}
async fn bounded_protocol_source_case(
    actor_session: &str,
    native_id: &str,
    responses: Vec<serde_json::Value>,
    source: &str,
) -> (Result<CompletedGroup>, usize) {
    let root = tempfile::tempdir().unwrap();
    git_fixture(root.path(), &["init", "-q"]);
    std::fs::write(root.path().join("source.rs"), source).unwrap();
    git_fixture(root.path(), &["add", "."]);
    git_fixture(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "protocol fixture",
        ],
    );
    let head = git_fixture(root.path(), &["rev-parse", "HEAD"]);
    let binding = aligned::ReviewBinding {
        head: head.clone(),
        base: head,
        environment_id: "fixture".into(),
        contract_hash: "c".repeat(64),
        evidence_hash: "d".repeat(64),
    };
    let contract = BTreeMap::from([("C1".into(), "Keep source correct".into())]);
    let inputs = evidence_plan::Inputs {
        binding: &binding,
        issue: "Protocol bounds",
        contract: &contract,
        prior: &[],
        system_bytes: 1024,
        controller_acceptance: None,
    };
    let prefix = source
        .lines()
        .next()
        .map(|line| format!("{line}\n"))
        .unwrap_or_default();
    let original = inputs
        .request(
            BTreeMap::from([(format!("source.rs#bytes-0-{}", prefix.len()), prefix)]),
            false,
            None,
        )
        .unwrap();
    let actor = aligned::Actor {
        provider: "codex".into(),
        backend: "native".into(),
        model: "fixture".into(),
        session_id: actor_session.into(),
    };
    let builders = [aligned::Actor {
        provider: "claude".into(),
        backend: "native".into(),
        model: "fixture".into(),
        session_id: "fixture-writer".into(),
    }];
    let origin = aligned::ReviewOrigin::RecordedBuilders;
    let opts = codex_review_opts(root.path().to_path_buf(), "Protocol fixture");
    let transport = ProtocolSequence {
        responses: Mutex::new(responses.into()),
        calls: Mutex::new(0),
        native_id: native_id.into(),
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
            role: aligned::ReviewRole::Diff,
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
async fn malformed_then_incomplete_cannot_read_another_dependency_or_call_again() {
    let (result,calls)=bounded_protocol_case("fixture-reviewer","actual-native-session",vec![
        serde_json::json!("not a structured review"),
        serde_json::json!({"decision":"incomplete","findings":[],"missing_sources":[{"path":"absent.rs","criterion_ids":["C1"],"question":"Need absent source","selector":{"kind":"whole_file"}}]}),
    ]).await;
    assert_eq!(calls, 2);
    assert!(format!(
        "{:#}",
        result.err().expect("review must remain unavailable")
    )
    .contains("remains incomplete after the second observation"));
    let (result, calls) = bounded_protocol_case(
        "fixture-reviewer",
        "actual-native-session",
        vec![
            serde_json::json!("not a structured review"),
            serde_json::json!({"decision":"approve","findings":[]}),
        ],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(calls, 2);
}
#[tokio::test]
async fn native_identity_limits_and_static_metadata_are_enforced_at_dispatch() {
    let approved = serde_json::json!({"decision":"approve","findings":[]});
    for invalid in [
        String::new(),
        "line\nbreak".into(),
        "α".into(),
        "n".repeat(513),
    ] {
        let (result, calls) =
            bounded_protocol_case("fixture-reviewer", &invalid, vec![approved.clone()]).await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
    let (result, calls) =
        bounded_protocol_case("fixture-reviewer", &"n".repeat(512), vec![approved.clone()]).await;
    assert_eq!(calls, 1);
    assert_eq!(result.unwrap().native_sessions, ["n".repeat(512)]);
    let (result, calls) = bounded_protocol_case(
        &"s".repeat(32 * 1024),
        "actual-native-session",
        vec![approved],
    )
    .await;
    assert!(is_evidence_budget(
        &result.err().expect("review must remain unavailable")
    ));
    assert_eq!(
        calls, 0,
        "oversized static metadata must not reach the model transport"
    );
}

#[tokio::test]
async fn all_matching_requested_items_must_fit_before_another_observation() {
    let padding = "x".repeat(300 * 1024);
    let source=format!("mod first {{\nfn cap() {{ /*{padding}*/ }}\n}}\nmod second {{\nfn cap() {{ /*{padding}*/ }}\n}}\n");
    let request = serde_json::json!({"decision":"incomplete","findings":[],"missing_sources":[{"path":"source.rs","criterion_ids":["C1"],"question":"Do both cap implementations preserve behavior?","selector":{"kind":"rust_item","name":"cap"}}]});
    let (result, calls) = bounded_protocol_source_case(
        "fixture-reviewer",
        "actual-native-session",
        vec![request],
        &source,
    )
    .await;
    assert!(is_evidence_budget(
        &result.err().expect("all matches must fit together")
    ));
    assert_eq!(
        calls, 1,
        "a fitting subset must never reach a recovery observation"
    );
}

struct CorrectionCandidateContext {
    calls: Mutex<usize>,
    defect: bool,
    sources: BTreeMap<String,String>,
}
#[async_trait::async_trait]
impl GroupTransport for CorrectionCandidateContext {
    async fn observe_group(&self, _provider: augmentagent_channel_core::ProviderKind,
        opts: &augmentagent_channel_core::ReasonerOpts, prompt: &str) -> Result<(String, String)> {
        let mut calls=self.calls.lock().unwrap();*calls+=1;
        assert!(*calls<=2,"format correction must not acquire another observation");
        if *calls==1 {return Ok(("not a structured review".into(),"native-fixture-first".into()));}
        let request:serde_json::Value=serde_json::from_str(prompt)?;
        assert_eq!(request["evidence_recovery"]["kind"],"structured_review_correction");
        assert!(request["full_file_evidence"]["original-scope"].as_str().unwrap().contains("original assigned evidence"));
        assert!(prompt.len()+opts.system_prompt.len()<=aligned::MAX_PROMPT_BYTES);
        let source=request["full_file_evidence"].as_object().unwrap().values()
            .map(|v|v.as_str().unwrap()).collect::<Vec<_>>().join("\n");
        assert!(!source.contains("uncommitted poison"));
        let files=request["full_file_evidence"].as_object().unwrap();
        let any_source=self.sources.keys().any(|key|files.contains_key(key));
        if any_source {
            for (key,body) in &self.sources {
                assert_eq!(files.get(key).and_then(|value|value.as_str()),Some(body.as_str()),"optional captured source must be complete and byte-exact");
            }
        }
        let supplied=self.sources.keys().all(|key|files.contains_key(key));
        let mut response=serde_json::json!({"binding":request["binding"],"decision":"incomplete","findings":[]});
        if supplied {
            assert!(source.contains("pinned observer fixture"));
            assert!(request["review_scope"].as_str().unwrap().starts_with("Partial evidence group"));
            if self.defect {
                assert!(source.contains("input + policy_limit()"));
                response["decision"]="changes_requested".into();
                response["findings"]=serde_json::json!([finding("cap-overflow","apply_limit exceeds the configured policy cap")]);
            } else {
                assert!(source.contains("input.min(policy_limit())"));
                response["decision"]="approve".into();
            }
        } else {
            response["missing_sources"]=serde_json::json!([{"path":"logic.rs","criterion_ids":["C1"],
                "question":"Need actual implementation to judge the assigned caller", "selector":{"kind":"whole_file"}}]);
        }
        Ok((response.to_string(),"native-fixture-second".into()))
    }
}

#[tokio::test]
async fn malformed_first_observation_reuses_captured_candidate_in_the_same_bounded_correction() {
    for case in ["approve","defect","over-budget","escaped-over-budget"] {
        let root=tempfile::tempdir().unwrap();git_fixture(root.path(),&["init","-q"]);
        let padding=if case=="escaped-over-budget" {"\\".repeat(aligned::MAX_PROMPT_BYTES/2+4096)}
            else {"x".repeat(if case=="over-budget" {aligned::MAX_PROMPT_BYTES+4096}else{4096})};
        let implementation=if case=="defect" {"input + policy_limit()"}else{"input.min(policy_limit())"};
        let source=format!("pub fn policy_limit() -> i32 {{ 3 }}\n/*{padding}*/\npub fn apply_limit(input:i32) -> i32 {{ {implementation} }}\n");
        std::fs::write(root.path().join("logic.rs"),&source).unwrap();
        git_fixture(root.path(),&["add","logic.rs"]);
        git_fixture(root.path(),&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","captured source"]);
        let head=git_fixture(root.path(),&["rev-parse","HEAD"]);
        let identity=snapshot_source_identity(root.path(),&head,"logic.rs",source.len()).await.unwrap();
        let mut all=supplied_shards(&BTreeMap::from([("logic.rs".into(),source)]),"","",
            &BTreeMap::from([("logic.rs".into(),identity)]));
        all.insert("controller_source:\"observer.rs\"#part-0".into(),"// pinned observer fixture\nfn observe() {}\n".into());
        // Optional caller inventory cannot crowd already-captured source out of
        // the sole correction. The original assigned caller remains mandatory.
        all.insert("context:callers:part-0".into(),"m".repeat(aligned::MAX_PROMPT_BYTES));
        std::fs::write(root.path().join("logic.rs"),"uncommitted poison").unwrap();
        let binding=aligned::ReviewBinding {head:head.clone(),base:head,environment_id:"fixture".into(),
            contract_hash:"c".repeat(64),evidence_hash:"d".repeat(64)};
        let contract=BTreeMap::from([("C1".into(),"Honor the configured cap".into())]);
        let opts=codex_review_opts(root.path().to_path_buf(),"Scripted correction fixture");
        let inputs=evidence_plan::Inputs {binding:&binding,issue:"Review caller",contract:&contract,prior:&[],
            system_bytes:opts.system_prompt.len(),controller_acceptance:None};
        let original=inputs.request(BTreeMap::from([("original-scope".into(),
            format!("// original assigned evidence\n/*{}*/\nfn caller() {{}}\n","a".repeat(64*1024)))]),false,None).unwrap();
        let actor=aligned::Actor {provider:"codex".into(),backend:"native".into(),model:"fixture".into(),session_id:"declared-review-actor".into()};
        let builders=[aligned::Actor {provider:"claude".into(),backend:"native".into(),model:"fixture".into(),session_id:"fixture-builder".into()}];
        let sources=all.iter().filter(|(key,_)|key.starts_with("source:") || key.starts_with("controller_source:"))
            .map(|(key,body)|(key.clone(),body.clone())).collect();
        let transport=CorrectionCandidateContext {calls:Mutex::new(0),defect:case=="defect",sources};
        let result=execute_evidence_group(EvidenceGroupInput {root:root.path(),inputs:&inputs,
            original_request:&original,evidence_shards:&all,actor:&actor,builders:&builders,
            origin:&aligned::ReviewOrigin::RecordedBuilders,role:aligned::ReviewRole::Diff,
            provider:augmentagent_channel_core::ProviderKind::Codex,opts:&opts,group_index:0,workflow:None},&transport).await;
        assert_eq!(*transport.calls.lock().unwrap(),2);
        if matches!(case,"over-budget"|"escaped-over-budget") {
            assert!(result.is_err(),"oversized optional source cannot authorize completion");
        } else {
            let completed=result.expect("available captured source must reach the bounded correction");
            assert_eq!(completed.native_sessions.len(),2);
            assert_eq!(completed.findings.len(),usize::from(case=="defect"));
        }
    }
}
