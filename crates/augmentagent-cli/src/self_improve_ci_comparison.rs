//! Paired diagnostic replay using the existing pinned CI runner and build VM.
//! A comparison is not a passing CI receipt and never authorizes publication.
use super::*;

#[derive(Debug)]
pub(in crate::self_improve) struct ReplayResult {
    pub failed: bool,
    pub failed_check: Option<String>,
    pub diagnostics: String,
}
impl Serialize for ReplayResult {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serde_json::json!({"failed":self.failed,"failed_check":self.failed_check,
            "diagnostic_sha256":digest(self.diagnostics.as_bytes())})
        .serialize(serializer)
    }
}
#[derive(Debug, Serialize)]
pub(in crate::self_improve) struct Comparison {
    pub candidate: String,
    pub main: String,
    pub recipe_sha256: String,
    pub runtime_sha256: String,
    pub candidate_result: ReplayResult,
    pub main_result: ReplayResult,
}

fn select<'a>(
    plan: &'a serde_json::Value,
    binding: &Binding,
    workflow: &str,
    job: &str,
) -> Result<Vec<&'a serde_json::Value>> {
    let parsed: Plan = serde_json::from_value(plan.clone())?;
    parsed.validate(binding)?;
    let checks = plan["checks"]
        .as_array()
        .context("paired CI checks unavailable")?
        .iter()
        .filter(|check| check["workflow"] == workflow && check["job"] == job)
        .collect::<Vec<_>>();
    ensure!(
        !checks.is_empty()
            && checks.iter().all(|check| check["status"] == "ready"
                && check["matrix"]
                    .as_object()
                    .is_some_and(|matrix| matrix.is_empty())
                && check["body"].as_str().is_some_and(|body| !body.is_empty())),
        "{PREREQUISITE}: failed job has no complete comparable local recipe"
    );
    Ok(checks)
}
fn recipe(checks: &[&serde_json::Value]) -> Result<String> {
    // Source-head arguments differ deliberately; command, matrix, declared
    // environment, prerequisites and deadlines must be identical.
    let rows = checks.iter().map(|row| serde_json::json!({
        "id":row["id"],"command_id":row["command_id"],"body":row["body"],"shell":row["shell"],
        "matrix":row["matrix"],"environment":row["environment"],"prerequisites":row["prerequisites"],
        "working_directory":row["working_directory"],"job_timeout_seconds":row["job_timeout_seconds"],
        "step_timeout_seconds":row["step_timeout_seconds"]})).collect::<Vec<_>>();
    Ok(digest(&serde_json::to_vec(&rows)?))
}

struct Execution<'a> {
    root: &'a Path,
    target: &'a Path,
    storage: &'a crate::build_storage::Lease,
    vm: &'a vm::Session,
    audit: &'a Arc<augmentagent_channel_core::AuditLogger>,
    started: std::time::Instant,
}
impl Execution<'_> {
    fn remaining(&self) -> Result<std::time::Duration> {
        std::time::Duration::from_secs(900)
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .context("shared CI prerequisite unavailable: paired replay time budget exhausted")
    }
    async fn plan(
        &self,
        source: &Path,
        binding: &Binding,
        changed: &[String],
    ) -> Result<serde_json::Value> {
        let result = self
            .vm
            .run(
                "plan",
                source,
                self.root,
                binding,
                None,
                changed,
                &[],
                &[],
                None,
                self.storage,
                self.target,
                self.audit,
                self.remaining()?.min(std::time::Duration::from_secs(180)),
            )
            .await?;
        ensure!(
            result.status == "planned",
            "{PREREQUISITE}: paired CI planning unavailable"
        );
        result
            .plan
            .context("shared CI prerequisite unavailable: paired plan missing")
    }
    async fn replay(
        &self,
        source: &Path,
        binding: &Binding,
        checks: &[&serde_json::Value],
    ) -> Result<ReplayResult> {
        let started = std::time::Instant::now();
        for row in checks {
            let check: Check = serde_json::from_value((*row).clone())?;
            let result = self
                .vm
                .run(
                    "step",
                    source,
                    self.root,
                    binding,
                    Some(&check),
                    &[],
                    &[],
                    &[],
                    None,
                    self.storage,
                    self.target,
                    self.audit,
                    check.remaining(started.elapsed())?.min(self.remaining()?),
                )
                .await?;
            if result.status != "passed" {
                ensure!(
                    result.status == "failed",
                    "{PREREQUISITE}: paired CI result unavailable"
                );
                return Ok(ReplayResult {
                    failed: true,
                    failed_check: Some(check.id),
                    diagnostics: format!(
                        "{}\n{}",
                        failure_excerpt(&result.stdout, 16000),
                        failure_excerpt(&result.stderr, 8000)
                    ),
                });
            }
        }
        Ok(ReplayResult {
            failed: false,
            failed_check: None,
            diagnostics: String::new(),
        })
    }
}

