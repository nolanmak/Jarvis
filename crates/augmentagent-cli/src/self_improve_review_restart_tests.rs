//! Crashes exercise the production cache boundary, not a hand-built receipt.
use super::*;
use crate::autoship_workflow::WorkflowStore;

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
struct Scripted {
    root: PathBuf,
    crash: bool,
}
#[async_trait::async_trait]
impl GroupTransport for Scripted {
    async fn observe_group(
        &self,
        _: augmentagent_channel_core::ProviderKind,
        opts: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> Result<(String, String)> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("calls"))?;
        writeln!(file, "observed")?;
        file.sync_all()?;
        let request: serde_json::Value = serde_json::from_str(prompt)?;
        let binding = request["binding"].clone();
        let source = std::fs::read_to_string(self.root.join("repo/code.rs"))?;
        anyhow::ensure!(
            source.contains("panic!"),
            "the seeded defect must remain present"
        );
        let response=serde_json::json!({"binding":binding,"decision":"changes_requested","findings":[{
            "id":"seed","criterion_id":"C1","evidence_hash":aligned::digest(source.as_bytes()),"claim":"value panics instead of returning one",
            "requested_change":"return one without panicking","conflicts_with":[],"repair_rounds":0,
            "repair_target":{"kind":"candidate","paths":["code.rs"]}}]}).to_string();
        let mut response: serde_json::Value = serde_json::from_str(&response)?;
        if !opts.system_prompt.contains("Diff") {
            response["decision"] = "approve".into();
            response["findings"] = serde_json::json!([]);
        }
        let response = response.to_string();
        std::fs::write(self.root.join("orphan-provider-response"), &response)?;
        if self.crash {
            std::process::exit(77);
        }
        let calls = std::fs::read_to_string(self.root.join("calls"))?
            .lines()
            .count();
        Ok((response, format!("fresh-scripted-review-{calls}")))
    }
}
async fn exercise(before_cache: bool, name: &str) {
    if let Some(root) = std::env::var_os("REVIEW_CRASH_ROOT").map(PathBuf::from) {
        let repo = root.join("repo");
        let state = root.join("workflow.db");
        let at: i64 = std::env::var("REVIEW_CRASH_CLOCK")
            .unwrap()
            .parse()
            .unwrap();
        let guard = workflow::WorkflowGuard::acquire_fixture_at(
            &state,
            workflow::Start {
                repo_root: &repo,
                worktree: &repo,
                repository: "owner/repo",
                source_issue: Some(1709),
                pr: None,
                branch: "main",
                contract: "value returns one",
                environment: "fixture",
            },
            at,
        )
        .await
        .unwrap()
        .unwrap();
        let snapshot = super::super::candidate_snapshot(&repo).await.unwrap();
        let binding = aligned::ReviewBinding {
            head: snapshot.head,
            base: snapshot.base,
            environment_id: "fixture".into(),
            contract_hash: "contract".into(),
            evidence_hash: aligned::digest(snapshot.diff.as_bytes()),
        };
        let contract = BTreeMap::from([("C1".into(), "value returns one".into())]);
        let inputs = evidence_plan::Inputs {
            binding: &binding,
            issue: "value returns one",
            contract: &contract,
            prior: &[],
            system_bytes: 0,
            controller_acceptance: None,
        };
        let shards = BTreeMap::from([(
            "code.rs".into(),
            std::fs::read_to_string(repo.join("code.rs")).unwrap(),
        )]);
        let request = inputs.pack(&shards).unwrap().remove(0);
        let actor = aligned::Actor {
            provider: "codex".into(),
            backend: "native".into(),
            model: "fixture".into(),
            session_id: "planned-isolated".into(),
        };
        let builder = aligned::Actor {
            provider: "claude".into(),
            backend: "native".into(),
            model: "fixture".into(),
            session_id: "actual-writer".into(),
        };
        let opts = augmentagent_channel_core::ReasonerOpts::pinned(
            augmentagent_channel_core::providers::ModelTier::Quality,
            "fixture",
        );
        let origin = aligned::ReviewOrigin::RecordedBuilders;
        let phase = std::env::var("REVIEW_CRASH_PHASE").unwrap();
        let transport = Scripted {
            root: root.clone(),
            crash: before_cache && phase == "crash",
        };
        let mut passes = Vec::new();
        for role in [
            aligned::ReviewRole::Qa,
            aligned::ReviewRole::Diff,
            aligned::ReviewRole::System,
        ] {
            let mut opts = opts.clone();
            opts.system_prompt = evidence_plan::system_prompt(role, "fixture");
            let completed = execute_cached_evidence_group(
                EvidenceGroupInput {
                    root: &repo,
                    inputs: &inputs,
                    original_request: &request,
                    evidence_shards: &shards,
                    actor: &actor,
                    builders: std::slice::from_ref(&builder),
                    origin: &origin,
                    role,
                    provider: augmentagent_channel_core::ProviderKind::Codex,
                    opts: &opts,
                    group_index: 0,
                    workflow: Some(&guard),
                },
                &transport,
                Some(&guard),
            )
            .await
            .unwrap();
            assert_eq!(
                completed.findings.len(),
                usize::from(role == aligned::ReviewRole::Diff),
                "replay lost the seeded defect"
            );
            let mut actual = actor.clone();
            actual.session_id = completed.native_sessions[0].clone();
            let mut coverage = request.coverage.clone();
            coverage.prompt_hash =
                aligned::digest(&serde_json::to_vec(&completed.request_hashes).unwrap());
            passes.push(aligned::ReviewPass {
                binding: binding.clone(),
                actor: actual,
                role,
                completed: true,
                coverage,
                additional_sessions: completed.native_sessions[1..].to_vec(),
                findings: completed.findings,
            });
        }
        if phase == "crash" {
            std::process::exit(77);
        }
        assert!(
            aligned::review_authority(&binding, &[builder.clone()], &passes, None).is_err(),
            "seeded defect merged after restart"
        );
        let pending = aligned::MandatoryChecks {
            binding: binding.clone(),
            authorized: true,
            required_ci_green: false,
            verifier_integrity: true,
            scope_valid: true,
            mergeable: true,
        };
        let mut clean = passes.clone();
        for pass in &mut clean {
            pass.findings.clear();
        }
        assert!(
            aligned::merge_authority(
                &binding,
                std::slice::from_ref(&builder),
                &clean,
                &pending,
                None
            )
            .is_err(),
            "restart waived a mandatory CI check"
        );
        let evidence = aligned::ReviewEvidence {
            binding: binding.clone(),
            origin,
            builders: vec![builder],
            passes,
            adjudication: None,
            supplemental_source_hashes: BTreeMap::new(),
        };
        guard
            .record_review(
                "crash-fixture",
                false,
                &serde_json::to_string(&evidence).unwrap(),
            )
            .unwrap();
        guard.finish().unwrap();
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (k, v) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("core.hooksPath", "/dev/null"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", k, v]);
    }
    std::fs::write(repo.join("code.rs"), "pub fn value()->u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "baseline"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    std::fs::write(
        repo.join("code.rs"),
        "pub fn value()->u8 { panic!(\"seeded defect\") }\n",
    )
    .unwrap();
    git(&repo, &["commit", "-am", "candidate"]);
    let start = chrono::Utc::now().timestamp_millis();
    for (index, (phase, clock)) in [
        ("crash", start),
        ("resume", start + 601_000),
        ("resume", start + 1_202_000),
        ("resume", start + 1_803_000),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 3 {
            assert_eq!(
                std::fs::read_to_string(root.join("calls"))
                    .unwrap()
                    .lines()
                    .count(),
                if before_cache { 4 } else { 3 },
                "same binding did not reuse all three validated roles"
            );
            std::fs::write(repo.join("code.rs"),"// new head still contains the seeded defect\npub fn value()->u8 { panic!(\"seeded defect\") }\n").unwrap();
            git(&repo, &["commit", "-am", "new candidate head"]);
        }
        let out = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("self_improve::aligned_review::group_execution::restart_tests::{name}"),
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
            .env("REVIEW_CRASH_ROOT", &root)
            .env("REVIEW_CRASH_CLOCK", clock.to_string())
            .env("REVIEW_CRASH_PHASE", phase)
            .output()
            .await
            .unwrap();
        if phase == "crash" {
            assert_eq!(out.status.code(), Some(77));
            assert!(
                WorkflowStore::open(&root.join("workflow.db"))
                    .unwrap()
                    .current_reviews("owner/repo#1709")
                    .unwrap()
                    .is_empty(),
                "crash recorded a final approval"
            );
        } else {
            assert!(
                out.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(root.join("calls"))
            .unwrap()
            .lines()
            .count(),
        if before_cache { 7 } else { 6 },
        "new head borrowed an old review or restart failed to reuse all validated roles"
    );
}
#[tokio::test]
async fn crash_after_validated_review_reuses_artifact_before_recording() {
    exercise(
        false,
        "crash_after_validated_review_reuses_artifact_before_recording",
    )
    .await;
}
#[tokio::test]
async fn crash_before_review_fragment_commit_reruns_safely() {
    exercise(true, "crash_before_review_fragment_commit_reruns_safely").await;
}
