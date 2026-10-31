//! Paid, isolated source-less adoption acceptance. Real native models and Git;
//! GitHub responses/CI/merge are explicitly simulated by a local controller.
use super::*;
use augmentagent_channel_core::ProviderKind::Codex;

async fn fixture_git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "fixture git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

const GH_FIXTURE: &str = r#"#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys
root=pathlib.Path(os.environ['AUTOSHIP_ADOPTION_ACCEPTANCE_CHILD'])
c=json.loads((root/'fixture.json').read_text()); a=sys.argv[1:]
def git(*args):
 return subprocess.check_output(['/usr/bin/git','--git-dir',c['origin'],*args],text=True).strip()
head=git('rev-parse','refs/heads/'+c['branch']); state='MERGED' if (root/'merged').exists() else 'OPEN'
draft=not (root/'ready').exists()
merge_commit=(root/'merge-commit').read_text() if state=='MERGED' else None
row=dict(number=841,id='PR_disposable_adoption',url='https://github.com/'+c['repository']+'/pull/841',
 headRefName=c['branch'],headRefOid=head,baseRefName='main',isCrossRepository=False,isDraft=draft,
 author={'login':'autoship-canary'},title=c['title'],body=c['body'],closingIssuesReferences=[],state=state,
 mergeable='MERGEABLE',mergeCommit={'oid':merge_commit,'message':git('show','-s','--format=%B',merge_commit)} if state=='MERGED' else None,
 files=[{'path':p} for p in git('diff','--name-only',c['base'],head,'--').splitlines()])
with (root/'gh-calls.jsonl').open('a') as stream:stream.write(json.dumps(a)+'\n')
if a[:2]==['repo','view']: print(c['repository'])
elif a[:2]==['pr','list']: print(json.dumps([row] if state=='OPEN' else []))
elif a[:2]==['pr','view']:
 if '--jq' in a:
  expression=a[a.index('--jq')+1];assert expression=='.headRefOid';print(head)
 else: print(json.dumps(row))
elif a[:2]==['pr','checks']:
 print(json.dumps([{'name':name,'state':'SUCCESS' if (root/'ci-green').exists() else 'PENDING','bucket':'pass' if (root/'ci-green').exists() else 'pending'} for name in ['privacy','node-release']]))
elif a[:2]==['pr','ready']:
 assert (root/'ci-green').exists();(root/'ready').write_text(head);print('ready')
elif a[:2]==['api','graphql']:
 fields={}
 for i,value in enumerate(a):
  if value in ['-f','-F'] and i+1<len(a) and '=' in a[i+1]:
   key,val=a[i+1].split('=',1);fields[key]=val
 if fields.get('query','').startswith('query AutoshipDirectMergePolicy'):
  print(json.dumps({'data':{'repository':{'mergeQueue':None}}}));sys.exit(0)
 assert 'AutoshipMerge' in fields.get('query',''), 'only merge mutation is simulated'
 assert fields['head']==head and fields['id']==row['id']
 assert state=='OPEN' and not draft and (root/'ci-green').exists()
 subprocess.run(['/usr/bin/git','--git-dir',c['origin'],'merge-base','--is-ancestor','refs/heads/main',head],check=True)
 parent=git('rev-parse','refs/heads/main')
 merge_commit=subprocess.check_output(['/usr/bin/git','--git-dir',c['origin'],'-c','user.name=Autoship fixture','-c','user.email=fixture@example.invalid','commit-tree',git('rev-parse',head+'^{tree}'),'-p',parent],input=fields['body']+'\n',text=True).strip()
 git('update-ref','refs/heads/main',merge_commit,parent)
 (root/'merge-commit').write_text(merge_commit)
 (root/'merged').write_text(head)
 with (root/'merge-mutations.jsonl').open('a') as stream:stream.write(json.dumps(fields)+'\n')
 row.update(state='MERGED',mergedAt=__import__('datetime').datetime.now(__import__('datetime').timezone.utc).isoformat(),mergeCommit={'oid':merge_commit,'message':git('show','-s','--format=%B',merge_commit)},mergedBy={'login':'autoship-canary'})
 print(json.dumps({'data':{'mergePullRequest':{'clientMutationId':fields['request'],'pullRequest':row}}}))
