//! Versioned, executable acceptance evidence for autoship.
//!
//! This module never launches an untrusted shell, chooses credentials, or writes
//! approvals. The orchestrator runs `Invocation` through its admitted sandbox and
//! stores `RegisteredContract`/receipts outside the builder's writable scope.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

pub const SCHEMA_VERSION: u32 = 1;
const MAX_ITEMS: usize = 128;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceContract {
    pub schema_version: u32,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<u64>,
    pub criteria: Vec<Criterion>,
    pub allowed_paths: Vec<PathScope>,
    pub verifiers: Vec<Verifier>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "number", rename_all = "snake_case")]
pub enum AcceptanceSource {
    Issue(u64),
    PullRequest(u64),
}
impl AcceptanceSource {
    pub fn from_ids(issue: Option<u64>, pull_request: Option<u64>) -> Result<Self> {
        match (issue, pull_request) {
            (Some(n), None) if n > 0 => Ok(Self::Issue(n)),
            (None, Some(n)) if n > 0 => Ok(Self::PullRequest(n)),
            _ => bail!("acceptance requires exactly one positive issue or pull request source"),
        }
    }
    pub fn validate(self) -> Result<()> {
        Self::from_ids(self.issue(), self.pull_request()).map(|_| ())
    }
    pub fn issue(self) -> Option<u64> {
        match self {
            Self::Issue(n) => Some(n),
            _ => None,
        }
    }
    pub fn pull_request(self) -> Option<u64> {
        match self {
            Self::PullRequest(n) => Some(n),
            _ => None,
        }
    }
    pub fn key(self) -> String {
        match self {
            Self::Issue(n) => format!("issue-{n}"),
            Self::PullRequest(n) => format!("pr-{n}"),
        }
    }
}
impl std::fmt::Display for AcceptanceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Issue(n) => write!(f, "issue #{n}"),
            Self::PullRequest(n) => write!(f, "PR #{n}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub id: String,
    pub requirement: String,
    pub verifier_ids: Vec<String>,
}

