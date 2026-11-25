//! #2464: scope writes a plan, the build carries the proof.
//!
//! With `AUTOSHIP_SCOPE_PROOF_GATE` off, one scope call yields a [`Plan`]:
//! the implementation guidance, the acceptance criteria as text and the
//! paths the scoper expects to touch. Nothing is measured on the original
//! revision. The builder writes or updates tests with the change and
//! records, in a `TEST-PROOF` block, that each one failed before its
//! production change and passes after it. The controller checks that record
//! against the diff ([`TestProof::check`]) and records the result. Hosted CI
//! on the head and the independent cross-provider review are the merge
//! gate; a pure refactor or doc change needs only those two.
use super::*;
use sha2::{Digest, Sha256};

/// What one scope call produced, as text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Plan {
    pub body: String,
    pub criteria: Vec<String>,
    /// Paths the scoper named in an `ALLOWED-PATHS:` line. Guidance for the
    /// builder and the reviewer; the write admission is the tracked tree
    /// (controller and verifier paths stay excluded by the admission itself,
    /// and the blast-radius guard still runs on the diff).
    pub declared_paths: Vec<String>,
}

/// The scope prompt addition in plan mode.
pub(crate) const SCOPE_PLAN_POLICY: &str = "\nPlan mode (#2464): do not design an executable acceptance contract and do not try to reproduce anything on the original revision. Write a short implementation plan and acceptance criteria as text. Add one header line `ALLOWED-PATHS: <comma-separated repository paths the change should touch>`. The builder will write the tests with the change.\n";

/// What the builder must do in plan mode.
pub(crate) const BUILD_PROOF_POLICY: &str = "\nTests are the proof (#2464). Write or update tests with this change. Run each new or changed test before your production change (it must fail) and again after it (it must pass). End your reply with this block, one entry per test:\nTEST-PROOF\ntest: <exact test name>\nred: <one line of the failing output before the change>\ngreen: <one line of the passing output after the change>\nEND-TEST-PROOF\nA pure refactor or documentation change with no behavior to test writes `TEST-PROOF: none (refactor)` or `TEST-PROOF: none (docs)` instead.\n";

impl Plan {
    pub(crate) fn from_scope(owner_request: &str, scope: Option<&ScopeOutcome>, criteria: &[String]) -> Self {
        let body = scope.filter(|scope| scope.fixable).map(|scope| scope.body.trim()).filter(|body| !body.is_empty());
        let body = match body {
            Some(spec) if spec != owner_request.trim() => format!("Original owner request (preserve every requirement):\n{owner_request}\n\nPlan:\n{spec}"),
            _ => owner_request.to_owned(),
        };
        let declared_paths = declared_paths(&body);
        Plan { body, criteria: criteria.to_vec(), declared_paths }
    }

    /// The binding contract a plan owns: `plan:<sha256>`.
    pub(crate) fn contract(&self) -> Result<String> {
        let digest = Sha256::digest(serde_json::to_vec(&("autoship-plan-v1", self))?);
        Ok(format!("{}{digest:x}", crate::autoship_workflow::scope_proof_gate::PLAN_CONTRACT_PREFIX))
    }

    /// The builder's write admission: every tracked top-level entry.
    pub(crate) fn write_admission(&self, worktree: &Path) -> Result<Vec<augmentagent_channel_core::autoship_write_scope::DeclaredPath>> {
        let output = std::process::Command::new("git")
            .args(["ls-tree", "-z", "HEAD"])
            .current_dir(worktree)
            .output()
            .context("list tracked tree for plan write admission")?;
        anyhow::ensure!(output.status.success(), "cannot list tracked tree for plan write admission");
        let raw = String::from_utf8(output.stdout).context("tracked tree is not UTF-8")?;
        Ok(tree_admission(&raw))
    }
}

fn tree_admission(raw: &str) -> Vec<augmentagent_channel_core::autoship_write_scope::DeclaredPath> {
    let mut paths: Vec<_> = raw
        .split('\0')
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            let kind = meta.split_whitespace().nth(1)?;
            let directory = match kind {
                "tree" => true,
                "blob" => false,
                _ => return None,
            };
            let literal = !path.is_empty() && path != "." && path != ".." && !path.contains('/');
            literal.then(|| augmentagent_channel_core::autoship_write_scope::DeclaredPath { path: path.to_owned(), directory })
        })
        .collect();
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    paths.truncate(256);
    paths
}