elif a[:1]==['api'] and '/git/ref/heads/main' in a[1]:print(git('rev-parse','refs/heads/main'))
elif a[:1]==['api'] and '/branches/main/protection' in a[1]:
 print(json.dumps({'required_status_checks':{'strict':True,'checks':[{'context':'privacy','app_id':15368},{'context':'node-release','app_id':15368}]},'enforce_admins':{'enabled':True}}))
elif a[:1]==['api'] and ('/rules/branches/main' in a[1] or '/pulls/comments' in a[1]):print('[]')
else:
 print('unsupported synthetic GitHub call '+repr(a[:3]),file=sys.stderr);sys.exit(2)
"#;

#[tokio::test]
#[ignore = "paid native full source-less adoption; run only in isolated delegated service"]
async fn live_native_source_less_adoption_repairs_reviews_and_merges() -> Result<()> {
    const CHILD: &str = "AUTOSHIP_ADOPTION_ACCEPTANCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        anyhow::ensure!(
            std::env::var("AUGMENTAGENT_RUN_RESOURCES").as_deref() == Ok("1"),
            "probe requires explicit delegated resource policy"
        );
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::Builder::new()
            .prefix("autoship-source-less-acceptance-")
            .tempdir_in("/tmp")?
            .keep();
        let gh = root.join("github-fixture.py");
        std::fs::write(&gh, GH_FIXTURE)?;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700))?;
        println!(
            "Retained source-less acceptance fixture: {}",
            root.display()
        );
        let result=std::process::Command::new(std::env::current_exe()?)
            .args(["--exact","self_improve::adoption_acceptance::live_native_source_less_adoption_repairs_reviews_and_merges","--ignored","--nocapture"])
            .env(CHILD,&root).env("GH_BIN",gh)
            .env("XDG_STATE_HOME",root.join("state"))
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",root.join("state/augmentagent/workflow.db"))
            .env("AUGMENTAGENT_GATE_TARGET_DIR",root.join("gate"))
            .env("CARGO_TARGET_DIR",root.join("gate"))
            .env("AUGMENTAGENT_AUTOPR_REVIEWERS","codex")
            .env("AUGMENTAGENT_AUTOPR_AUTOMERGE","true")
            .env("AUGMENTAGENT_GH_OWNER","autoship-canary")
            .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","autoship-canary")
            .env("AUGMENTAGENT_CCAT_ENABLED","false")
            .output()?;
        std::fs::write(
            root.join("transcript.log"),
            [result.stdout.clone(), result.stderr.clone()].concat(),
        )?;
        anyhow::ensure!(
            result.status.success(),
            "source-less native pipeline failed; retained {}\n{}\n{}",
            root.display(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        anyhow::ensure!(
            root.join("result.json").exists(),
            "child acceptance did not complete"
        );
        return Ok(());
    }
    augmentagent_channel_core::model_router::native_reviewer_scope(Codex, run_pipeline()).await
}

