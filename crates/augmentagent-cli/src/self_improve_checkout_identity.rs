//! Physical continuity evidence for an already authorized workflow checkout.
//! This does not grant a lease, authorize a target, or replace exact-head review.
use super::*;
use crate::autoship_workflow::{Binding, Lease, WorkflowStore};
use augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

#[derive(Debug)]
pub(super) struct Hold {
    pub reason_code: &'static str,
    pub outcome: WorkflowOutcome,
}
impl std::fmt::Display for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workflow-isolation:{} ({:?})",
            self.reason_code, self.outcome
        )
    }
}
impl std::error::Error for Hold {}
fn hold(reason_code: &'static str) -> Hold {
    Hold {
        reason_code,
        outcome: WorkflowOutcome::ConfigurationFault,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    directory: (u64, u64),
    git_entry: (u64, u64),
    git_pointer_sha256: Option<String>,
    common_directory: (u64, u64),
}
fn fingerprint(root: &Path, common: &Path) -> Result<Fingerprint> {
    let directory = std::fs::symlink_metadata(root)?;
    let entry = root.join(".git");
    let git_entry = std::fs::symlink_metadata(&entry)?;
    let common_directory = std::fs::symlink_metadata(common)?;
    anyhow::ensure!(
        directory.is_dir()
            && !directory.file_type().is_symlink()
            && common_directory.is_dir()
            && !common_directory.file_type().is_symlink()
            && directory.uid() == unsafe { libc::geteuid() }
            && common_directory.uid() == unsafe { libc::geteuid() },
        hold("checkout_identity_changed")
    );
    let git_pointer_sha256 = if git_entry.is_file() {
        use std::io::Read;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&entry)?;
        anyhow::ensure!(
            git_entry.len() <= 16384
                && git_entry.nlink() == 1
                && file.metadata()?.ino() == git_entry.ino(),
            hold("checkout_identity_changed")
        );
        let mut bytes = Vec::new();
        file.take(16385).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 16384, hold("checkout_identity_changed"));
        Some(format!("{:x}", Sha256::digest(bytes)))
    } else {
        anyhow::ensure!(
            git_entry.is_dir() && !git_entry.file_type().is_symlink(),
            hold("checkout_identity_changed")
        );
        None
    };
    Ok(Fingerprint {
        directory: (directory.dev(), directory.ino()),
        git_entry: (git_entry.dev(), git_entry.ino()),
        git_pointer_sha256,
        common_directory: (common_directory.dev(), common_directory.ino()),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Ownership {
    version: u32,
    workflow: String,
    repository: String,
    source_branch: String,
    worktree: PathBuf,
    common: PathBuf,
    initial_head: String,
    initial_base: String,
    fingerprint: Fingerprint,
}
#[derive(Clone)]
pub(super) struct Identity {
    receipt: PathBuf,
    ownership: Ownership,
}

fn read_receipt(path: &Path) -> Result<Option<Ownership>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 65536,
        hold("checkout_receipt_invalid")
    );
    Ok(Some(serde_json::from_reader(file)?))
}
async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut options = vec!["-c", "core.hooksPath=/dev/null"];
    options.extend_from_slice(args);
    let (ok, output, _) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run("git", &options, root),
    )
    .await
    .context("checkout identity lookup timed out")??;
    anyhow::ensure!(ok, hold("checkout_identity_unavailable"));
    Ok(output.trim().to_owned())
}
impl Identity {
    /// Observe existing evidence before workflow registration or claim changes.
    /// A first enrollment is permitted only later, under the current lease.
    pub(super) async fn observe(
        state: &Path,
        repo: &Path,
        worktree: &Path,
        workflow: &str,
        repository: &str,
        branch: &str,
    ) -> Result<Self> {
        let root = worktree.canonicalize()?;
        anyhow::ensure!(
            !std::fs::symlink_metadata(worktree)?
                .file_type()
                .is_symlink(),
            hold("checkout_identity_changed")
        );
        let top = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"]).await?)
            .canonicalize()?;
        let common = PathBuf::from(
            git(
                worktree,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?,
        )
        .canonicalize()?;
        let expected = PathBuf::from(
            git(
                repo,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?,
        )
        .canonicalize()?;
        anyhow::ensure!(
            top == root && common == expected,
            hold("checkout_repository_changed")
        );
        // Existing producers constrain candidate paths/branches and registration.
        // The receipt adds continuity, including when a byte-identical worktree
        // replaces that path. Separate stores never manufacture shared authority.
        let state = state.canonicalize()?;
        let key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(&state, workflow, &root))?)
        );
        let receipt = state
            .parent()
            .context("checkout control directory unavailable")?
            .join("workflow-checkout-identities")
            .join(format!("{key}.json"));
        let current = fingerprint(worktree, &common)?;
        let ownership = match read_receipt(&receipt)? {
            Some(recorded) => {
                anyhow::ensure!(
                    recorded.version == 1
                        && recorded.workflow == workflow
                        && recorded.repository.eq_ignore_ascii_case(repository)
                        && recorded.source_branch == branch
                        && recorded.worktree == root
                        && recorded.common == common
                        && recorded.fingerprint == current,
                    hold("checkout_identity_changed")
                );
                recorded
            }
            None => Ownership {
                version: 1,
                workflow: workflow.into(),
                repository: repository.to_ascii_lowercase(),
                source_branch: branch.into(),
                worktree: root,
                common,
                initial_head: git(worktree, &["rev-parse", "HEAD"]).await?,
                initial_base: git(worktree, &["merge-base", "origin/main", "HEAD"]).await?,
                fingerprint: current,
            },
        };
        Ok(Self { receipt, ownership })
    }
    pub(super) fn enroll(
        &self,
        store: &mut WorkflowStore,
        lease: &Lease,
        binding: &Binding,
        now: i64,
    ) -> Result<()> {
        store.validate_lease(lease, binding, now)?;
        anyhow::ensure!(
            self.ownership.workflow == lease.workflow,
            hold("checkout_workflow_changed")
        );
        anyhow::ensure!(
            fingerprint(&self.ownership.worktree, &self.ownership.common)?
                == self.ownership.fingerprint,
            hold("checkout_identity_changed")
        );
        let parent = self
            .receipt
            .parent()
            .context("checkout receipt parent unavailable")?;
        match std::fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let meta = std::fs::symlink_metadata(parent)?;
        anyhow::ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0,
            hold("checkout_receipt_invalid")
        );
        if read_receipt(&self.receipt)?.is_none() {
            use std::io::Write;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            temporary.write_all(&serde_json::to_vec(&self.ownership)?)?;
            temporary.as_file().sync_all()?;
            match temporary.persist_noclobber(&self.receipt) {
                Ok(_) => (),
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error.error.into()),
            }
            std::fs::File::open(parent)?.sync_all()?;
        }
        self.validate()
    }
    pub(super) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            read_receipt(&self.receipt)?.as_ref() == Some(&self.ownership)
                && fingerprint(&self.ownership.worktree, &self.ownership.common)?
                    == self.ownership.fingerprint,
            hold("checkout_identity_changed")
        );
        Ok(())
    }
}

pub(super) fn validate(identity: &Arc<std::sync::Mutex<Option<Identity>>>) -> Result<()> {
    if let Some(identity) = identity
        .lock()
        .map_err(|_| anyhow::anyhow!("checkout identity lock poisoned"))?
        .as_ref()
    {
        identity.validate()?;
    }
    Ok(())
}

pub(super) async fn controller_refresh(root: &Path, head: &str) -> Result<()> {
    let (ancestor, _, _) = run(
        "git",
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "merge-base",
            "--is-ancestor",
            "HEAD",
            head,
        ],
        root,
    )
    .await?;
    anyhow::ensure!(ancestor, hold("controller_unmerged_work"));
    Ok(())
}
