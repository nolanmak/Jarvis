//! Initial preparation must use protected bytes and literal committed source.
use super::*;
use crate::autoship_contract::{
    BehaviorInvocation, Criterion, OutputOracle, PathScope, Verifier, VerifierCommand,
};

struct Fixture {
    root: tempfile::TempDir,
    prepared: PreparedAcceptance,
    path: String,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        trusted_git(root.path(), &["init", "--initial-branch=main"])
            .await
            .unwrap();
        std::fs::write(root.path().join("behavior.py"), "READY = False\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "/autoship_verifiers/\n").unwrap();
        trusted_git(root.path(), &["add", "behavior.py", ".gitignore"])
            .await
            .unwrap();
        Self::commit(root.path()).await;
        let base = actual_head(root.path()).await.unwrap();
        let path = "autoship_verifiers/observe.py".to_owned();
        let contract = AcceptanceContract {
            schema_version: 1,
            revision: 2,
            issue: Some(42),
            pull_request: None,
            criteria: vec![Criterion {
                id: "ready".into(),
                requirement: "Ready becomes true".into(),
                verifier_ids: vec!["observe".into()],
            }],
            allowed_paths: vec![PathScope::File("behavior.py".into())],
            verifiers: vec![Verifier {
                id: "observe".into(),
                command: VerifierCommand::BlackBox {
                    invocation: BehaviorInvocation::PythonScript {
                        file: path.clone(),
                        args: vec![],
                    },
                    oracle: OutputOracle::Json {
                        value: serde_json::json!(true),
                    },
                },
                pinned_files: vec![path.clone()],
                failure_witness: "ready differs".into(),
            }],
        };
        let files = BTreeMap::from([(
            path.clone(),
            "import behavior\nprint(behavior.READY)\n".into(),
        )]);
        let registration = RegisteredContract::register_from_files(
            contract.clone(),
            base,
            "fixture-environment".into(),
            &files,
        )
        .unwrap();
        let prepared = PreparedAcceptance {
            proposal: ContractProposal { contract, files },
            baseline: BaselineEvidence {
                registration,
                receipts: vec![],
            },
        };
        std::fs::create_dir(root.path().join("autoship_verifiers")).unwrap();
        Self {
            root,
            prepared,
            path,
        }
    }
    async fn commit(root: &Path) {
        trusted_git(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "synthetic product",
            ],
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn candidate_source_ignores_stale_ignored_adapter_bytes() {
    let f = Fixture::new().await;
    let path = f.root.path().join(&f.path);
    std::fs::write(&path, "old revision adapter bytes\n").unwrap();
    let head = actual_head(f.root.path()).await.unwrap();
    validate_candidate_harness(f.root.path(), &f.prepared)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "old revision adapter bytes\n"
    );
    assert_eq!(actual_head(f.root.path()).await.unwrap(), head);
}
#[tokio::test]
async fn candidate_source_ignores_worker_symlink_without_following_or_replacing_it() {
    let f = Fixture::new().await;
    let target = f.root.path().join("behavior.py");
    let path = f.root.path().join(&f.path);
    std::os::unix::fs::symlink(&target, &path).unwrap();
    validate_candidate_harness(f.root.path(), &f.prepared)
        .await
        .unwrap();
    assert_eq!(std::fs::read_link(&path).unwrap(), target);
    assert!(std::fs::symlink_metadata(path)
        .unwrap()
        .file_type()
        .is_symlink());
}
#[tokio::test]
async fn candidate_source_committed_matching_adapter_is_typed_collision() {
    let f = Fixture::new().await;
    std::fs::write(
        f.root.path().join(&f.path),
        &f.prepared.proposal.files[&f.path],
    )
    .unwrap();
    trusted_git(f.root.path(), &["add", "-f", &f.path])
        .await
        .unwrap();
    Fixture::commit(f.root.path()).await;
    let error = validate_candidate_harness(f.root.path(), &f.prepared)
        .await
        .unwrap_err();
    let collision = error
        .downcast_ref::<VerifierSourceCollision>()
        .expect("committed adapter needs typed recovery");
    assert_eq!(collision.paths, vec![f.path]);
    assert_eq!(collision.head, actual_head(f.root.path()).await.unwrap());
}
#[tokio::test]
async fn candidate_source_committed_directory_is_typed_collision() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.root.path().join(&f.path)).unwrap();
    std::fs::write(
        f.root.path().join(&f.path).join("owner.txt"),
        "owner content\n",
    )
    .unwrap();
    trusted_git(f.root.path(), &["add", "-f", &f.path])
        .await
        .unwrap();
    Fixture::commit(f.root.path()).await;
    let error = validate_candidate_harness(f.root.path(), &f.prepared)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<VerifierSourceCollision>().is_some());
    assert_eq!(
        std::fs::read_to_string(f.root.path().join(&f.path).join("owner.txt")).unwrap(),
        "owner content\n"
    );
}
#[tokio::test]
async fn candidate_source_rejects_tampered_protected_map_without_worker_adapter() {
    let mut f = Fixture::new().await;
    f.prepared
        .proposal
        .files
        .insert(f.path, "unregistered substitution\n".into());
    assert!(validate_candidate_harness(f.root.path(), &f.prepared)
        .await
        .is_err());
}