async fn run_pipeline() -> Result<()> {
    const CHILD: &str = "AUTOSHIP_ADOPTION_ACCEPTANCE_CHILD";
    let root = PathBuf::from(std::env::var_os(CHILD).context("fixture root")?).canonicalize()?;
    anyhow::ensure!(
        root.starts_with("/tmp") && workflow::path().starts_with(&root),
        "fixture state is not isolated"
    );
    let repo = root.join("repository");
    let origin = root.join("origin.git");
    std::fs::create_dir(&repo)?;
    fixture_git(
        &root,
        &[
            "init",
            "--bare",
            "-q",
            origin.to_str().context("origin path")?,
        ],
    )
    .await?;
    fixture_git(&repo, &["init", "-q", "-b", "main"]).await?;
    fixture_git(&repo, &["config", "user.name", "Synthetic Adoption Probe"]).await?;
    fixture_git(&repo, &["config", "user.email", "probe@example.invalid"]).await?;
    fixture_git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().context("origin path")?,
        ],
    )
    .await?;
    std::fs::create_dir(repo.join("src"))?;
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname = \"autoship-disposable-adoption\"\nversion = \"0.0.0\"\nedition = \"2021\"\n")?;
    std::fs::write(repo.join("Cargo.lock"),"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"autoship-disposable-adoption\"\nversion = \"0.0.0\"\n")?;
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn fixture_identity() -> &'static str { \"synthetic\" }\n",
    )?;
    std::fs::write(
        repo.join(".gitignore"),
        "/target/\n/.self-improve-worktrees/\n**/__pycache__/\n",
    )?;
    std::fs::write(
        repo.join("arithmetic.py"),
        "def add(a, b):\n    return a + b - 2\n",
    )?;
    shared_ci::fixture::install(&repo)?;
    fixture_git(&repo, &["add", "."]).await?;
    fixture_git(
        &repo,
        &["commit", "-qm", "original red arithmetic behavior"],
    )
    .await?;
    let base = fixture_git(&repo, &["rev-parse", "HEAD"]).await?;
    fixture_git(&repo, &["push", "-u", "origin", "main"]).await?;
    let branch = "worktree/imported-arithmetic";
    fixture_git(&repo, &["checkout", "-b", branch]).await?;
    std::fs::write(
        repo.join("arithmetic.py"),
        "def add(a, b):\n    return a + b - 1\n",
    )?;
    fixture_git(&repo, &["add", "arithmetic.py"]).await?;
    fixture_git(&repo, &["commit", "-qm", "imported incomplete repair"]).await?;
    let imported = fixture_git(&repo, &["rev-parse", "HEAD"]).await?;
    fixture_git(&repo, &["push", "-u", "origin", branch]).await?;
    fixture_git(&repo, &["checkout", "main"]).await?;
    let repository = format!("autoship-canary/adopted-{}", uuid::Uuid::new_v4());
    let body="Adding two integer inputs returns their arithmetic sum. Repair arithmetic.py, including mixed-sign and zero cases. This imported PR is an incomplete repair. Preserve this criterion verbatim. Author an independent pinned Python behavior adapter with JSON output and expected results in a controller black_box oracle, not in child assertions. Rust files are unchanged scaffolding for the normal full gate, not requested work. No source issue exists.\n\n- complexity (scoping pass): simple";
    std::fs::write(
        root.join("fixture.json"),
        serde_json::to_vec(&serde_json::json!({"repository":repository,
        "origin":origin,"branch":branch,"base":base,"title":"Correct integer addition","body":body}))?,
    )?;
    let selected = adopted_execution::next(&repo, false)
        .await?
        .context("source-less selector did not adopt fixture")?;
    anyhow::ensure!(
        selected.pr == 841 && selected.source_issue.is_none() && selected.branch == branch,
        "wrong source-less adoption identity"
    );
    let id = selected.workflow_id()?;
    anyhow::ensure!(
        id == crate::autoship_workflow::pr_workflow_id(&repository, 841)?,
        "issue identity substituted for PR"
    );
    let builder =
        augmentagent_channel_core::build_pinned(Codex).context("native Codex unavailable")?;
    let report = shared_ci::fixture::scope(
        &repo, adopted_execution::resume(&repo, &builder, selected, false),
    ).await??;
    println!("Initial adopted execution: {}", report.message);
    let store = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?;
    let snapshot = store.snapshot(&id)?.context("adopted workflow missing")?;
    anyhow::ensure!(
        snapshot.checkpoint.next.action == "reconcile_approved_pr",
        "did not reach independently approved CI hold: {}",
        report.message
    );
    anyhow::ensure!(
        snapshot.checkpoint.binding.head != imported,
        "imported incomplete behavior was not repaired"
    );
    anyhow::ensure!(!root.join("merged").exists(), "pending CI permitted merge");
    let calls = snapshot.model_calls;
    anyhow::ensure!(calls > 0, "no actual native dispatch accounted");
    let verdict = store
        .current_reviews(&id)?
        .into_iter()
        .filter(|review| review.approved)
        .find_map(|review| serde_json::from_str::<RecordedVerdict>(&review.evidence).ok())
        .context("no protected independent approval")?;
    let evidence = verdict
        .structured
        .context("missing exact-head review evidence")?;
    evidence.authority()?;
    anyhow::ensure!(
        matches!(
            evidence.origin,
            crate::autoship_review::ReviewOrigin::Imported { .. }
        ),
        "unknown imported provenance was replaced"
    );
    let control = acceptance::control_directories(&repository, &id)?.workflow_acceptance;
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(control.join("pr-841-workflow.json"))?)?;
    anyhow::ensure!(
        manifest["pull_request"] == 841 && manifest.get("issue").is_none_or(|v| v.is_null()),
        "acceptance source fabricated an issue"
    );
    drop(store);
    std::fs::write(root.join("ci-green"), "controller fixture CI complete")?;
    // Advance only the fixture's scheduling deadline rather than sleeping five
    // minutes. Candidate, review, CI receipt and ownership evidence are untouched.
    rusqlite::Connection::open(workflow::path())?.execute(
        "UPDATE workflow SET checkpoint=json_set(checkpoint,'$.next.eligible_at_ms',0) WHERE id=?1",
        [&id],
    )?;
    let merged = adopted_execution::sweep(&repo, false).await?;
    anyhow::ensure!(
        merged == 1,
        "approved source-less PR did not merge in free reconciliation"
    );
    let store = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?;
    let after = store.snapshot(&id)?.context("workflow disappeared")?;
    anyhow::ensure!(
        after.model_calls == calls,
        "CI-only reconciliation rebilled native work"
    );
    verify_merge_attribution(&store, &id)?;
    anyhow::ensure!(
        std::fs::read_to_string(root.join("merge-mutations.jsonl"))?
            .lines()
            .count()
            == 1,
        "merge mutation repeated"
    );
    anyhow::ensure!(
        std::fs::read_to_string(root.join("merged"))? == after.checkpoint.binding.head,
        "merged head differs from approved head"
    );
    anyhow::ensure!(
        adopted_execution::sweep(&repo, false).await? == 0,
        "completed source-less merge replayed"
    );
    std::fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
        "ci_authority":"compiled disposable arithmetic fixture; not full Jarvis CI","full_jarvis_ci_pass":false,
        "native_models":true,"git":"real local bare origin","github":"simulated; not hosted-GitHub proof",
        "workflow":id,"source":"pull_request:841","imported_head":imported,"approved_head":after.checkpoint.binding.head,
        "model_calls_before_ci":calls,"model_calls_after_ci":after.model_calls,"merge_mutations":1,"passed":true}))?,
    )?;
    Ok(())
}


