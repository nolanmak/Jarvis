//! Execute final-authority source requests against immutable candidate objects.
//! Large files use the existing bounded stream and verified window selector;
//! no requested path grants host filesystem or model tool access.
use super::*;
use sha2::{Digest, Sha256};

fn admitted_path(path: &str) -> bool {
    evidence_plan::valid_missing_path(path)
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|part| {
            matches!(
                part,
                ".ssh" | ".aws" | ".azure" | ".config" | "credentials.json" | "auth.json"
            )
        })
}
fn size(source: &BTreeMap<String, String>) -> Result<usize> {
    Ok(serde_json::to_vec(source)?.len())
}
async fn compact(
    root: &Path,
    guard: Option<&workflow::WorkflowGuard>,
    binding: &aligned::ReviewBinding,
    findings: &[aligned::Finding],
    pool: &mut BTreeMap<String, String>,
    budget: usize,
    receipts: &mut Vec<String>,
) -> Result<()> {
    if size(pool)? <= budget {
        return Ok(());
    }
    let selected = adjudication_windows::select(
        root,
        guard.context("large requested source requires protected evidence selection")?,
        binding,
        findings,
        pool,
        budget,
    )
    .await?;
    receipts.push(aligned::digest(&serde_json::to_vec(&selected)?));
    let mut retained = BTreeMap::new();
    for window in selected["verified_windows"]
        .as_array()
        .context("verified selector windows missing")?
    {
        let key = window["source_key"]
            .as_str()
            .context("verified source key missing")?;
        let start = window["start_byte"]
            .as_u64()
            .context("verified source offset missing")?;
        let end = window["end_byte"]
            .as_u64()
            .context("verified source end missing")?;
        let body = window["text"]
            .as_str()
            .context("verified window bytes missing")?;
        let (prefix, range) = key
            .rsplit_once("#bytes-")
            .context("requested-source offset provenance missing")?;
        let base = range
            .split_once('-')
            .context("requested-source offset malformed")?
            .0
            .parse::<u64>()?;
        let absolute_start = base.checked_add(start).context("source offset overflow")?;
        let absolute_end = base.checked_add(end).context("source offset overflow")?;
        let name = format!("{prefix}#bytes-{absolute_start}-{absolute_end}");
        if let Some(prior) = retained.insert(name, body.into()) {
            anyhow::ensure!(prior == body, "same immutable window changed");
        }
    }
    *pool = retained;
    anyhow::ensure!(
        size(pool)? <= budget,
        "verified requested-source pool exceeds assigned bound"
    );
    Ok(())
}
pub(super) async fn read(
    root: &Path,
    guard: &workflow::WorkflowGuard,
    binding: &aligned::ReviewBinding,
    paths: &BTreeSet<String>,
    findings: &[aligned::Finding],
    controller_sources: &BTreeMap<String, String>,
    budget: usize,
) -> Result<serde_json::Value> {
    read_inner(
        root,
        Some(guard),
        binding,
        paths,
        findings,
        controller_sources,
        budget,
    )
    .await
}
async fn read_inner(
    root: &Path,
    guard: Option<&workflow::WorkflowGuard>,
    binding: &aligned::ReviewBinding,
    paths: &BTreeSet<String>,
    findings: &[aligned::Finding],
    controller_sources: &BTreeMap<String, String>,
    budget: usize,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        !paths.is_empty() && budget >= 2048,
        "requested evidence budget/paths invalid"
    );
    anyhow::ensure!(
        git(root, &["rev-parse", "HEAD"]).await?.trim() == binding.head,
        "requested evidence candidate changed"
    );
    let mut pool = BTreeMap::new();
    let mut states = Vec::new();
    let mut selections = Vec::new();
    let mut source_hashes = BTreeMap::new();
    // Reserve bounded room for source status and provenance. Source itself may
    // be streamed indefinitely across quota-limited native selector passes.
    let source_budget =
        budget.saturating_sub(1024 + paths.iter().map(String::len).sum::<usize>() * 2) / 2;
    anyhow::ensure!(
        source_budget >= 1024,
        "requested source metadata exceeds residual budget"
    );
    for path in paths {
        if !admitted_path(path) {
            states.push(serde_json::json!({"path":path,"status":"not_admitted","reason":"not a permitted repository source path"}));
            continue;
        }
        if let Some(body) = controller_sources.get(path) {
            let mut offset = 0;
            for chunk in chunks(body, 32 * 1024) {
                let end = offset + chunk.len();
                pool.insert(
                    format!(
                        "controller:{}#bytes-{offset}-{end}",
                        serde_json::to_string(path)?
                    ),
                    chunk.into(),
                );
                offset = end;
                if size(&pool)? > source_budget.max(128 * 1024) {
                    compact(
                        root,
                        guard,
                        binding,
                        findings,
                        &mut pool,
                        source_budget,
                        &mut selections,
                    )
                    .await?;
                }
            }
            source_hashes.insert(path.clone(), aligned::digest(body.as_bytes()));
            states.push(serde_json::json!({"path":path,"status":"controller_source_supplied","sha256":aligned::digest(body.as_bytes())}));
            continue;
        }
        let plan = match caller_stream::Plan::named(root, &binding.head, path).await {
            Ok(plan) => plan,
            Err(_) => {
                states.push(serde_json::json!({"path":path,"status":"unavailable","reason":"not an immutable tracked regular source object"}));
                continue;
            }
        };
        let mut stream = plan.stream(root);
        let mut complete = true;
        let mut hasher = Sha256::new();
        let mut source_bytes = 0usize;
        loop {
            let batch = match stream.next_batch().await {
                Ok(batch) => batch,
                Err(_) => {
                    complete = false;
                    break;
                }
            };
            let Some(batch) = batch else {
                break;
            };
            let mut ordered: Vec<_> = batch.into_iter().collect();
            ordered.sort_by_key(|(key, _)| {
                key.rsplit_once("#bytes-")
                    .and_then(|(_, range)| range.split_once('-'))
                    .and_then(|(start, _)| start.parse::<usize>().ok())
                    .unwrap_or(usize::MAX)
            });
            for (key, body) in ordered {
                hasher.update(body.as_bytes());
                source_bytes += body.len();
                pool.insert(key, body);
                if size(&pool)? > source_budget.max(128 * 1024) {
                    compact(
                        root,
                        guard,
                        binding,
                        findings,
                        &mut pool,
                        source_budget,
                        &mut selections,
                    )
                    .await?;
                }
            }
        }
        if complete {
            source_hashes.insert(path.clone(), format!("{:x}", hasher.finalize()));
        }
        states.push(serde_json::json!({"path":path,"source_bytes":source_bytes,"status":if complete {"immutable_source_processed"}else{"incomplete_source"},
            "source_manifest_sha256":aligned::digest(plan.manifest.as_bytes())}));
    }
    compact(
        root,
        guard,
        binding,
        findings,
        &mut pool,
        source_budget,
        &mut selections,
    )
    .await?;
    anyhow::ensure!(
        git(root, &["rev-parse", "HEAD"]).await?.trim() == binding.head,
        "candidate moved during source recovery"
    );
    let value = serde_json::json!({"binding":binding,"requested_paths":paths,"source_status":states,"supplied_source_entries":pool,"source_hashes":source_hashes,
        "window_selection_receipts_sha256":aligned::digest(&serde_json::to_vec(&selections)?),"window_selection_count":selections.len(),
        "scope":"Only supplied_source_entries are source bytes available to this final conversation. Larger sources were processed by independent bounded selection; omitted bytes and unavailable paths are NOT evidence a source-dependent finding is false. No source mutation or authority was produced by this reader."});
    anyhow::ensure!(
        serde_json::to_vec(&value)?.len() <= budget,
        "requested source evidence exceeds final residual budget"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn requested_source_uses_immutable_git_and_preserves_pinned_controller_bytes() {
        let root = super::super::tests::candidate_fixture();
        let head = git(root.path(), &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .trim()
            .to_owned();
        let binding = aligned::ReviewBinding {
            head,
            base: "b".repeat(40),
            environment_id: "fixture".into(),
            contract_hash: "c".repeat(64),
            evidence_hash: "d".repeat(64),
        };
        let committed = git(root.path(), &["show", "HEAD:source.rs"]).await.unwrap();
        std::fs::write(
            root.path().join("source.rs"),
            "mutable working bytes must not be read",
        )
        .unwrap();
        let controller = BTreeMap::from([(
            "autoship_verifiers/observe.py".into(),
            "print(actual_behavior())\n".into(),
        )]);
        let mut paths = BTreeSet::from([
            "source.rs".into(),
            "autoship_verifiers/observe.py".into(),
            ".git/config".into(),
            "../outside".into(),
            "missing.rs".into(),
        ]);
        // Several valid per-finding requests accumulate across recovery rounds.
        paths.extend((0..18).map(|i| format!("missing-{i}.rs")));
        let result = read_inner(
            root.path(),
            None,
            &binding,
            &paths,
            &[],
            &controller,
            16 * 1024,
        )
        .await
        .unwrap();
        let source = result["supplied_source_entries"].as_object().unwrap();
        assert!(source
            .values()
            .any(|body| body.as_str() == Some(committed.as_str())));
        assert!(source
            .values()
            .any(|body| body.as_str() == Some("print(actual_behavior())\n")));
        assert!(!result.to_string().contains("mutable working bytes"));
        assert_eq!(
            result["source_hashes"]["source.rs"],
            aligned::digest(committed.as_bytes())
        );
        let states = result["source_status"].as_array().unwrap();
        assert_eq!(
            states.len(),
            paths.len(),
            "every cumulative request gets an explicit result"
        );
        assert_eq!(
            states
                .iter()
                .filter(|state| state["status"] == "not_admitted")
                .count(),
            2
        );
        assert!(states
            .iter()
            .any(|state| state["path"] == "missing.rs" && state["status"] == "unavailable"));
        assert!(result.get("rulings").is_none());
    }
    #[tokio::test]
    async fn requested_symlink_cannot_read_host_data() {
        use std::os::unix::fs::symlink;
        let root = super::super::tests::candidate_fixture();
        symlink("/etc/passwd", root.path().join("symlink.rs")).unwrap();
        git(root.path(), &["add", "symlink.rs"]).await.unwrap();
        git(root.path(), &["commit", "-qm", "fixture symlink"])
            .await
            .unwrap();
        let binding = aligned::ReviewBinding {
            head: git(root.path(), &["rev-parse", "HEAD"])
                .await
                .unwrap()
                .trim()
                .into(),
            base: "b".repeat(40),
            environment_id: "fixture".into(),
            contract_hash: "c".repeat(64),
            evidence_hash: "d".repeat(64),
        };
        let result = read_inner(
            root.path(),
            None,
            &binding,
            &BTreeSet::from(["symlink.rs".into()]),
            &[],
            &BTreeMap::new(),
            8192,
        )
        .await
        .unwrap();
        assert_eq!(result["source_status"][0]["status"], "unavailable");
        assert!(result["supplied_source_entries"]
            .as_object()
            .unwrap()
            .is_empty());
    }
}
