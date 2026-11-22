//! #2444 — GitHub calls per worker tick must not grow with open PRs.
//! Production selectors and the merge sweep run against a scripted `gh` that
//! records every invocation; no model, network or mutation is involved.
use super::*;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "AUTOSHIP_GH_BUDGET_FIXTURE";

const FAKE_GH: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
a = sys.argv[1:]
root = pathlib.Path(os.environ['AUTOSHIP_GH_BUDGET_FIXTURE'])
with (root / 'calls').open('a') as out:
    out.write(json.dumps(a) + '\n')
prs = json.loads((root / 'prs.json').read_text())
issues = json.loads((root / 'issues.json').read_text())
if a[:2] == ['repo', 'view']:
    print('owner/repo')
elif a[:2] == ['pr', 'list']:
    print(json.dumps(prs))
elif a[:2] == ['issue', 'list']:
    print(json.dumps(issues))
elif a[:2] == ['api', 'rate_limit']:
    print('4999' if '--jq' in a else json.dumps({'resources': {'core': {'remaining': 4999}}}))
elif a[:2] == ['pr', 'view']:
    print(json.dumps(next(p for p in prs if p['number'] == int(a[2]))))
elif a[:2] == ['issue', 'view']:
    print(json.dumps({'title': 'Fixture', 'body': 'Fixture request', 'author': {'login': 'owner'}}))
elif a[:2] == ['pr', 'diff']:
    print('src/lib.rs')
elif a[:1] == ['api'] and '/issues/' in a[1] and a[1].rsplit('/', 1)[-1].isdigit():
    n = int(a[1].rsplit('/', 1)[-1])
    print(json.dumps(next((i for i in issues if i['number'] == n), {'number': n, 'state': 'open', 'labels': []})))
else:
    sys.exit(1)
"#;

fn rows(open: u64) -> (serde_json::Value, serde_json::Value) {
    let mut prs = Vec::new();
    let mut issues = Vec::new();
    for k in 1..=open {
        // Alternate loop-authored branches and owner PRs linked by closing
        // references, the two shapes the selectors and merge sweep examine.
        let (branch, body) = if k % 2 == 1 {
            (format!("{BRANCH_PREFIX}{k}"), format!("{SELF_IMPROVE_BODY_MARKER}{k}."))
        } else {
            (format!("feat/repair-{k}"), format!("Owner repair for #{k}"))
        };
        prs.push(serde_json::json!({
            "number": 100 + k, "isDraft": true, "headRefName": branch,
            "headRefOid": format!("{:040x}", 100 + k), "baseRefName": "main",
            "baseRefOid": format!("{:040x}", 1), "mergeable": "MERGEABLE",
            "isCrossRepository": false, "headRepositoryOwner": {"login": "owner"},
            "author": {"login": "owner"}, "title": format!("Repair widget {k}"), "body": body,
            "closingIssuesReferences": [{"number": k, "url": format!("https://github.com/owner/repo/issues/{k}")}],
            "labels": [], "state": "OPEN", "url": format!("https://github.com/owner/repo/pull/{}", 100 + k),
        }));
        issues.push(serde_json::json!({
            "number": k, "state": "open", "title": format!("Widget {k}"), "body": "Fix the widget.",
            "labels": [], "author": {"login": "owner"}, "user": {"login": "owner"}, "author_association": "OWNER",
        }));
    }
    (serde_json::Value::Array(prs), serde_json::Value::Array(issues))
}

/// One worker tick's read side: both selectors, then the reconciliation sweep.
async fn tick(root: &Path) {
    gh_budget::tick_scope(async {
        let _ = find_resumable_draft(root, None, true).await;
        let _ = adopted_execution::next(root, true).await;
    })
    .await;
    gh_budget::tick_scope(merge_sweep(root, true)).await;
}

fn calls(root: &Path) -> std::collections::BTreeMap<String, usize> {
    let mut kinds = std::collections::BTreeMap::new();
    for line in std::fs::read_to_string(root.join("calls")).unwrap_or_default().lines() {
        let args: Vec<String> = serde_json::from_str(line).unwrap();
        // Group by command and endpoint shape, never by PR or issue number.
        let kind = args.iter().take(2)
            .map(|arg| arg.split('/').map(|part| if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {"N"} else {part}).collect::<Vec<_>>().join("/"))
            .collect::<Vec<_>>().join(" ");
        *kinds.entry(kind).or_default() += 1;
    }
    kinds
}

async fn measure(open: u64) -> std::collections::BTreeMap<String, usize> {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("repo");
    std::fs::create_dir(&root).unwrap();
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("/usr/bin/git").args(args).current_dir(&root)
            .env_clear().env("PATH", "/usr/bin:/bin").status().unwrap().success());
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "https://github.com/owner/repo.git"]);
    git(&["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
        "commit", "--quiet", "--allow-empty", "-m", "Original fixture"]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    std::fs::write(root.join(".git/info/exclude"), "prs.json\nissues.json\ncalls\n").unwrap();
    let (prs, issues) = rows(open);
    std::fs::write(root.join("prs.json"), serde_json::to_vec(&prs).unwrap()).unwrap();
    std::fs::write(root.join("issues.json"), serde_json::to_vec(&issues).unwrap()).unwrap();
    let gh = base.join("github.py");
    augmentagent_test_support::write_executable(&gh, FAKE_GH).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "self_improve::gh_budget_tests::github_calls_per_tick_do_not_grow_with_open_prs", "--nocapture"])
        .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &base)
        .env("XDG_STATE_HOME", base.join("state"))
        .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", base.join("workflow.db"))
        .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner")
        .env("AUGMENTAGENT_GH_OWNER", "owner").env("GH_BIN", &gh)
        .env("CODEX_CLI", "/bin/false").env("CLAUDE_CLI", "/bin/false")
        .env(CHILD, &root)
        .current_dir(&root).output().await.unwrap();
    assert!(output.status.success(), "open={open}: {}\n{}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    calls(&root)
}

#[tokio::test]
async fn github_calls_per_tick_do_not_grow_with_open_prs() {
    if let Some(root) = std::env::var_os(CHILD) {
        tick(Path::new(&root)).await;
        return;
    }
    let few = measure(2).await;
    let many = measure(30).await;
    let total = |kinds: &std::collections::BTreeMap<String, usize>| kinds.values().sum::<usize>();
    assert_eq!(few, many, "GitHub calls per tick grew with open PRs: 2 PRs={} calls, 30 PRs={} calls",
        total(&few), total(&many));
    eprintln!("GitHub calls per tick ({} total): {many:?}", total(&many));
    for (kind, count) in &many {
        assert!(*count <= 3, "{kind} ran {count} times in one tick");
    }
}