/// Literal paths only: no glob/shell interpretation, and no implicit `..` scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum PathScope {
    File(String),
    Directory(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verifier {
    pub id: String,
    pub command: VerifierCommand,
    /// Dedicated verifier/harness files installed before checking baseline.
    /// Register the complete harness closure, including fixture inputs.
    pub pinned_files: Vec<String>,
    /// Literal assertion text that must accompany a named failing test.
    /// A compiler error, missing test, timeout, or import error is not red.
    pub failure_witness: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerifierCommand {
    CargoTest {
        package: String,
        target: CargoTarget,
        test: String,
    },
    PythonUnittest {
        test: String,
    },
    NodeTest {
        file: String,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum CargoTarget {
    Lib,
    Bin(String),
    Integration(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: &'static str,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredContract {
    contract: AcceptanceContract,
    baseline_head: String,
    environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate_environment_id: Option<String>,
    file_hashes: BTreeMap<String, String>,
    contract_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBinding {
    pub head: String,
    pub base: String,
    /// Toolchain + runner configuration identity, not a display-only label.
    pub environment_id: String,
    pub contract_hash: String,
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub interrupted: bool,
    pub output_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    IntendedFailure,
    Passed,
    InfrastructureFailure,
    UnexpectedFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierReceipt {
    pub verifier_id: String,
    pub binding: EvidenceBinding,
    pub result: CheckResult,
    pub output_digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub diagnostic: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaselineResult {
    Reproduced,
    /// Requires a relevance investigation; never automatic issue closure.
    AlreadyPasses(Vec<String>),
    NotReproduced(Vec<String>),
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-:.".contains(&b))
}
fn relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && !value.contains('\0')
        && !Path::new(value).is_absolute()
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && value
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}
fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl PathScope {
    fn path(&self) -> &str {
        match self {
            Self::File(p) | Self::Directory(p) => p,
        }
    }
    pub fn contains(&self, path: &str) -> bool {
        relative_path(path)
            && match self {
                Self::File(p) => path == p,
                Self::Directory(p) => path
                    .strip_prefix(p)
                    .is_some_and(|suffix| suffix.starts_with('/')),
            }
    }
}

impl AcceptanceContract {
    pub fn source(&self) -> Result<AcceptanceSource> {
        AcceptanceSource::from_ids(self.issue, self.pull_request)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported acceptance schema"
        );
        ensure!(self.revision > 0, "contract requires revision");
        self.source()?;
        for (label, size) in [
            ("criteria", self.criteria.len()),
            ("scope", self.allowed_paths.len()),
            ("verifiers", self.verifiers.len()),
        ] {
            ensure!(size > 0 && size <= MAX_ITEMS, "invalid {label} count");
        }
        ensure!(
            self.allowed_paths.iter().all(|s| relative_path(s.path())),
            "invalid writable scope"
        );
        let mut verifier_ids = BTreeSet::new();
        for verifier in &self.verifiers {
            ensure!(
                identifier(&verifier.id) && verifier_ids.insert(&verifier.id),
                "invalid/duplicate verifier ID"
            );
            ensure!(
                !verifier.failure_witness.trim().is_empty()
                    && verifier.failure_witness.len() <= 2048,
                "missing or oversized failure witness"
            );
            ensure!(
                !verifier.pinned_files.is_empty() && verifier.pinned_files.len() <= MAX_ITEMS,
                "verifier requires pinned harness files"
            );
            ensure!(
                verifier.pinned_files.iter().all(|p| relative_path(p)),
                "invalid harness path"
            );
            verifier.command.invocation()?;
        }
        let mut criterion_ids = BTreeSet::new();
        let mut used = BTreeSet::new();
        for criterion in &self.criteria {
            ensure!(
                identifier(&criterion.id) && criterion_ids.insert(&criterion.id),
                "invalid/duplicate criterion ID"
            );
            ensure!(
                !criterion.requirement.trim().is_empty() && criterion.requirement.len() <= 4096,
                "missing or oversized requirement"
            );
            ensure!(
                !criterion.verifier_ids.is_empty(),
                "criterion has no objective verifier"
            );
            for id in &criterion.verifier_ids {
                ensure!(
                    verifier_ids.contains(id),
                    "criterion references unknown verifier"
                );
                used.insert(id);
            }
        }
        ensure!(used == verifier_ids, "unmapped verifier");
        Ok(())
    }
}

impl VerifierCommand {
    pub fn invocation(&self) -> Result<Invocation> {
        let (program, args) = match self {
            Self::CargoTest {
                package,
                target,
                test,
            } => {
                ensure!(
                    identifier(package) && identifier(test),
                    "invalid cargo test selector"
                );
                let mut args = vec![
                    "test".into(),
                    "--locked".into(),
                    "-p".into(),
                    package.clone(),
                ];
                match target {
                    CargoTarget::Lib => args.push("--lib".into()),
                    CargoTarget::Bin(name) | CargoTarget::Integration(name) => {
                        ensure!(identifier(name), "invalid cargo target");
                        args.push(
                            if matches!(target, CargoTarget::Bin(_)) {
                                "--bin"
                            } else {
                                "--test"
                            }
                            .into(),
                        );
                        args.push(name.clone());
                    }
                }
                args.extend([
                    test.clone(),
                    "--".into(),
                    "--exact".into(),
                    "--test-threads=1".into(),
                ]);
                ("cargo", args)
            }
            Self::PythonUnittest { test } => {
                ensure!(
                    identifier(test) && !test.contains(':') && !test.contains('-'),
                    "invalid unittest selector"
                );
                (
                    "python3",
                    vec!["-m".into(), "unittest".into(), "-v".into(), test.clone()],
                )
            }
            Self::NodeTest { file, name } => {
                ensure!(
                    relative_path(file) && !file.starts_with('-'),
                    "invalid node test path"
                );
                ensure!(
                    identifier(name),
                    "node test name must be a literal identifier"
                );
                // Name is escaped before entering Node's regular-expression option.
                let escaped = name.replace('.', "\\.");
                (
                    "node",
                    vec![
                        "--test".into(),
                        "--test-reporter=tap".into(),
                        format!("--test-name-pattern=^{escaped}$"),
                        format!("./{file}"),
                    ],
                )
            }
        };
        Ok(Invocation { program, args })
    }

    fn selected_outcome(&self, output: &str, passed: bool) -> bool {
        match self {
            Self::CargoTest { test, .. } => output.lines().any(|line| {
                line.trim() == format!("test {test} ... {}", if passed { "ok" } else { "FAILED" })
            }),
            Self::PythonUnittest { test } => {
                let Some((class, method)) = test.rsplit_once('.') else {
                    return false;
                };
                if passed {
                    output.lines().any(|line| {
                        line.trim() == format!("{method} ({class}.{method}) ... ok")
                            || line.trim() == format!("{method} ({class}) ... ok")
                    })
                } else {
                    output.lines().any(|line| {
                        line.trim() == format!("FAIL: {method} ({class}.{method})")
                            || line.trim() == format!("FAIL: {method} ({class})")
                    })
                }
            }
            Self::NodeTest { name, .. } => output.lines().any(|line| {
                let prefix = if passed { "ok " } else { "not ok " };
                line.trim()
                    .strip_prefix(prefix)
                    .and_then(|s| s.split_once(" - "))
                    .is_some_and(|(n, actual)| n.parse::<u64>().is_ok() && actual == name)
            }),
        }
    }
}

impl RegisteredContract {
    pub fn register(
        root: &Path,
        contract: AcceptanceContract,
        baseline_head: String,
        environment_id: String,
    ) -> Result<Self> {
        contract.validate()?;
        ensure!(
            valid_commit(&baseline_head),
            "baseline must be a full commit ID"
        );
        ensure!(
            !environment_id.is_empty(),
            "runner/toolchain identity missing"
        );
        let mut file_hashes = BTreeMap::new();
        for verifier in &contract.verifiers {
            for file in &verifier.pinned_files {
                file_hashes.insert(file.clone(), hash_file(root, file)?);
            }
        }
        let contract_hash = digest(&serde_json::to_vec(&(&contract, &file_hashes))?);
        Ok(Self {
            contract,
            baseline_head,
            environment_id,
            candidate_environment_id: None,
            file_hashes,
            contract_hash,
        })
    }
    /// Controller-only: bind an independently authorized environment transition
    /// before either baseline or candidate receipts are generated.
    pub fn register_transition(
        root: &Path,
        contract: AcceptanceContract,
        baseline_head: String,
        baseline_environment: String,
        candidate_environment: String,
    ) -> Result<Self> {
        ensure!(
            !candidate_environment.is_empty(),
            "candidate environment missing"
        );
        let mut registered = Self::register(root, contract, baseline_head, baseline_environment)?;
        registered.candidate_environment_id = Some(candidate_environment);
        registered.contract_hash = registered.computed_hash()?;
        Ok(registered)
    }
    fn computed_hash(&self) -> Result<String> {
        Ok(if let Some(candidate) = &self.candidate_environment_id {
            digest(&serde_json::to_vec(&(
                &self.contract,
                &self.file_hashes,
                candidate,
            ))?)
        } else {
            digest(&serde_json::to_vec(&(&self.contract, &self.file_hashes))?)
        })
    }
    pub fn contract(&self) -> &AcceptanceContract {
        &self.contract
    }
    pub fn contract_hash(&self) -> &str {
        &self.contract_hash
    }
    pub fn baseline_head(&self) -> &str {
        &self.baseline_head
    }
    pub fn candidate_environment_id(&self) -> &str {
        self.candidate_environment_id
            .as_deref()
            .unwrap_or(&self.environment_id)
    }
    pub fn binding(&self, head: &str) -> Result<EvidenceBinding> {
        ensure!(valid_commit(head), "candidate must be a full commit ID");
        Ok(EvidenceBinding {
            head: head.into(),
            base: self.baseline_head.clone(),
            environment_id: if head == self.baseline_head {
                self.environment_id.clone()
            } else {
                self.candidate_environment_id
                    .clone()
                    .unwrap_or_else(|| self.environment_id.clone())
            },
            contract_hash: self.contract_hash.clone(),
        })
    }
    /// Verify before AND after running the child. The orchestrator additionally
    /// must deny builder writes while verification executes (lease/sandbox).
    pub fn verify_integrity(&self, root: &Path) -> Result<()> {
        self.contract.validate()?;
        ensure!(
            self.computed_hash()? == self.contract_hash,
            "contract registration was modified"
        );
        for (file, expected) in &self.file_hashes {
            ensure!(
                &hash_file(root, file)? == expected,
                "registered verifier changed: {file}"
            );
        }
        Ok(())
    }
    pub fn observe(
        &self,
        root: &Path,
        verifier_id: &str,
        binding: EvidenceBinding,
        observation: &Observation,
    ) -> Result<VerifierReceipt> {
        self.verify_integrity(root)?;
        ensure!(
            binding == self.binding(&binding.head)?,
            "evidence inputs changed; re-register/re-run"
        );
        let verifier = self
            .contract
            .verifiers
            .iter()
            .find(|v| v.id == verifier_id)
            .context("unknown verifier")?;
        let output = format!("{}\n{}", observation.stdout, observation.stderr);
        let result = if observation.interrupted
            || !observation.output_complete
            || observation.exit_code.is_none()
        {
            CheckResult::InfrastructureFailure
        } else if observation.exit_code == Some(0)
            && verifier.command.selected_outcome(&output, true)
        {
            CheckResult::Passed
        } else if observation.exit_code != Some(0)
            && verifier.command.selected_outcome(&output, false)
            && output.contains(&verifier.failure_witness)
        {
            CheckResult::IntendedFailure
        } else {
            CheckResult::UnexpectedFailure
        };
        let diagnostic = if result == CheckResult::Passed {
            String::new()
        } else {
            output.chars().take(2048).collect()
        };
        Ok(VerifierReceipt {
            verifier_id: verifier_id.into(),
            binding,
            result,
            output_digest: digest(output.as_bytes()),
            diagnostic,
        })
    }
    pub fn baseline_result(&self, receipts: &[VerifierReceipt]) -> Result<BaselineResult> {
        self.validate_receipts(receipts, &self.binding(&self.baseline_head)?)?;
        let passed = receipts
            .iter()
            .filter(|r| r.result == CheckResult::Passed)
            .map(|r| r.verifier_id.clone())
            .collect::<Vec<_>>();
        let bad = receipts
            .iter()
            .filter(|r| !matches!(r.result, CheckResult::Passed | CheckResult::IntendedFailure))
            .map(|r| r.verifier_id.clone())
            .collect::<Vec<_>>();
        if !bad.is_empty() {
            Ok(BaselineResult::NotReproduced(bad))
        } else if !passed.is_empty() {
            Ok(BaselineResult::AlreadyPasses(passed))
        } else {
            Ok(BaselineResult::Reproduced)
        }
    }
    pub fn candidate_passes(
        &self,
        baseline: &[VerifierReceipt],
        candidate: &[VerifierReceipt],
        binding: &EvidenceBinding,
    ) -> Result<bool> {
        ensure!(
            binding == &self.binding(&binding.head)?,
            "candidate binding mismatch"
        );
        self.validate_receipts(candidate, binding)?;
        Ok(
            self.baseline_result(baseline)? == BaselineResult::Reproduced
                && candidate.iter().all(|r| r.result == CheckResult::Passed),
        )
    }
    fn validate_receipts(
        &self,
        receipts: &[VerifierReceipt],
        binding: &EvidenceBinding,
    ) -> Result<()> {
        ensure!(
            receipts.len() == self.contract.verifiers.len(),
            "missing verifier evidence"
        );
        let expected = self
            .contract
            .verifiers
            .iter()
            .map(|v| v.id.as_str())
            .collect::<BTreeSet<_>>();
        let actual = receipts
            .iter()
            .map(|r| r.verifier_id.as_str())
            .collect::<BTreeSet<_>>();
        ensure!(
            expected == actual && actual.len() == receipts.len(),
            "duplicate/unknown verifier evidence"
        );
        ensure!(
            receipts
                .iter()
                .all(|r| &r.binding == binding && r.output_digest.len() == 64),
            "stale or incomplete verifier evidence"
        );
        Ok(())
    }
}

fn hash_file(root: &Path, relative: &str) -> Result<String> {
    ensure!(relative_path(relative), "invalid verifier path");
    let root = root.canonicalize()?;
    let mut path = root.clone();
    for part in Path::new(relative).components() {
        path.push(part);
        ensure!(
            !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "symlink in verifier path"
        );
    }
    let metadata = std::fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_FILE_BYTES,
        "invalid/oversized verifier file"
    );
    ensure!(
        path.canonicalize()?.starts_with(&root),
        "verifier escaped worktree"
    );
    Ok(digest(&std::fs::read(path)?))
}

/// IDs come from actual git diff content, never a model-selected description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedHunk {
    pub id: String,
    pub path: String,
}
impl ChangedHunk {
    pub fn new(path: &str, diff_content: &[u8]) -> Result<Self> {
        ensure!(relative_path(path), "invalid diff path");
        let id = digest(&serde_json::to_vec(&(path, diff_content))?);
        Ok(Self {
            id,
            path: path.into(),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlignmentMapping {
    pub hunk_id: String,
    pub criterion_ids: Vec<String>,
    /// Independent review validates necessity of supporting changes; this
    /// explanation cannot exempt the hunk from criterion linkage or scope.
    pub supporting_reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlignmentReport {
    pub outside_scope: Vec<String>,
    pub unmapped_hunks: Vec<String>,
    pub uncovered_criteria: Vec<String>,
}
impl AlignmentReport {
    pub fn complete(&self) -> bool {
        self.outside_scope.is_empty()
            && self.unmapped_hunks.is_empty()
            && self.uncovered_criteria.is_empty()
    }
}
pub fn check_alignment(
    contract: &AcceptanceContract,
    hunks: &[ChangedHunk],
    mappings: &[AlignmentMapping],
) -> Result<AlignmentReport> {
    contract.validate()?;
    ensure!(!hunks.is_empty(), "empty patch is not an implementation");
    let ids = hunks.iter().map(|h| h.id.as_str()).collect::<BTreeSet<_>>();
    ensure!(ids.len() == hunks.len(), "duplicate diff hunk");
    let criteria = contract
        .criteria
        .iter()
        .map(|c| c.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut covered = BTreeSet::new();
    let mut mapped = BTreeSet::new();
    for mapping in mappings {
        ensure!(
            ids.contains(mapping.hunk_id.as_str()) && mapped.insert(mapping.hunk_id.as_str()),
            "unknown/duplicate mapped hunk"
        );
        ensure!(
            !mapping.criterion_ids.is_empty(),
            "hunk requires acceptance linkage"
        );
        for id in &mapping.criterion_ids {
            ensure!(
                criteria.contains(id.as_str()),
                "unknown alignment criterion"
            );
            covered.insert(id.as_str());
        }
    }
    Ok(AlignmentReport {
        outside_scope: hunks
            .iter()
            .filter(|h| !contract.allowed_paths.iter().any(|s| s.contains(&h.path)))
            .map(|h| h.path.clone())
            .collect(),
        unmapped_hunks: ids.difference(&mapped).map(|s| (*s).into()).collect(),
        uncovered_criteria: criteria.difference(&covered).map(|s| (*s).into()).collect(),
    })
}

/// Mined comments are proposals, never shell commands. A trusted controller
/// promotes reviewed/versioned constraints and supplies this immutable set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Constraint {
    ForbiddenPath {
        id: String,
        source: String,
        scope: PathScope,
    },
    ForbiddenAddedText {
        id: String,
        source: String,
        literal: String,
    },
}
pub fn constraint_failures(
    constraints: &[Constraint],
    changed_paths: &[String],
    added_lines: &[String],
) -> Result<Vec<String>> {
    let mut failures = Vec::new();
    let mut ids = BTreeSet::new();
    for constraint in constraints {
        let (id, source, fails) = match constraint {
            Constraint::ForbiddenPath { id, source, scope } => {
                ensure!(relative_path(scope.path()), "invalid constraint path");
                (id, source, changed_paths.iter().any(|p| scope.contains(p)))
            }
            Constraint::ForbiddenAddedText {
                id,
                source,
                literal,
            } => {
                ensure!(
                    !literal.is_empty() && literal.len() <= 2048,
                    "invalid constraint literal"
                );
                (
                    id,
                    source,
                    added_lines.iter().any(|line| line.contains(literal)),
                )
            }
        };
        ensure!(
            identifier(id) && ids.insert(id),
            "invalid/duplicate constraint ID"
        );
        ensure!(
            source.starts_with("https://github.com/") && source.len() <= 2048,
            "constraint provenance missing"
        );
        if fails {
            failures.push(id.clone());
        }
    }
    if changed_paths.iter().any(|p| !relative_path(p)) {
        bail!("invalid changed path");
    }
    Ok(failures)
}

#[cfg(test)]
#[path = "autoship_contract_tests.rs"]
mod tests;
