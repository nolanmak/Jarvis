//! Disposable caller tests. Scripted GitHub results grant no live authority.
use super::*;
use std::os::unix::fs::PermissionsExt;

const ROOT: &str = "AUTOSHIP_LINKED_BOUNDARY_ROOT";
const GH: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
root=pathlib.Path(os.environ['AUTOSHIP_LINKED_BOUNDARY_ROOT']);a=sys.argv[1:]
mode=os.environ['AUTOSHIP_LINKED_BOUNDARY_MODE']
with (root/'calls').open('a') as stream:stream.write(json.dumps(a)+'\n')
head=(root/'head').read_text() if (root/'head').exists() else 'a'*40
branch='agent-fix/issue-7'
def own():
 return dict(number=12,url='https://github.com/owner/repo/pull/12',headRefName=branch,
  headRefOid=head,baseRefName='main',isCrossRepository=False,state='OPEN',
  headRepositoryOwner={'login':'owner'},author={'login':'owner'},
  closingIssuesReferences=[{'number':7,'url':'https://github.com/owner/repo/issues/7'}])
if a[:2]==['repo','view']:print('owner/repo')
elif a[:2]==['issue','view']:print(json.dumps({'state':'OPEN'}))
elif a[:1]==['api']:print(json.dumps({'object':{'sha':head}}))
elif a[:2]==['pr','view']:
 assert a[2]=='12' and (root/'own-visible').exists()
 print(json.dumps(own()))
elif a[:2]==['pr','list']:
 if '--head' in a:
  if (root/'history-fail').exists():sys.exit(1)
  print(json.dumps([own()] if (root/'own-visible').exists() else []))
 else:
  if (root/'inventory-fail').exists():sys.exit(1)
  assert a[a.index('--state')+1]=='open'
  if not (root/'blocked').exists():print('[]')
  elif mode=='unknown-json':print('{bad-json')
  elif mode=='unknown-command':sys.exit(1)
  else:
   row=own();row.update(number=99,headRefName='fix/already-open',headRefOid='b'*40)
   print(json.dumps([row]))
elif a[:2]==['pr','create']:
 with (root/'creates').open('a') as stream:stream.write('create\n')
 if os.environ['AUTOSHIP_LINKED_BOUNDARY_KIND']=='publication':
  assert mode=='lost-response-claim' and not (root/'blocked').exists()
  sys.exit(1) # The simulated server accepted creation but its reply was lost.
 assert '--draft' in a and not (root/'blocked').exists()
 print('https://github.com/owner/repo/pull/12')
else:raise Exception('unexpected fixture request '+repr(a))
"#;