#[tokio::test]
async fn adoption_boundary_preserves_policy_and_immutable_merge_marker() -> Result<()> {
    let root = tempfile::tempdir()?;
    let repo = root.path().join("origin");
    std::fs::create_dir(&repo)?;
    fixture_git(&repo, &["init", "-b", "main"]).await?;
    fixture_git(&repo, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "fixture"]).await?;
    let before = fixture_git(&repo, &["rev-parse", "HEAD"]).await?;
    std::fs::write(root.path().join("fixture.json"), serde_json::json!({
        "origin":repo.join(".git"),"branch":"main","repository":"fixture/project",
        "base":before,"title":"fixture","body":"fixture"
    }).to_string())?;
    let script = root.path().join("github.py");
    std::fs::write(&script, GH_FIXTURE)?;
    let output = Command::new("/usr/bin/python3").arg(script)
        .args(["api", "graphql", "-f", "query=query AutoshipDirectMergePolicy($owner:String!,$repo:String!){repository(owner:$owner,name:$repo){mergeQueue(branch:\"main\"){id}}}", "-f", "owner=fixture", "-f", "repo=project"])
        .env("AUTOSHIP_ADOPTION_ACCEPTANCE_CHILD", root.path()).output().await?;
    anyhow::ensure!(output.status.success(), "read-only policy query failed: {}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    anyhow::ensure!(value.pointer("/data/repository/mergeQueue").is_some_and(serde_json::Value::is_null), "missing explicit direct-merge policy");
    anyhow::ensure!(!root.path().join("merged").exists() && !root.path().join("merge-mutations.jsonl").exists(), "policy query mutated merge state");
    anyhow::ensure!(fixture_git(&repo, &["rev-parse", "HEAD"]).await? == before, "policy query moved main");
    fixture_git(&repo, &["checkout", "-b", "repair"]).await?;
    fixture_git(&repo, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "repair"]).await?;
    let head=fixture_git(&repo, &["rev-parse", "HEAD"]).await?;
    let path=root.path().join("fixture.json");
    let mut config:serde_json::Value=serde_json::from_slice(&std::fs::read(&path)?)?;
    config["branch"]="repair".into();std::fs::write(path,serde_json::to_vec(&config)?)?;
    std::fs::write(root.path().join("ready"),"ready")?;
    std::fs::write(root.path().join("ci-green"),"green")?;
    let body="synthetic controller commit marker";
    let output=Command::new("/usr/bin/python3").arg(root.path().join("github.py"))
        .args(["api","graphql","-f","query=mutation AutoshipMerge","-f","id=PR_disposable_adoption","-f",&format!("head={head}"),"-f","request=fixture-request","-f",&format!("body={body}")])
        .env("AUTOSHIP_ADOPTION_ACCEPTANCE_CHILD",root.path()).output().await?;
    anyhow::ensure!(output.status.success(),"fixture merge failed");
    let response:serde_json::Value=serde_json::from_slice(&output.stdout)?;
    let commit=&response["data"]["mergePullRequest"]["pullRequest"]["mergeCommit"];
    anyhow::ensure!(commit["message"].as_str().is_some_and(|message|message.trim()==body),"fixture must return immutable merge message for attribution");
    let oid=commit["oid"].as_str().context("merge commit missing")?;
    anyhow::ensure!(fixture_git(&repo,&["show","-s","--format=%B",oid]).await?==body,"response marker must exist in real Git commit");
    anyhow::ensure!(fixture_git(&repo,&["rev-parse","main"]).await?==oid,"main must point at the fixture squash commit");
    anyhow::ensure!(fixture_git(&repo,&["rev-parse",&format!("{oid}^{{tree}}")]).await?==fixture_git(&repo,&["rev-parse",&format!("{head}^{{tree}}")]).await?,"squash must preserve approved candidate tree");
    Ok(())
}