/// Only fixed, non-matrix job identities are replayed in this delivery slice.
/// Unsupported jobs remain an explicit dependency rather than guessing a recipe.
pub(in crate::self_improve) async fn compare(
    root: &Path,
    expected_head: &str,
    main: &str,
    workflow: &str,
    display_name: &str,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<Comparison> {
    let before = binding(root).await?;
    ensure!(
        before.head == expected_head
            && before.verification_head == expected_head
            && before.status.trim().is_empty(),
        "{PREREQUISITE}: candidate changed before paired CI replay"
    );
    let contract: serde_json::Value = serde_json::from_str(&contract_for(root)?)?;
    let jobs = contract["workflows"][workflow]["jobs"]
        .as_object()
        .context("shared CI prerequisite unavailable: failed workflow not in pinned contract")?;
    let matching = jobs
        .iter()
        .filter(|(key, row)| {
            row["name"].as_str().unwrap_or(key) == display_name && row.get("strategy").is_none()
        })
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    let [job] = matching.as_slice() else {
        bail!("{PREREQUISITE}: failed job identity cannot be paired");
    };
    // Selection scripts whose command set depends on the source diff need a
    // separately frozen selection contract. Do not compare different suites.
    for row in jobs[*job]["steps"].as_array().context("CI steps missing")? {
        if let Some(id) = row["command_id"].as_str() {
            let body = contract["commands"][id]["body"]
                .as_str()
                .context("CI command missing")?;
            ensure!(
                !body.contains("ci-targeted.py") && !body.contains("github.event.pull_request"),
                "{PREREQUISITE}: failed job requires frozen diff-dependent selection"
            );
        }
    }
    let target = crate::build_storage::effective_target(root);
    let storage = crate::build_storage::admit_ci(root, &target).await?;
    let session = vm::Session::prepare(root, &target)?;
    let mut baseline = before.clone();
    baseline.head = main.into();
    baseline.verification_head = main.into();
    baseline.index_tree = git(root, &["rev-parse", &format!("{main}^{{tree}}")])
        .await?
        .trim()
        .into();
    baseline.status.clear();
    let changed = git(
        root,
        &["diff", "--name-only", "-z", &before.base, &before.head],
    )
    .await?
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let execution = Execution {
        root,
        target: &target,
        storage: &storage,
        vm: &session,
        audit,
        started: std::time::Instant::now(),
    };
    let candidate_source = snapshot(root, &before, &target).await?;
    let baseline_source = snapshot(root, &baseline, &target).await?;
    let candidate_plan = execution
        .plan(candidate_source.path(), &before, &changed)
        .await?;
    let baseline_plan = execution
        .plan(baseline_source.path(), &baseline, &changed)
        .await?;
    let candidate_checks = select(&candidate_plan, &before, workflow, job)?;
    let baseline_checks = select(&baseline_plan, &baseline, workflow, job)?;
    let recipe_sha256 = recipe(&candidate_checks)?;
    ensure!(
        recipe_sha256 == recipe(&baseline_checks)?,
        "{PREREQUISITE}: paired CI recipes/environments differ"
    );
    ensure!(
        binding(root).await? == before
            && git(root, &["rev-parse", "origin/main"]).await?.trim() == main,
        "{PREREQUISITE}: source moved during paired CI planning"
    );
    // Planning may execute candidate toolchain probes. Start from fresh literal
    // exports so a probe cannot edit the code later used by the comparison.
    drop(candidate_source);
    drop(baseline_source);
    let candidate_source = snapshot(root, &before, &target).await?;
    let candidate_result = execution
        .replay(candidate_source.path(), &before, &candidate_checks)
        .await?;
    drop(candidate_source);
    let baseline_source = snapshot(root, &baseline, &target).await?;
    let main_result = execution
        .replay(baseline_source.path(), &baseline, &baseline_checks)
        .await?;
    ensure!(
        binding(root).await? == before
            && git(root, &["rev-parse", "origin/main"]).await?.trim() == main,
        "{PREREQUISITE}: source moved during paired CI replay"
    );
    let comparison = Comparison {
        candidate: expected_head.into(),
        main: main.into(),
        recipe_sha256,
        runtime_sha256: session.authority_sha256.clone(),
        candidate_result,
        main_result,
    };
    audit.record_controller(
        serde_json::json!({"kind":"hosted_ci_paired_diagnostic", "comparison":comparison,
        "shipping_authority":false,"full_ci_pass":false}),
    )?;
    Ok(comparison)
}