async fn fixture(kind: &str, mode: &str, name: &str) -> bool {
    if std::env::var_os(ROOT).is_some() {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    let gh = root.path().join("github.py");
    std::fs::write(&gh, GH).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("self_improve::linked_claim_boundaries::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env(ROOT, root.path())
        .env("AUTOSHIP_LINKED_BOUNDARY_KIND", kind)
        .env("AUTOSHIP_LINKED_BOUNDARY_MODE", mode)
        .env("GH_BIN", gh)
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env(
            "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
            root.path().join("workflow.db"),
        )
        .env(
            "AUGMENTAGENT_AGENT_WORKDIR",
            root.path().join("remote-work"),
        )
        .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner")
        .current_dir(root.path())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{kind}/{mode}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join("completed").exists());
    true
}
fn create_count(root: &Path) -> usize {
    std::fs::read_to_string(root.join("creates"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[tokio::test]
async fn publication_claims_preserve_intents_and_reconcile_original_identity() {
    let name = "publication_claims_preserve_intents_and_reconcile_original_identity";
    if std::env::var_os(ROOT).is_none() {
        for mode in [
            "known-claim",
            "unknown-json",
            "unknown-command",
            "lost-response-claim",
        ] {
            assert!(fixture("publication", mode, name).await);
        }
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let mode = std::env::var("AUTOSHIP_LINKED_BOUNDARY_MODE").unwrap();
    let repo = root.join("repo");
    let worktree = root.join("candidate");
    std::fs::create_dir(&repo).unwrap();
    assert!(run("git", &["init", "-q"], &repo).await.unwrap().0);
    std::fs::write(repo.join("source"), "retained candidate\n").unwrap();
    assert!(run("git", &["add", "source"], &repo).await.unwrap().0);
    assert!(
        run(
            "git",
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "candidate"
            ],
            &repo
        )
        .await
        .unwrap()
        .0
    );
    let head = run("git", &["rev-parse", "HEAD"], &repo)
        .await
        .unwrap()
        .1
        .trim()
        .to_owned();
    std::fs::write(root.join("head"), &head).unwrap();
    assert!(
        run(
            "git",
            &["update-ref", "refs/remotes/origin/main", &head],
            &repo
        )
        .await
        .unwrap()
        .0
    );
    assert!(
        run(
            "git",
            &[
                "worktree",
                "add",
                "-b",
                "agent-fix/issue-7",
                worktree.to_str().unwrap(),
                &head
            ],
            &repo
        )
        .await
        .unwrap()
        .0
    );
    let path = root.join("workflow.db");
    let mut guard = workflow::WorkflowGuard::acquire_fixture(
        &path,
        workflow::Start {
            repo_root: &repo,
            worktree: &worktree,
            repository: "owner/repo",
            source_issue: Some(7),
            pr: None,
            branch: "agent-fix/issue-7",
            contract: "original-contract",
            environment: "fixture",
        },
    )
    .await
    .unwrap()
    .unwrap();
    guard
        .checkpoint(
            crate::autoship_workflow::Stage::Publish,
            "fixture_ready",
            "publish",
            0,
            &["original-evidence".into()],
        )
        .await
        .unwrap();
    let before = crate::autoship_workflow::WorkflowStore::open(&path)
        .unwrap()
        .snapshot(guard.id())
        .unwrap()
        .unwrap();
    let args = [
        "pr",
        "create",
        "--head",
        "agent-fix/issue-7",
        "--base",
        "main",
    ];
    if mode == "lost-response-claim" {
        let error = publication::publish(&guard, &worktree, "agent-fix/issue-7", &head, 7, &args)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<linked_claims::Hold>().is_none());
        assert_eq!(create_count(&root), 1);
    }
    std::fs::write(root.join("blocked"), "").unwrap();
    let error = publication::publish(&guard, &worktree, "agent-fix/issue-7", &head, 7, &args)
        .await
        .unwrap_err();
    let hold = if mode.starts_with("unknown") {
        linked_claims::Hold::Unknown
    } else {
        linked_claims::Hold::Claimed
    };
    assert_eq!(error.downcast_ref::<linked_claims::Hold>(), Some(&hold));
    let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
    let after = store.snapshot(guard.id()).unwrap().unwrap();
    assert_eq!(after.checkpoint.stage, before.checkpoint.stage);
    assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
    assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
    assert_eq!(after.progress_at_ms, before.progress_at_ms);
    assert_eq!(after.model_calls, 0);
    assert_eq!(after.checkpoint.next.reason, hold.reason());
    let pending = store.pending_publications("owner/repo").unwrap();
    assert_eq!(pending.len(), 1);
    let pending_body = pending[0].1.clone();
    let op = guard
        .pending_operations()
        .unwrap()
        .into_iter()
        .find(|op| op.kind == crate::autoship_workflow::OperationKind::CreatePr)
        .unwrap();
    let dispatch = store.publication_dispatch(&op.id).unwrap();
    let expected = usize::from(mode == "lost-response-claim");
    assert_eq!(create_count(&root), expected);
    assert_eq!(
        dispatch.as_ref().map(|v| v.0),
        if expected == 1 { Some(1) } else { None }
    );
    drop(store);
    // Original exact identity must settle even while issue inventory is unknown.
    std::fs::write(root.join("own-visible"), "").unwrap();
    std::fs::write(root.join("inventory-fail"), "").unwrap();
    let found = publication::publish(&guard, &worktree, "agent-fix/issue-7", &head, 7, &args)
        .await
        .unwrap();
    assert_eq!(found.number, 12);
    assert_eq!(create_count(&root), expected);
    let store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
    assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().pr, Some(12));
    assert!(!store.has_pending_publication(guard.id()).unwrap());
    // Reopen the original crash-window index through its protected API. The
    // already-observed operation must use direct identity, never list/create.
    guard.save_publication(&pending_body).unwrap();
    std::fs::write(root.join("history-fail"), "").unwrap();
    assert_eq!(
        publication::publish(&guard, &worktree, "agent-fix/issue-7", &head, 7, &args)
            .await
            .unwrap()
            .number,
        12
    );
    assert_eq!(create_count(&root), expected);
    assert!(!store.has_pending_publication(guard.id()).unwrap());
    assert_eq!(
        std::fs::read_to_string(worktree.join("source")).unwrap(),
        "retained candidate\n"
    );
    guard.finish().unwrap();
    std::fs::write(root.join("completed"), "").unwrap();
}

#[tokio::test]
async fn approved_remote_runs_hold_without_losing_approval_then_publish_a_draft() {
    let name = "approved_remote_runs_hold_without_losing_approval_then_publish_a_draft";
    if std::env::var_os(ROOT).is_none() {
        for mode in ["known-claim", "unknown-json", "unknown-command"] {
            assert!(fixture("remote", mode, name).await);
        }
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    std::fs::create_dir(root.join("remote-work")).unwrap();
    let path = root.join("product-state.db");
    let store = Store::open(&path).unwrap();
    store
        .upsert_agent_repo("owner/repo", "main", "", "", 600)
        .unwrap();
    let row = store
        .insert_agent_pr_run(
            "owner/repo",
            7,
            "agent-fix/issue-7",
            "Synthetic approved repair",
            5,
            "pending_approval",
        )
        .unwrap();
    let approved = store.approve_agent_pr_run(&row.id).unwrap().unwrap();
    std::fs::write(root.join("blocked"), "").unwrap();
    let outcome = open_approved_runs(&store).await.unwrap();
    let expected = if std::env::var("AUTOSHIP_LINKED_BOUNDARY_MODE")
        .unwrap()
        .starts_with("unknown")
    {
        "linked_pr_inventory_unknown"
    } else {
        "linked_pr_claim_exists"
    };
    assert!(outcome.contains(expected));
    assert_eq!(create_count(&root), 0);
    assert_eq!(
        serde_json::to_value(store.get_agent_pr_run(&row.id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&approved).unwrap()
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.get_agent_pr_run(&row.id).unwrap().unwrap().status,
        "approved"
    );
    std::fs::remove_file(root.join("blocked")).unwrap();
    assert!(open_approved_runs(&store)
        .await
        .unwrap()
        .contains("draft PR opened"));
    let published = store.get_agent_pr_run(&row.id).unwrap().unwrap();
    assert_eq!(published.status, "pr_opened");
    assert_eq!(published.branch, approved.branch);
    assert_eq!(
        published.pr_url.as_deref(),
        Some("https://github.com/owner/repo/pull/12")
    );
    assert_eq!(create_count(&root), 1);
    std::fs::write(root.join("completed"), "").unwrap();
}
