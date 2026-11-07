//! Derive an exact legacy adapter removal. A plan is data, not execution or
//! cleanup authority; production application requires the owning runtime fence.
use super::*;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CommitIdentity {
    name: String,
    email: String,
    seconds: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RemovalPlan {
    version: u32,
    workflow: String,
    source: AcceptanceSource,
    worktree: PathBuf,
    common_dir: PathBuf,
    git_dir: PathBuf,
    device: u64,
    inode: u64,
    installed_contract_hash: String,
    active_contract_hash: String,
    original_base: String,
    upstream: String,
    contribution_base: String,
    original_head: String,
    original_tree: String,
    resulting_tree: String,
    removal_hashes: BTreeMap<String, String>,
    transaction: String,
    commit_message: String,
    commit_bytes: String,
    expected_commit: String,
}

// These commands only inspect Git objects or write an isolated index/tree. They
// do not invoke hooks, publish commits, change refs, or grant runtime authority.
async fn git_bytes(
    root: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: &[u8],
) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt;
    let mut command = Command::new("/usr/bin/git");
    command
        .args([
            "--no-replace-objects",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_TERMINAL_PROMPT", "0")
        // --no-replace-objects does not disable info/grafts. Ancestry must
        // come from original commit parents, including in linked worktrees.
        .env("GIT_GRAFT_FILE", "/dev/null")
        // Inspection must not refresh the worker's real index stat cache.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().context("Git stdin unavailable")?;
    let stdout = child.stdout.take().context("Git stdout unavailable")?;
    let stderr = child.stderr.take().context("Git stderr unavailable")?;
    let operation = async {
        stdin.write_all(input).await?;
        drop(stdin);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdout = stdout.take(4_194_305);
        let mut stderr = stderr.take(65_537);
        let (status, _, _) = tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )?;
        ensure!(
            out.len() <= 4_194_304 && err.len() <= 65_536,
            "legacy plan Git output limit exceeded"
        );
        ensure!(
            status.success(),
            "legacy plan Git operation failed: {}",
            String::from_utf8_lossy(&err)
        );
        Ok(out)
    };
    tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .context("legacy plan Git timed out")?
}
async fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(root, None, args, &[]).await?)?
        .trim()
        .into())
}
fn commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn built_in_test(path: &str) -> bool {
    matches!(
        path,
        "crates/augmentagent-cli/tests/autoship_commands_cli.rs"
            | "crates/augmentagent-cli/tests/autoship_resolution_cli.rs"
            | "crates/augmentagent-cli/tests/autoship_submit_cli.rs"
    )
}
async fn prepare(
    root: &Path,
    control: &Path,
    installed: &PreparedAcceptance,
    active: &PreparedAcceptance,
    workflow: &str,
    expected_head: &str,
    upstream: &str,
    identity: CommitIdentity,
) -> Result<RemovalPlan> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    installed.verify_registration()?;
    active.verify_registration()?;
    let old = &installed.baseline.registration;
    let new = &active.baseline.registration;
    ensure!(
        old.contract().source()? == new.contract().source()?
            && old.baseline_head() == new.baseline_head(),
        "legacy recovery source or original baseline changed"
    );
    ensure!(
        old.contract().revision <= new.contract().revision
            && old.contract().criteria.iter().all(|c| new
                .contract()
                .criteria
                .iter()
                .any(|n| n.id == c.id && n.requirement == c.requirement)),
        "legacy recovery changed original criteria"
    );
    ensure!(
        !workflow.is_empty() && workflow.len() <= 512 && !workflow.chars().any(char::is_control),
        "invalid workflow identity"
    );
    for value in [expected_head, upstream, old.baseline_head()] {
        ensure!(
            commit_id(value),
            "legacy recovery requires full immutable commit IDs"
        );
    }
    for value in [&identity.name, &identity.email] {
        ensure!(
            !value.is_empty()
                && value.len() <= 256
                && !value
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '<' | '>')),
            "invalid deterministic commit identity"
        );
    }
    ensure!(identity.seconds >= 0, "invalid commit timestamp");
    let root = root.canonicalize()?;
    let control = control.canonicalize()?;
    ensure!(
        !control.starts_with(&root) && !root.starts_with(&control),
        "legacy plan control overlaps product"
    );
    let control_metadata = std::fs::symlink_metadata(&control)?;
    ensure!(
        control_metadata.is_dir()
            && control_metadata.uid() == unsafe { libc::geteuid() }
            && control_metadata.mode() & 0o077 == 0,
        "legacy plan control must be private controller storage"
    );
    ensure!(
        PathBuf::from(git_text(&root, &["rev-parse", "--show-toplevel"]).await?).canonicalize()?
            == root,
        "legacy recovery requires exact worktree root"
    );
    let common_dir = PathBuf::from(
        git_text(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )
    .canonicalize()?;
    let git_dir = PathBuf::from(git_text(&root, &["rev-parse", "--absolute-git-dir"]).await?)
        .canonicalize()?;
    ensure!(
        git_text(&root, &["rev-parse", "HEAD"]).await? == expected_head,
        "legacy recovery head changed"
    );
    ensure!(
        git_bytes(
            &root,
            None,
            &["status", "--porcelain=v1", "--untracked-files=all"],
            &[]
        )
        .await?
        .is_empty(),
        "legacy recovery requires clean product and index"
    );
    let tags = git_bytes(&root, None, &["ls-files", "-v", "-z"], &[]).await?;
    ensure!(
        tags.split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .all(|r| r[0] == b'H'),
        "legacy recovery refuses hidden or unusual index entries"
    );
    for descendant in [expected_head, upstream] {
        git_bytes(
            &root,
            None,
            &[
                "merge-base",
                "--is-ancestor",
                old.baseline_head(),
                descendant,
            ],
            &[],
        )
        .await?;
    }
    let contribution_base = git_text(&root, &["merge-base", upstream, expected_head]).await?;
    let original_tree =
        git_text(&root, &["rev-parse", &format!("{expected_head}^{{tree}}")]).await?;
    let tracked = git_bytes(&root, None, &["ls-files", "-z"], &[]).await?;
    let mut removal_hashes = BTreeMap::new();
    for bytes in tracked.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let path = std::str::from_utf8(bytes)?;
        if !harness_path(path) || built_in_test(path) {
            continue;
        }
        let body = installed
            .proposal
            .files
            .get(path)
            .context("unregistered tracked legacy adapter")?;
        let mut cursor = root.clone();
        let components: Vec<_> = path.split('/').collect();
        for part in &components[..components.len() - 1] {
            cursor.push(part);
            ensure!(
                std::fs::symlink_metadata(&cursor)?.file_type().is_dir(),
                "legacy adapter parent is not a real directory"
            );
        }
        let file = root.join(path);
        let metadata = std::fs::symlink_metadata(&file)?;
        ensure!(
            metadata.file_type().is_file() && metadata.nlink() == 1,
            "legacy adapter must be a regular single-link file"
        );
        let blob = git_bytes(
            &root,
            None,
            &["show", &format!("{expected_head}:{path}")],
            &[],
        )
        .await?;
        ensure!(
            blob == body.as_bytes() && std::fs::read(&file)? == blob,
            "legacy adapter differs from registered bytes"
        );
        for base in [old.baseline_head(), upstream, &contribution_base] {
            ensure!(
                git_bytes(&root, None, &["ls-tree", "-z", base, "--", path], &[])
                    .await?
                    .is_empty(),
                "legacy adapter collides with original or upstream product"
            );
        }
        removal_hashes.insert(path.to_owned(), format!("{:x}", Sha256::digest(&blob)));
    }
    ensure!(
        !removal_hashes.is_empty(),
        "no registered tracked legacy adapters to remove"
    );
    let transaction = uuid::Uuid::new_v4().to_string();
    let private = tempfile::Builder::new()
        .prefix("legacy-plan-")
        .tempdir_in(&control)?;
    let index = private.path().join("index");
    git_bytes(&root, Some(&index), &["read-tree", expected_head], &[]).await?;
    for path in removal_hashes.keys() {
        git_bytes(
            &root,
            Some(&index),
            &["update-index", "--force-remove", "--", path],
            &[],
        )
        .await?;
    }
    let resulting_tree =
        String::from_utf8(git_bytes(&root, Some(&index), &["write-tree"], &[]).await?)?
            .trim()
            .to_owned();
    let deleted = git_bytes(
        &root,
        None,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "--diff-filter=D",
            "-r",
            "-z",
            &original_tree,
            &resulting_tree,
        ],
        &[],
    )
    .await?;
    let expected_paths: Vec<u8> = removal_hashes
        .keys()
        .flat_map(|p| p.as_bytes().iter().copied().chain(std::iter::once(0)))
        .collect();
    ensure!(
        deleted == expected_paths,
        "legacy deletion tree differs from exact registered paths"
    );
    let commit_message = format!("Remove controller-owned legacy verifier adapters\n\nAuto-ship transaction: {transaction}\n");
    let who = format!(
        "{} <{}> {} +0000",
        identity.name, identity.email, identity.seconds
    );
    let commit_bytes = format!("tree {resulting_tree}\nparent {expected_head}\nauthor {who}\ncommitter {who}\n\n{commit_message}");
    let expected_commit = String::from_utf8(
        git_bytes(
            &root,
            None,
            &["hash-object", "-t", "commit", "--stdin"],
            commit_bytes.as_bytes(),
        )
        .await?,
    )?
    .trim()
    .to_owned();
    ensure!(
        git_text(&root, &["rev-parse", "HEAD"]).await? == expected_head
            && git_bytes(
                &root,
                None,
                &["status", "--porcelain=v1", "--untracked-files=all"],
                &[]
            )
            .await?
            .is_empty(),
        "product changed during legacy planning"
    );
    let metadata = std::fs::metadata(&root)?;
    Ok(RemovalPlan {
        version: 1,
        workflow: workflow.into(),
        source: old.contract().source()?,
        worktree: root,
        common_dir,
        git_dir,
        device: metadata.dev(),
        inode: metadata.ino(),
        installed_contract_hash: old.contract_hash().into(),
        active_contract_hash: new.contract_hash().into(),
        original_base: old.baseline_head().into(),
        upstream: upstream.into(),
        contribution_base,
        original_head: expected_head.into(),
        original_tree,
        resulting_tree,
        removal_hashes,
        transaction,
        commit_message,
        commit_bytes,
        expected_commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_contract::{
        BehaviorInvocation, Criterion, OutputOracle, PathScope, Verifier, VerifierCommand,
    };
    struct Fixture {
        root: tempfile::TempDir,
        control: tempfile::TempDir,
        prepared: PreparedAcceptance,
        base: String,
        head: String,
    }
    impl Fixture {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let control = tempfile::tempdir_in("/tmp").unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(control.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            trusted_git(root.path(), &["init", "--initial-branch=main"])
                .await
                .unwrap();
            std::fs::write(
                root.path().join("behavior.py"),
                "def add(a,b): return a-b\n",
            )
            .unwrap();
            trusted_git(root.path(), &["add", "behavior.py"])
                .await
                .unwrap();
            Self::commit_at(root.path(), "synthetic original product").await;
            let base = actual_head(root.path()).await.unwrap();
            trusted_git(root.path(), &["checkout", "-b", "agent-fix/issue-42"])
                .await
                .unwrap();
            let files = BTreeMap::from([
                (
                    "autoship_verifiers/observe.py".into(),
                    "import behavior\nprint(behavior.add(2,3))\n".into(),
                ),
                (
                    "autoship_verifiers/input.json".into(),
                    "{\"expected\":5}\n".into(),
                ),
            ]);
            std::fs::create_dir(root.path().join("autoship_verifiers")).unwrap();
            for (path, body) in &files {
                std::fs::write(root.path().join(path), body).unwrap();
            }
            std::fs::write(
                root.path().join("behavior.py"),
                "def add(a,b): return a+b\n",
            )
            .unwrap();
            trusted_git(root.path(), &["add", "behavior.py", "autoship_verifiers"])
                .await
                .unwrap();
            Self::commit_at(root.path(), "synthetic fixed product and legacy adapters").await;
            let head = actual_head(root.path()).await.unwrap();
            let contract = AcceptanceContract {
                schema_version: 1,
                revision: 1,
                issue: Some(42),
                pull_request: None,
                criteria: vec![Criterion {
                    id: "sum".into(),
                    requirement: "Return the arithmetic sum".into(),
                    verifier_ids: vec!["observe".into()],
                }],
                allowed_paths: vec![PathScope::File("behavior.py".into())],
                verifiers: vec![Verifier {
                    id: "observe".into(),
                    command: VerifierCommand::BlackBox {
                        invocation: BehaviorInvocation::PythonScript {
                            file: "autoship_verifiers/observe.py".into(),
                            args: vec![],
                        },
                        oracle: OutputOracle::Json {
                            value: serde_json::json!(5),
                        },
                    },
                    pinned_files: files.keys().cloned().collect(),
                    failure_witness: "sum differs".into(),
                }],
            };
            let registration = RegisteredContract::register_from_files(
                contract.clone(),
                base.clone(),
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
            Self {
                root,
                control,
                prepared,
                base,
                head,
            }
        }
        async fn commit_at(root: &Path, message: &str) {
            trusted_git(
                root,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    message,
                ],
            )
            .await
            .unwrap();
        }
        async fn plan(&self) -> Result<RemovalPlan> {
            prepare(
                self.root.path(),
                self.control.path(),
                &self.prepared,
                &self.prepared,
                "fixture/repository/pr/7",
                &self.head,
                &self.base,
                CommitIdentity {
                    name: "Fixture".into(),
                    email: "fixture@example.invalid".into(),
                    seconds: 1_790_000_000,
                },
            )
            .await
        }
    }
    #[tokio::test]
    async fn legacy_removal_plan_cannot_adopt_grafted_unrelated_history() {
        for linked in [false,true] {
            let mut f=Fixture::new().await;
            let tree=git_text(f.root.path(),&["rev-parse","HEAD^{tree}"]).await.unwrap();
            let root_commit=format!("tree {tree}\nauthor Fixture <fixture@example.invalid> 1790000000 +0000\ncommitter Fixture <fixture@example.invalid> 1790000000 +0000\n\nUnrelated product snapshot\n");
            let unrelated=String::from_utf8(git_bytes(f.root.path(),None,
                &["hash-object","-w","-t","commit","--stdin"],root_commit.as_bytes()).await.unwrap()).unwrap().trim().to_owned();
            trusted_git(f.root.path(),&["update-ref","HEAD",&unrelated,&f.head]).await.unwrap();
            f.head=unrelated;
            assert!(f.plan().await.is_err(),"unrelated original history must refuse without grafts");
            let common=PathBuf::from(git_text(f.root.path(),&["rev-parse","--path-format=absolute","--git-common-dir"]).await.unwrap());
            let graft=common.join("info/grafts");let bytes=format!("{} {}\n",f.head,f.base).into_bytes();
            std::fs::create_dir_all(graft.parent().unwrap()).unwrap();std::fs::write(&graft,&bytes).unwrap();
            let linked_directory=tempfile::tempdir().unwrap();let checkout=linked_directory.path().join("candidate");
            if linked {
                trusted_git(f.root.path(),&["worktree","add","--detach",checkout.to_str().unwrap(),&f.head]).await.unwrap();
            }
            let candidate=if linked {checkout.as_path()} else {f.root.path()};
            // Establish that this local Git actually honors the forged graph,
            // including --no-replace-objects and a linked worktree's common dir.
            let forged=Command::new("/usr/bin/git").args(["--no-replace-objects","merge-base","--is-ancestor",&f.base,&f.head])
                .current_dir(candidate).env_clear().env("PATH","/usr/bin:/bin").output().await.unwrap();
            assert!(forged.status.success(),"fixture did not exercise repository grafts: {}",String::from_utf8_lossy(&forged.stderr));
            let result=prepare(candidate,f.control.path(),&f.prepared,&f.prepared,"fixture/repository#42",&f.head,&f.base,
                CommitIdentity {name:"Fixture".into(),email:"fixture@example.invalid".into(),seconds:1_790_000_000}).await;
            assert!(result.is_err(),"repository graft supplied false original-baseline ancestry (linked={linked})");
            assert_eq!(actual_head(candidate).await.unwrap(),f.head);
            assert_eq!(std::fs::read(&graft).unwrap(),bytes,"planning changed repository metadata");
            for (path,body) in &f.prepared.proposal.files {
                assert_eq!(std::fs::read(candidate.join(path)).unwrap(),body.as_bytes());
            }
        }
    }
    #[tokio::test]
    async fn legacy_removal_plan_preserves_product_worktree_index_and_head() {
        let f = Fixture::new().await;
        let index = trusted_git(f.root.path(), &["write-tree"]).await.unwrap();
        let plan = f
            .plan()
            .await
            .expect("registered legacy adapters need an exact recovery plan");
        assert_eq!(plan.original_head, f.head);
        assert_eq!(plan.original_base, f.base);
        assert_eq!(plan.removal_hashes.len(), 2);
        assert_ne!(plan.resulting_tree, plan.original_tree);
        assert_eq!(actual_head(f.root.path()).await.unwrap(), f.head);
        assert_eq!(
            trusted_git(f.root.path(), &["write-tree"]).await.unwrap(),
            index
        );
        assert!(trusted_git(
            f.root.path(),
            &["status", "--porcelain", "--untracked-files=all"]
        )
        .await
        .unwrap()
        .is_empty());
        for (path, body) in &f.prepared.proposal.files {
            assert_eq!(
                std::fs::read_to_string(f.root.path().join(path)).unwrap(),
                *body
            );
        }
        assert_eq!(
            std::fs::read_to_string(f.root.path().join("behavior.py")).unwrap(),
            "def add(a,b): return a+b\n"
        );
        assert_eq!(
            trusted_git(
                f.root.path(),
                &["show", &format!("{}:behavior.py", plan.resulting_tree)]
            )
            .await
            .unwrap(),
            "def add(a,b): return a+b"
        );
        assert!(!plan.expected_commit.is_empty());
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_dirty_staged_and_untracked_work() {
        for mode in ["dirty", "staged", "untracked"] {
            let f = Fixture::new().await;
            let path = if mode == "untracked" {
                "owner-notes.txt"
            } else {
                "behavior.py"
            };
            std::fs::write(f.root.path().join(path), "owner work\n").unwrap();
            if mode == "staged" {
                trusted_git(f.root.path(), &["add", path]).await.unwrap();
            }
            let before = trusted_git(
                f.root.path(),
                &["status", "--porcelain", "--untracked-files=all"],
            )
            .await
            .unwrap();
            assert!(f
                .plan()
                .await
                .unwrap_err()
                .to_string()
                .contains("clean product"));
            assert_eq!(
                trusted_git(
                    f.root.path(),
                    &["status", "--porcelain", "--untracked-files=all"]
                )
                .await
                .unwrap(),
                before
            );
            assert_eq!(
                std::fs::read_to_string(f.root.path().join(path)).unwrap(),
                "owner work\n"
            );
        }
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_changed_registered_bytes() {
        let mut f = Fixture::new().await;
        std::fs::write(f.root.path().join("autoship_verifiers/input.json"), "{}\n").unwrap();
        trusted_git(f.root.path(), &["add", "autoship_verifiers/input.json"])
            .await
            .unwrap();
        Fixture::commit_at(f.root.path(), "changed adapter").await;
        f.head = actual_head(f.root.path()).await.unwrap();
        assert!(f
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("registered bytes"));
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_unknown_tracked_helpers() {
        let mut f = Fixture::new().await;
        std::fs::write(
            f.root.path().join("autoship_verifiers/foreign.py"),
            "owner content\n",
        )
        .unwrap();
        trusted_git(f.root.path(), &["add", "autoship_verifiers/foreign.py"])
            .await
            .unwrap();
        Fixture::commit_at(f.root.path(), "unregistered helper").await;
        f.head = actual_head(f.root.path()).await.unwrap();
        assert!(f
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("unregistered tracked"));
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_hardlinks_and_hidden_index_bits() {
        let f = Fixture::new().await;
        let link = f.root.path().join(".git/linked-adapter");
        std::fs::hard_link(f.root.path().join("autoship_verifiers/input.json"), &link).unwrap();
        assert!(f
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("single-link"));
        std::fs::remove_file(link).unwrap();
        trusted_git(
            f.root.path(),
            &["update-index", "--assume-unchanged", "behavior.py"],
        )
        .await
        .unwrap();
        assert!(f
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("hidden or unusual"));
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_upstream_collision_and_wrong_head() {
        let f = Fixture::new().await;
        let identity = CommitIdentity {
            name: "Fixture".into(),
            email: "fixture@example.invalid".into(),
            seconds: 1_790_000_000,
        };
        let collision = prepare(
            f.root.path(),
            f.control.path(),
            &f.prepared,
            &f.prepared,
            "fixture/repository/pr/7",
            &f.head,
            &f.head,
            identity.clone(),
        )
        .await
        .unwrap_err();
        assert!(collision.to_string().contains("collides"));
        let changed = prepare(
            f.root.path(),
            f.control.path(),
            &f.prepared,
            &f.prepared,
            "fixture/repository/pr/7",
            &f.base,
            &f.base,
            identity,
        )
        .await
        .unwrap_err();
        assert!(changed.to_string().contains("head changed"));
    }
    #[tokio::test]
    async fn legacy_removal_plan_does_not_publish_expected_commit() {
        let f = Fixture::new().await;
        let plan = f.plan().await.unwrap();
        assert!(git_bytes(
            f.root.path(),
            None,
            &["cat-file", "-e", &plan.expected_commit],
            &[]
        )
        .await
        .is_err());
        let hash = git_bytes(
            f.root.path(),
            None,
            &["hash-object", "-w", "-t", "commit", "--stdin"],
            plan.commit_bytes.as_bytes(),
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(hash).unwrap().trim(),
            plan.expected_commit
        );
        assert_eq!(
            git_text(
                f.root.path(),
                &["rev-parse", &format!("{}^", plan.expected_commit)]
            )
            .await
            .unwrap(),
            f.head
        );
        assert_eq!(
            git_text(
                f.root.path(),
                &["rev-parse", &format!("{}^{{tree}}", plan.expected_commit)]
            )
            .await
            .unwrap(),
            plan.resulting_tree
        );
        // Independently ask Git's commit constructor to derive the same object;
        // writing that object in this fixture still leaves every ref unchanged.
        use tokio::io::AsyncWriteExt;
        let mut child = Command::new("/usr/bin/git")
            .args(["commit-tree", &plan.resulting_tree, "-p", &f.head])
            .current_dir(f.root.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", "1790000000 +0000")
            .env("GIT_COMMITTER_DATE", "1790000000 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(plan.commit_message.as_bytes())
            .await
            .unwrap();
        drop(input);
        let output = child.wait_with_output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            plan.expected_commit
        );
        assert_eq!(actual_head(f.root.path()).await.unwrap(), f.head);
    }

    #[tokio::test]
    async fn legacy_removal_plan_refuses_symlink_adapter() {
        let mut f = Fixture::new().await;
        let path = f.root.path().join("autoship_verifiers/input.json");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("../behavior.py", &path).unwrap();
        trusted_git(f.root.path(), &["add", "autoship_verifiers/input.json"])
            .await
            .unwrap();
        Fixture::commit_at(f.root.path(), "symlink adapter").await;
        f.head = actual_head(f.root.path()).await.unwrap();
        assert!(f
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("regular single-link"));
        assert!(std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink());
    }
    #[tokio::test]
    async fn legacy_removal_plan_preserves_builtin_tests_and_ignored_copies() {
        let mut f = Fixture::new().await;
        let builtin = "crates/augmentagent-cli/tests/autoship_commands_cli.rs";
        std::fs::create_dir_all(f.root.path().join("crates/augmentagent-cli/tests")).unwrap();
        std::fs::write(f.root.path().join(builtin), "// real product test\n").unwrap();
        std::fs::write(
            f.root.path().join(".gitignore"),
            "autoship_verifiers/ignored.py\n",
        )
        .unwrap();
        trusted_git(f.root.path(), &["add", builtin, ".gitignore"])
            .await
            .unwrap();
        Fixture::commit_at(f.root.path(), "real test and ignore policy").await;
        std::fs::write(
            f.root.path().join("autoship_verifiers/ignored.py"),
            "ignored owner content\n",
        )
        .unwrap();
        f.head = actual_head(f.root.path()).await.unwrap();
        let plan = f.plan().await.unwrap();
        assert_eq!(plan.removal_hashes.len(), 2);
        assert_eq!(
            git_text(
                f.root.path(),
                &["show", &format!("{}:{builtin}", plan.resulting_tree)]
            )
            .await
            .unwrap(),
            "// real product test"
        );
        assert_eq!(
            std::fs::read_to_string(f.root.path().join("autoship_verifiers/ignored.py")).unwrap(),
            "ignored owner content\n"
        );
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_product_control_and_registration_tampering() {
        let f = Fixture::new().await;
        let identity = CommitIdentity {
            name: "Fixture".into(),
            email: "fixture@example.invalid".into(),
            seconds: 1_790_000_000,
        };
        let error = prepare(
            f.root.path(),
            f.root.path(),
            &f.prepared,
            &f.prepared,
            "fixture/repository/pr/7",
            &f.head,
            &f.base,
            identity,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("control overlaps"));
        let mut changed = f.prepared.clone();
        changed.proposal.files.insert(
            "autoship_verifiers/input.json".into(),
            "unregistered bytes".into(),
        );
        assert!(prepare(
            f.root.path(),
            f.control.path(),
            &changed,
            &f.prepared,
            "fixture/repository/pr/7",
            &f.head,
            &f.base,
            CommitIdentity {
                name: "Fixture".into(),
                email: "fixture@example.invalid".into(),
                seconds: 1_790_000_000
            }
        )
        .await
        .is_err());
    }
    #[tokio::test]
    async fn legacy_removal_plan_refuses_shared_control_storage() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new().await;
        std::fs::set_permissions(f.control.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(f
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("private controller storage"));
    }
    #[tokio::test]
    async fn legacy_removal_plan_preserves_real_index_bytes_after_stat_change() {
        let f = Fixture::new().await;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(f.root.path().join("behavior.py"))
            .unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        )
        .unwrap();
        drop(file);
        let index_path = f.root.path().join(".git/index");
        let index_before = std::fs::read(&index_path).unwrap();
        f.plan().await.unwrap();
        assert_eq!(
            std::fs::read(index_path).unwrap(),
            index_before,
            "planning refreshed the real index"
        );
    }
}
