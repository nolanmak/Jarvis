//! GitHub-owned workflow identity, not a display-name exemption.
use super::*;
use serde_json::Value;
use std::collections::BTreeSet;
const WORKFLOW: &str = ".github/workflows/claude-pr-review.yml";

async fn api(root: &Path, gh: &str, endpoint: &str) -> Option<Value> {
    let (_, out, _) = run(gh, &["api", endpoint], root).await.ok()?;
    serde_json::from_str(&out).ok()
}
fn required_contexts(rules: &Value, legacy: &Value) -> Option<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for rule in rules.as_array()? {
        if rule["type"] == "required_status_checks" {
            for check in rule
                .pointer("/parameters/required_status_checks")?
                .as_array()?
            {
                names.insert(check["context"].as_str()?.to_string());
            }
        }
    }
    if legacy["status"] == "404" && legacy["message"] == "Branch not protected" {
        return Some(names);
    }
    let checks = legacy.get("required_status_checks")?;
    if !checks.is_null() {
        for context in checks["contexts"].as_array()? {
            names.insert(context.as_str()?.to_string());
        }
        for check in checks["checks"].as_array()? {
            names.insert(check["context"].as_str()?.to_string());
        }
    }
    Some(names)
}
fn optional(
    row: &Value,
    job: &Value,
    run: &Value,
    repo: &str,
    head: &str,
    required: &BTreeSet<String>,
) -> bool {
    !row["name"]
        .as_str()
        .is_some_and(|name| required.contains(name))
        && identity(row, job, run, repo, head)
        && run["path"] == WORKFLOW
}
fn identity(row: &Value, job: &Value, run: &Value, repo: &str, head: &str) -> bool {
    let Some(name) = row["name"].as_str() else {
        return false;
    };
    let Some(job_id) = job["id"].as_u64() else {
        return false;
    };
    let Some(run_id) = run["id"].as_u64() else {
        return false;
    };
    job["name"] == name
        && job["run_id"] == run_id
        && run["repository"]["full_name"] == repo
        && run["head_sha"] == head
        && run["path"]
            .as_str()
            .is_some_and(|path| path.starts_with(".github/workflows/"))
        && row["link"] == format!("https://github.com/{repo}/actions/runs/{run_id}/job/{job_id}")
}
/// Unknown advisory identity blocks merge but is a free evidence dependency,
/// never an instruction to repair candidate code.
/// The marker is set only here, on controller-created rows from `gh pr checks`.
pub(super) async fn classify(root: &Path, pr: u64, raw: &str) -> String {
    classify_with_gh(root, &gh_bin(), pr, raw).await
}
pub(super) async fn classify_with_gh(root: &Path, gh: &str, pr: u64, raw: &str) -> String {
    let Ok(mut rows) = serde_json::from_str::<Vec<Value>>(raw) else {
        return raw.into();
    };
    for row in &mut rows {
        if let Some(object) = row.as_object_mut() {
            object.remove("autoship_optional_workflow");
            object.remove("autoship_required_context");
            object.remove("autoship_advisory_identity_unknown");
        }
    }
    for row in &mut rows {
        if matches!(
            row["name"].as_str(),
            Some(
                "Eligibility gate"
                    | "Claude review (comment-only)"
                    | "No-op (no secret configured)"
            )
        ) {
            row["autoship_advisory_identity_unknown"] = true.into();
        }
    }
    let sanitized = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into());
    let Some(rules) = api(root, gh, "repos/{owner}/{repo}/rules/branches/main").await else {
        return sanitized.clone();
    };
    let Some(legacy) = api(root, gh, "repos/{owner}/{repo}/branches/main/protection").await else {
        return sanitized.clone();
    };
    let Some(required) = required_contexts(&rules, &legacy) else {
        return sanitized.clone();
    };
    for row in &mut rows {
        if row["name"].as_str().is_some_and(|n| required.contains(n)) {
            row["autoship_required_context"] = true.into();
            row["autoship_advisory_identity_unknown"] = false.into();
        }
    }
    let Some(repository) = api(root, gh, "repos/{owner}/{repo}").await else {
        return serde_json::to_string(&rows).unwrap_or_else(|_| sanitized.clone());
    };
    let Some(repo) = repository["full_name"].as_str() else {
        return serde_json::to_string(&rows).unwrap_or_else(|_| sanitized.clone());
    };
    let Some(pull) = api(root, gh, &format!("repos/{{owner}}/{{repo}}/pulls/{pr}")).await else {
        return serde_json::to_string(&rows).unwrap_or_else(|_| sanitized.clone());
    };
    let Some(head) = pull["head"]["sha"].as_str() else {
        return serde_json::to_string(&rows).unwrap_or_else(|_| sanitized.clone());
    };
    let prefix = format!("https://github.com/{repo}/actions/runs/");
    let mut runs: std::collections::BTreeMap<u64, Value> = std::collections::BTreeMap::new();
    for row in &mut rows {
        // Display name only limits API work; it never establishes exemption.
        if !matches!(
            row["name"].as_str(),
            Some(
                "Eligibility gate"
                    | "Claude review (comment-only)"
                    | "No-op (no secret configured)"
            )
        ) {
            continue;
        }
        let Some(suffix) = row["link"]
            .as_str()
            .and_then(|link| link.strip_prefix(&prefix))
        else {
            continue;
        };
        let Some((run_id, job_id)) = suffix.split_once("/job/") else {
            continue;
        };
        let (Ok(run_id), Ok(job_id)) = (run_id.parse::<u64>(), job_id.parse::<u64>()) else {
            continue;
        };
        let Some(job) = api(
            root,
            gh,
            &format!("repos/{{owner}}/{{repo}}/actions/jobs/{job_id}"),
        )
        .await
        else {
            continue;
        };
        let run = if let Some(run) = runs.get(&run_id) {
            run.clone()
        } else {
            let Some(run) = api(
                root,
                gh,
                &format!("repos/{{owner}}/{{repo}}/actions/runs/{run_id}"),
            )
            .await
            else {
                continue;
            };
            runs.insert(run_id, run.clone());
            run
        };
        if identity(row, &job, &run, repo, head) {
            row["autoship_advisory_identity_unknown"] = false.into();
        }
        if optional(row, &job, &run, repo, head, &required) {
            row["autoship_optional_workflow"] = true.into();
        }
    }
    serde_json::to_string(&rows).unwrap_or_else(|_| sanitized.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_nonrequired_workflow_can_be_advisory() {
        let base = serde_json::json!({"name":"Claude review (comment-only)","link":"https://github.com/o/r/actions/runs/1/job/2"});
        let job = serde_json::json!({"id":2,"run_id":1,"name":base["name"]});
        let run = serde_json::json!({"id":1,"head_sha":"head","path":WORKFLOW,"repository":{"full_name":"o/r"}});
        for state in ["PENDING", "FAILURE"] {
            let mut row = base.clone();
            row["state"] = state.into();
            row["bucket"] = "fail".into();
            assert!(optional(&row, &job, &run, "o/r", "head", &BTreeSet::new()));
            row["autoship_optional_workflow"] = true.into();
            let rows = serde_json::json!([{"name":"tests","state":"SUCCESS"},row]);
            assert_eq!(ci_checks_green(&rows.to_string()), Some(true));
            assert!(failed_ci_checks(&rows.to_string()).is_empty());
            for changed in [
                serde_json::json!({}),
                serde_json::json!({"path":".github/workflows/required.yml","id":1,"head_sha":"head","repository":{"full_name":"o/r"}}),
            ] {
                assert!(!optional(
                    &base,
                    &job,
                    &changed,
                    "o/r",
                    "head",
                    &BTreeSet::new()
                ));
            }
            assert!(!optional(
                &base,
                &job,
                &run,
                "o/r",
                "other-head",
                &BTreeSet::new()
            ));
            assert!(!optional(
                &base,
                &job,
                &run,
                "o/r",
                "head",
                &BTreeSet::from(["Claude review (comment-only)".into()])
            ));
            let mut unknown = base.clone();
            unknown["state"] = state.into();
            unknown["bucket"] = "fail".into();
            assert_eq!(
                ci_checks_green(&serde_json::json!([unknown]).to_string()),
                Some(false)
            );
            assert_eq!(
                failed_ci_checks(&serde_json::json!([unknown]).to_string()).len(),
                1
            );
        }
    }
    #[test]
    fn explicitly_required_review_context_overrides_advisory_markers() {
        for name in ["CodeRabbit", "Claude review (comment-only)"] {
            let row = serde_json::json!({"name":name,"state":"FAILURE","bucket":"fail","autoship_required_context":true,"autoship_optional_workflow":true});
            assert_eq!(
                ci_checks_green(&serde_json::json!([row]).to_string()),
                Some(false)
            );
            assert_eq!(
                failed_ci_checks(&serde_json::json!([row]).to_string()).len(),
                1
            );
        }
    }
    #[test]
    fn unknown_policy_never_means_no_required_checks() {
        assert!(required_contexts(
            &serde_json::json!([]),
            &serde_json::json!({"message":"Forbidden","status":"403"})
        )
        .is_none());
        let required=required_contexts(&serde_json::json!([{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Claude review (comment-only)"}]}}]),&serde_json::json!({"message":"Branch not protected","status":"404"})).unwrap();
        assert!(required.contains("Claude review (comment-only)"));
    }
}