#[tokio::test]
async fn candidate_source_changed_registration_keeps_old_ignored_copy_unmodified() {
    for same_path in [true, false] {
        let mut f = Fixture::new().await;
        let old_body = f.prepared.proposal.files[&f.path].clone();
        std::fs::write(f.root.path().join(&f.path), &old_body).unwrap();
        let next_path = if same_path {
            f.path.clone()
        } else {
            "autoship_verifiers/replacement.py".into()
        };
        let mut contract = f.prepared.proposal.contract.clone();
        contract.revision += 1;
        contract.verifiers[0].pinned_files = vec![next_path.clone()];
        contract.verifiers[0].command = VerifierCommand::BlackBox {
            invocation: BehaviorInvocation::PythonScript {
                file: next_path.clone(),
                args: vec![],
            },
            oracle: OutputOracle::Json {
                value: serde_json::json!(true),
            },
        };
        let files = BTreeMap::from([(
            next_path.clone(),
            "import behavior\nprint(bool(behavior.READY))\n".into(),
        )]);
        let registration = RegisteredContract::register_from_files(
            contract.clone(),
            f.prepared.baseline.registration.baseline_head().into(),
            "fixture-environment".into(),
            &files,
        )
        .unwrap();
        f.prepared = PreparedAcceptance {
            proposal: ContractProposal { contract, files },
            baseline: BaselineEvidence {
                registration,
                receipts: vec![],
            },
        };
        validate_candidate_harness(f.root.path(), &f.prepared)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(f.root.path().join(&f.path)).unwrap(),
            old_body
        );
        if !same_path {
            assert!(!f.root.path().join(next_path).exists());
        }
        assert!(trusted_git(
            f.root.path(),
            &["status", "--porcelain", "--untracked-files=all"]
        )
        .await
        .unwrap()
        .is_empty());
    }
}
#[tokio::test]
async fn candidate_source_changed_committed_adapter_is_still_typed_collision() {
    let f = Fixture::new().await;
    std::fs::write(f.root.path().join(&f.path), "untrusted changed adapter\n").unwrap();
    trusted_git(f.root.path(), &["add", "-f", &f.path])
        .await
        .unwrap();
    Fixture::commit(f.root.path()).await;
    let error = validate_candidate_harness(f.root.path(), &f.prepared)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<VerifierSourceCollision>().is_some());
    assert_eq!(
        std::fs::read_to_string(f.root.path().join(&f.path)).unwrap(),
        "untrusted changed adapter\n"
    );
}
