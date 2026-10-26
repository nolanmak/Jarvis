//! Explicitly invoked synthetic GitHub acceptance. Never runs in ordinary CI.
use super::*;
use crate::autoship_workflow::{Binding, WorkflowStore};

struct ProbeTransport<'a> {
    real: MergeTransport<'a>,
    repository: String,
    race: Option<(String, String)>,
    discard: bool,
    mutations: usize,
}
#[async_trait::async_trait]
impl Github for ProbeTransport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<String> {
        if args.starts_with(&["api", "graphql"]) {
            if let Some((branch, head)) = self.race.take() {
                self.real.call(&["api", "--method", "PATCH",
                    &format!("repos/{}/git/refs/heads/{branch}", self.repository),
                    "-f", &format!("sha={head}"), "-F", "force=false"]).await?;
            }
            self.mutations += 1;
            let response = self.real.call(args).await?;
            if self.discard {
                self.discard = false;
                anyhow::bail!("injected response loss AFTER actual GitHub mutation returned");
            }
            return Ok(response);
        }
        self.real.call(args).await
    }
}

#[tokio::test]
#[ignore = "mutates an explicitly provisioned synthetic GitHub repository"]
async fn disposable_github_merge_boundary() -> Result<()> {
    let repository = std::env::var("AUTOSHIP_GITHUB_PROBE_REPOSITORY")?;
    let (_, name) = repository.split_once('/').context("repository identity")?;
    anyhow::ensure!(name.starts_with("autoship-acceptance-"), "only disposable repository admitted");
    let root = PathBuf::from(std::env::var("AUTOSHIP_GITHUB_PROBE_ROOT")?);
    let mode = std::env::var("AUTOSHIP_GITHUB_PROBE_MODE")?;
    let pr: u64 = std::env::var("AUTOSHIP_GITHUB_PROBE_PR")?.parse()?;
    let head = std::env::var("AUTOSHIP_GITHUB_PROBE_HEAD")?;
    let mut real = MergeTransport(&root);
    let metadata: serde_json::Value = serde_json::from_str(&real.call(&["api", &format!("repos/{repository}")]).await?)?;
    anyhow::ensure!(metadata["description"] == "Synthetic autoship acceptance fixture; no production source or credentials.", "fixture marker missing");
    let state = root.join(format!("probe-{pr}-{mode}.sqlite3"));
    let mut store = WorkflowStore::open(&state)?;
    let id = format!("github-probe/{repository}/{pr}/{mode}");
    let binding = Binding {head:head.clone(),contract:"synthetic-publication-boundary".into(),evidence_hash:head.clone()};
    let now = chrono::Utc::now().timestamp_millis();
    store.register(&id,"fixture",binding.clone(),now)?;
    let lease = store.claim(&id,"explicit-probe",now,1_000_000)?.context("probe already active")?;
    store.attach_pr(&lease,pr,now)?;
    let operation = store.begin_operation(&lease,OperationKind::MergePr,&binding,now)?;
    let race = if mode == "head-race" { Some((std::env::var("AUTOSHIP_GITHUB_PROBE_BRANCH")?,std::env::var("AUTOSHIP_GITHUB_PROBE_RACE_HEAD")?)) } else { None };
    let mut transport = ProbeTransport {real,repository:repository.clone(),race,discard:mode=="lost",mutations:0};
    if mode == "stale" {
        require_strict_integration(&mut transport,&repository).await?;
        let compare: serde_json::Value = serde_json::from_str(&transport.call(&["api",
            &format!("repos/{repository}/compare/main...{head}")]).await?)?;
        anyhow::ensure!(compare["behind_by"].as_u64().unwrap_or(0)>0,
            "candidate must demonstrably lack current main ancestry");
        let pull: serde_json::Value = serde_json::from_str(&transport.call(&["api",
            &format!("repos/{repository}/pulls/{pr}")]).await?)?;
        anyhow::ensure!(pull["mergeable_state"]=="behind" && pull["head"]["sha"]==head,
            "stale fixture remote state is not behind at the exact reviewed head");
        let main: serde_json::Value = serde_json::from_str(&transport.call(&["api",
            &format!("repos/{repository}/commits/main")]).await?)?;
        let advanced_at=main["commit"]["committer"]["date"].as_str().context("main commit timestamp missing")?;
        let checks: serde_json::Value = serde_json::from_str(&transport.call(&["api",
            &format!("repos/{repository}/commits/{head}/check-runs?per_page=100")]).await?)?;
        for name in ["privacy","node-release"] {
            let runs: Vec<_>=checks["check_runs"].as_array().context("missing check runs")?.iter()
                .filter(|run|run["name"]==name && run["app"]["id"]==15368).collect();
            anyhow::ensure!(!runs.is_empty() && runs.iter().all(|run|
                run["status"]=="completed" && run["conclusion"]=="success"
                && run["completed_at"].as_str().is_some_and(|time|time<=advanced_at)),
                "stale candidate lacks prior successful required app check before main advanced: {name}");
        }
        println!("stale boundary evidence: {}",serde_json::json!({"behind_by":compare["behind_by"],"head":head,"main":main["sha"],"mergeable_state":pull["mergeable_state"],
            "required_checks_previously_green":["privacy","node-release"],"strict_admin_protection":true}));
    }
    let result = merge_mutation(&mut transport,&repository,pr,&head,&operation.id).await;
    if matches!(mode.as_str(),"stale"|"head-race") {
        let error = result.expect_err("GitHub accepted forbidden mutation").to_string();
        let lower = error.to_ascii_lowercase();
        let expected = if mode == "stale" {
            ["base branch", "behind", "up to date", "up-to-date", "required status checks are expected"].iter().any(|s| lower.contains(s))
        } else {
            ["head branch", "head oid", "head commit", "expectedheadoid"].iter().any(|s| lower.contains(s))
        };
        anyhow::ensure!(expected,"unexpected rejection cannot prove boundary: {error}");
        println!("{mode} expected remote rejection: {error}");
        anyhow::ensure!(transport.mutations == 1,"rejection did not reach actual GitHub mutation");
        let observed:serde_json::Value=serde_json::from_str(&transport.call(&["pr","view",&pr.to_string(),"--json","state"]).await?)?;
        anyhow::ensure!(observed["state"]=="OPEN","forbidden candidate merged");
    } else {
        match mode.as_str() {
            "accepted" => {
                let receipt = result?.context("expected accepted own mutation")?;
                store.record_merge_mutation(&lease,&operation.id,&serde_json::to_value(&receipt)?,now)?;
                anyhow::ensure!(transport.mutations==1,"own merge count mismatch");
            },
            "lost" => { anyhow::ensure!(result.is_err() && transport.mutations==1,"lost-response fault not exercised"); },
            "manual" => { anyhow::ensure!(result?.is_none() && transport.mutations==0,"already merged candidate attributed to controller"); },
            _ => anyhow::bail!("unknown probe mode"),
        }
        for _ in 0..2 {
            anyhow::ensure!(merge_mutation(&mut transport,&repository,pr,&head,&operation.id).await?.is_none(),"reconciliation replayed merge");
        }
        let observed:serde_json::Value=serde_json::from_str(&transport.call(&["pr","view",&pr.to_string(),"--json","state,headRefOid"]).await?)?;
        anyhow::ensure!(observed["state"]=="MERGED" && observed["headRefOid"]==head,"exact merged head unconfirmed");
        store.complete_operation(&lease,&operation.id,"observed-merged-exact",now)?;
        drop(store);
        let reopened=WorkflowStore::open(&state)?;
        anyhow::ensure!(reopened.has_merge_mutation(&operation.id)?==(mode=="accepted"),"false autonomous attribution after restart");
        anyhow::ensure!(transport.mutations==if mode=="manual" {0} else {1},"repeat reconciliation dispatched mutation");
    }
    // A failed process leaves a lease to expire; successful stages release it.
    WorkflowStore::open(&state)?.release(&lease,now)?;
    Ok(())
}