/// Paths from `ALLOWED-PATHS:` lines, deduplicated, literal relative only.
pub(crate) fn declared_paths(text: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches(['-', '*', ' ']);
        let Some(rest) = trimmed.get(..14).filter(|head| head.eq_ignore_ascii_case("allowed-paths:")).map(|_| &trimmed[14..]) else { continue };
        for path in rest.split([',', ' ']).map(|p| p.trim().trim_matches('`')).filter(|p| !p.is_empty()) {
            let literal = !path.starts_with('/') && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..");
            if literal && !paths.iter().any(|seen| seen == path) {
                paths.push(path.to_owned());
            }
        }
    }
    paths
}

/// Pinned harness files the QA reviewer is told not to judge; none in plan mode.
pub(crate) fn harness_files(prepared: Option<&acceptance::PreparedAcceptance>) -> Vec<&str> {
    prepared.map(|prepared| prepared.proposal.files.keys().map(String::as_str).collect()).unwrap_or_default()
}

/// What the controller concluded from the builder's proof record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProofOutcome {
    /// Every listed test is new or changed in the diff, failed on base and
    /// passes on the head.
    FailedOnBasePassedOnHead { tests: Vec<String> },
    /// The builder declared a refactor or doc change: CI and review suffice.
    NotApplicable { reason: String },
    /// Not shown. Recorded as a note on the PR; CI and review still gate.
    NotShown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TestProof {
    pub head: String,
    pub outcome: ProofOutcome,
}

#[derive(Debug, Default)]
struct Entry {
    test: String,
    red: String,
    green: String,
}

fn parse_entries(summary: &str) -> Option<Vec<Entry>> {
    let start = summary.rfind("\nTEST-PROOF\n").map(|at| at + 1).or_else(|| summary.starts_with("TEST-PROOF\n").then_some(0))?;
    let block = &summary[start + "TEST-PROOF\n".len()..];
    let block = &block[..block.find("END-TEST-PROOF").unwrap_or(block.len())];
    let mut entries: Vec<Entry> = Vec::new();
    for line in block.lines().map(str::trim) {
        if let Some(test) = line.strip_prefix("test:") {
            entries.push(Entry { test: test.trim().to_owned(), ..Entry::default() });
        } else if let Some(red) = line.strip_prefix("red:") {
            if let Some(entry) = entries.last_mut() { entry.red = red.trim().to_owned(); }
        } else if let Some(green) = line.strip_prefix("green:") {
            if let Some(entry) = entries.last_mut() { entry.green = green.trim().to_owned(); }
        }
    }
    Some(entries)
}

fn red(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ["failed", "panicked", "assertionerror", "not ok", "error[", "fail:", "fail "].iter().any(|m| l.contains(m))
}

fn green(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    !red(line) && [" ok", "passed", "pass", "ok "].iter().any(|m| l.contains(m))
}

/// Base name a diff line may mention: `mod::tests::name` → `name`.
fn short_name(test: &str) -> &str {
    test.rsplit("::").next().unwrap_or(test).rsplit('.').next().unwrap_or(test)
}

impl TestProof {
    /// Check the builder's record against the change it describes. `diff`
    /// is the unified diff base..head. A listed test counts only if its name
    /// appears on an added line (it is new or changed), its red line shows a
    /// failure and its green line a pass.
    pub(crate) fn check(summary: &str, diff: &str, head: &str) -> Self {
        let outcome = Self::outcome(summary, diff);
        TestProof { head: head.to_owned(), outcome }
    }