fn verify_merge_attribution(store:&crate::autoship_workflow::WorkflowStore,id:&str)->Result<()> {
    let events=store.events(id)?;
    let confirmations:Vec<_>=events.iter().filter(|(_,kind,_,_)|kind=="autonomous_merge_confirmed").collect();
    anyhow::ensure!(confirmations.len()==1,"expected exactly one autonomous confirmation");
    let payload:serde_json::Value=serde_json::from_str(&confirmations[0].2)?;
    let operation=payload["operation"].as_str().context("confirmed operation missing")?;
    let receipt=store.confirmed_merge_receipt(operation)?.context("confirmed durable receipt missing")?;
    store.validate_confirmed_merge_proof(operation,&receipt)?;
    let at=chrono::DateTime::parse_from_rfc3339(receipt["merged_at"].as_str().context("merge time missing")?)?.timestamp_millis();
    let allowance=store.merge_allowance(at,5)?;
    anyhow::ensure!(allowance.confirmed==1 && allowance.pending==0 && allowance.remaining==4 && allowance.available==4,"wrong confirmed allowance or unresolved hold");
    Ok(())
}

#[tokio::test]
#[ignore = "verify final assertions against an explicit retained disposable native adoption run; no model calls"]
async fn verify_retained_adoption_completion() -> Result<()> {
    let root=PathBuf::from(std::env::var_os("AUTOSHIP_ADOPTION_COMPLETION_ROOT").context("retained fixture root required")?).canonicalize()?;
    anyhow::ensure!(root.starts_with("/tmp") && root.file_name().unwrap().to_string_lossy().starts_with("autoship-source-less-acceptance-"),"not a retained adoption fixture");
    let script=root.join("github-fixture.py");
    anyhow::ensure!(std::fs::read_to_string(&script)?==GH_FIXTURE,"retained GitHub boundary differs from current fixture");
    let config:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("fixture.json"))?)?;
    let repository=config["repository"].as_str().context("repository missing")?;
    let origin=PathBuf::from(config["origin"].as_str().context("origin missing")?).canonicalize()?;
    anyhow::ensure!(origin.starts_with(&root),"fixture origin escaped retained root");
    let path=root.join("state/augmentagent/workflow.db");
    anyhow::ensure!(workflow::path()==path,"completion replay must use retained workflow state");
    anyhow::ensure!(PathBuf::from(std::env::var_os("GH_BIN").context("fixture GitHub boundary required")?).canonicalize()?==script.canonicalize()?,"completion replay must use retained GitHub fixture");
    anyhow::ensure!(PathBuf::from(std::env::var_os("AUTOSHIP_ADOPTION_ACCEPTANCE_CHILD").context("fixture environment required")?).canonicalize()?==root,"completion fixture environment differs");
    let id=crate::autoship_workflow::pr_workflow_id(repository,841)?;
    let store=crate::autoship_workflow::WorkflowStore::open_read_only(&path)?.context("retained workflow missing")?;
    let before=store.snapshot(&id)?.context("retained snapshot missing")?;
    anyhow::ensure!(before.checkpoint.stage==crate::autoship_workflow::Stage::Merged,"workflow not merged");
    verify_merge_attribution(&store,&id)?;
    let mutations=std::fs::read(root.join("merge-mutations.jsonl"))?;
    anyhow::ensure!(mutations.split(|b|*b==b'\n').filter(|line|!line.is_empty()).count()==1,"mutation repeated");
    anyhow::ensure!(std::fs::read_to_string(root.join("merged"))?==before.checkpoint.binding.head,"merged candidate differs");
    let repo=root.join("repository");
    anyhow::ensure!(repo.canonicalize()?.starts_with(&root),"fixture repository escaped root");
    anyhow::ensure!(adopted_execution::sweep(&repo,false).await?==0,"completed merge replayed");
    anyhow::ensure!(std::fs::read(root.join("merge-mutations.jsonl"))?==mutations,"replay changed mutation history");
    let after=store.snapshot(&id)?.context("workflow disappeared")?;
    anyhow::ensure!(after.model_calls==before.model_calls,"reconciliation spent model calls");
    verify_merge_attribution(&store,&id)?;
    std::fs::write(root.join("completion-verification.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "passed":true,"workflow":id,"head":after.checkpoint.binding.head,"model_calls":after.model_calls,
        "confirmed_merges":1,"pending_merges":0,"remaining":4,"replay_mutations":0,
        "scope":"terminal assertions on retained real-native/local-Git/scripted-GitHub run; not production or fresh full rerun"
    }))?)?;
    Ok(())
}