    fn outcome(summary: &str, diff: &str) -> ProofOutcome {
        if let Some(line) = summary.lines().rev().map(str::trim).find(|line| line.starts_with("TEST-PROOF: none")) {
            let reason = line.trim_start_matches("TEST-PROOF: none").trim().trim_matches(['(', ')']).to_owned();
            return if only_docs(diff) || reason.eq_ignore_ascii_case("refactor") {
                ProofOutcome::NotApplicable { reason: if reason.is_empty() { "declared".into() } else { reason } }
            } else {
                ProofOutcome::NotShown { reason: format!("builder declared `{reason}` but the diff changes code") }
            };
        }
        let Some(entries) = parse_entries(summary) else {
            return if only_docs(diff) {
                ProofOutcome::NotApplicable { reason: "docs".into() }
            } else {
                ProofOutcome::NotShown { reason: "the builder recorded no TEST-PROOF block".into() }
            };
        };
        if entries.is_empty() {
            return ProofOutcome::NotShown { reason: "the TEST-PROOF block lists no tests".into() };
        }
        let added: Vec<&str> = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).collect();
        let mut tests = Vec::new();
        for entry in entries {
            let name = short_name(&entry.test);
            if name.is_empty() || !added.iter().any(|line| line.contains(name)) {
                return ProofOutcome::NotShown { reason: format!("`{}` is not a new or changed test in the diff", entry.test) };
            }
            if !red(&entry.red) {
                return ProofOutcome::NotShown { reason: format!("`{}` has no failing run on base", entry.test) };
            }
            if !green(&entry.green) {
                return ProofOutcome::NotShown { reason: format!("`{}` has no passing run on the head", entry.test) };
            }
            tests.push(entry.test);
        }
        ProofOutcome::FailedOnBasePassedOnHead { tests }
    }

    pub(crate) fn proven(&self) -> bool {
        matches!(self.outcome, ProofOutcome::FailedOnBasePassedOnHead { .. })
    }

    /// Record the result in the workflow's controller audit log.
    pub(crate) fn record(&self, workflow: &workflow::WorkflowGuard) -> Result<()> {
        workflow.audit_logger().record_controller(serde_json::json!({"kind":"plan_test_proof","head":self.head,
            "proof":self.outcome,"proven":self.proven(),"shipping_authority":false}))
    }

    /// The non-blocking note for the reviewer and the PR.
    pub(crate) fn note(&self) -> String {
        match &self.outcome {
            ProofOutcome::FailedOnBasePassedOnHead { tests } => format!(
                "Test proof (#2464): failed on base and pass on `{}`: {}.",
                truncate(&self.head, 12), tests.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ")),
            ProofOutcome::NotApplicable { reason } => format!("Test proof (#2464): not applicable ({reason}); hosted CI and the independent review gate this change."),
            ProofOutcome::NotShown { reason } => format!("Test proof (#2464): not shown ({reason}). Hosted CI and the independent review gate this change; reviewers should weigh the missing proof."),
        }
    }
}

fn only_docs(diff: &str) -> bool {
    let files: Vec<&str> = diff.lines().filter_map(|l| l.strip_prefix("+++ b/")).collect();
    !files.is_empty() && files.iter().all(|f| f.ends_with(".md") || f.starts_with("docs/") || f.ends_with(".txt"))
}

/// Measure and record the build's proof for the current head. Never fails
/// the build: errors reading the diff become a "not shown" proof.
pub(crate) async fn record_build_proof(worktree: &Path, base: &str, summary: &str, workflow: &workflow::WorkflowGuard) -> Result<TestProof> {
    let head = run("git", &["rev-parse", "HEAD"], worktree).await.ok().map(|(_, out, _)| out.trim().to_owned()).unwrap_or_default();
    let diff = match run("git", &["diff", &format!("{base}...HEAD")], worktree).await {
        Ok((true, diff, _)) => diff,
        _ => String::new(),
    };
    let staged = match run("git", &["diff", "--cached", base], worktree).await {
        Ok((true, diff, _)) if !diff.trim().is_empty() => diff,
        _ => String::new(),
    };
    let proof = TestProof::check(summary, if staged.is_empty() { &diff } else { &staged }, &head);
    proof.record(workflow)?;
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,6 @@\n-pub fn add(a:i32,b:i32)->i32{a-b}\n+pub fn add(a:i32,b:i32)->i32{a+b}\n+#[test]\n+fn adds_two_numbers() { assert_eq!(add(2,2),4); }\n";

    /// #2464 AC: tests that fail on base and pass on the head are recorded as proof.
    #[test]
    fn tests_failing_on_base_and_passing_on_head_are_recorded_as_proof() {
        let summary = "Fixed add.\n\nTEST-PROOF\ntest: tests::adds_two_numbers\nred: test tests::adds_two_numbers ... FAILED\ngreen: test tests::adds_two_numbers ... ok\nEND-TEST-PROOF\n";
        let proof = TestProof::check(summary, DIFF, "abc123");
        assert!(proof.proven(), "{proof:?}");
        assert_eq!(proof.outcome, ProofOutcome::FailedOnBasePassedOnHead { tests: vec!["tests::adds_two_numbers".into()] });
        assert!(proof.note().contains("failed on base and pass"));
        let recorded = serde_json::to_value(&proof).unwrap();
        assert_eq!(recorded["outcome"]["kind"], "failed_on_base_passed_on_head");
    }

    #[test]
    fn a_proof_the_diff_does_not_back_is_not_shown() {
        let untouched = "TEST-PROOF\ntest: tests::other_test\nred: test tests::other_test ... FAILED\ngreen: test tests::other_test ... ok\nEND-TEST-PROOF";
        assert!(matches!(TestProof::check(untouched, DIFF, "h").outcome, ProofOutcome::NotShown { .. }));
        let never_red = "TEST-PROOF\ntest: adds_two_numbers\nred: test adds_two_numbers ... ok\ngreen: test adds_two_numbers ... ok\nEND-TEST-PROOF";
        assert!(matches!(TestProof::check(never_red, DIFF, "h").outcome, ProofOutcome::NotShown { .. }));
        let still_red = "TEST-PROOF\ntest: adds_two_numbers\nred: test adds_two_numbers ... FAILED\ngreen: test adds_two_numbers ... FAILED\nEND-TEST-PROOF";
        assert!(matches!(TestProof::check(still_red, DIFF, "h").outcome, ProofOutcome::NotShown { .. }));
        assert!(matches!(TestProof::check("no block", DIFF, "h").outcome, ProofOutcome::NotShown { .. }));
    }

    #[test]
    fn refactor_and_doc_changes_need_no_test_proof() {
        let docs = "diff --git a/docs/x.md b/docs/x.md\n--- a/docs/x.md\n+++ b/docs/x.md\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(matches!(TestProof::check("nothing", docs, "h").outcome, ProofOutcome::NotApplicable { .. }));
        assert!(matches!(TestProof::check("TEST-PROOF: none (refactor)", DIFF, "h").outcome, ProofOutcome::NotApplicable { .. }));
        assert!(matches!(TestProof::check("TEST-PROOF: none (docs)", DIFF, "h").outcome, ProofOutcome::NotShown { .. }),
            "a docs claim on a code diff is not accepted");
    }

    #[test]
    fn plan_carries_criteria_and_declared_paths_and_binds_a_plan_contract() {
        let scope = ScopeOutcome {
            fixable: true, complexity: Complexity::Simple, est_diff_lines: Some(40), guarded_paths: false,
            body: "ALLOWED-PATHS: crates/x/src/phone.rs, `docs/phone.md`, ../escape\nAdd phone formatting.".into(),
            criteria: vec!["Phone numbers render in E.164".into()], relevance: Relevance::Current,
        };
        let plan = Plan::from_scope("Add phone formatting", Some(&scope), &scope.criteria);
        assert_eq!(plan.declared_paths, vec!["crates/x/src/phone.rs".to_string(), "docs/phone.md".to_string()]);
        assert!(plan.body.contains("Add phone formatting") && plan.body.contains("Plan:"));
        let contract = plan.contract().unwrap();
        assert!(crate::autoship_workflow::scope_proof_gate::plan_contract(&contract));
        assert_eq!(contract, plan.contract().unwrap(), "stable");
        let tree = "100644 blob aaa\tCargo.toml\0040000 tree bbb\tcrates\0160000 commit ccc\tvendored\0";
        let admission = tree_admission(tree);
        assert_eq!(admission.iter().map(|p| (p.path.as_str(), p.directory)).collect::<Vec<_>>(),
            vec![("Cargo.toml", false), ("crates", true)]);
    }
}
