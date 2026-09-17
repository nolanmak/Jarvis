//! Controlled production pipeline: real providers/builds, local Git and fake
//! GitHub transport. No public issues, branches or notifications are created.
use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
#[ignore = "requires Codex login and build VM; executes the synthetic auto-ship pipeline"]
async fn live_fallback_pipeline_preserves_draft_without_independent_capacity() {
    pipeline(false).await;
}

#[tokio::test]
#[ignore = "requires Codex/Claude login and build VM; synthetic draft, recovery, merge and checkout QA"]
async fn live_fallback_pipeline_resumes_after_independent_reviewer_recovers() {
    pipeline(true).await;
}

async fn pipeline(recover: bool) {
    const CHILD: &str = "JARVIS_LIFECYCLE_ROOT";
    if std::env::var_os(CHILD).is_none() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let gh = root.join("fixture-gh.py");
        std::fs::write(&gh, r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
root = pathlib.Path(os.environ['JARVIS_LIFECYCLE_ROOT'])
args = sys.argv[1:]
with (root/'gh-calls.jsonl').open('a') as log:
    log.write(json.dumps(args)+'\n')
if args[0]=='api' and '/issues?' in args[1]:
    issues=[{'number':700001,'title':'Correct integer addition','body':
        'The public add function subtracts its second argument. Make it return the sum for positive, zero and negative integers whose sum is in range. Add regression coverage before fixing the function. No dependency changes are needed.',
        'state':'open','user':{'login':'synthetic-owner'},'author_association':'OWNER','labels':[]}]
    (root/'issue.json').write_text(json.dumps(issues[0]))
    print(json.dumps(issues))
elif args[0]=='api' and args[1].endswith('/issues/700001'):
    print((root/'issue.json').read_text())
elif args[0]=='api' and args[1].endswith('/issues/1/comments?per_page=100'):
    print('[]')
elif args[:2]==['pr','list']:
    print('[]')
elif args[:2]==['pr','create']:
    (root/'created-pr.json').write_text(json.dumps(args))
    print('https://example.invalid/synthetic/project/pull/1')
elif args[:2]==['pr','view']:
    original=json.loads((root/'created-pr.json').read_text())
    print(json.dumps({'body':original[original.index('--body')+1]}))
elif args[:2]==['pr','merge']:
    remote=root/'remote.git'
    def git(*argv,**kw):
        return subprocess.check_output(['git','--git-dir='+str(remote),*argv],text=True,**kw).strip()
    before=git('rev-parse','main')
    tree=git('rev-parse','agent-fix/issue-700001^{tree}')
    identity=dict(os.environ,GIT_AUTHOR_NAME='Synthetic',GIT_AUTHOR_EMAIL='fixture@example.com',
        GIT_COMMITTER_NAME='Synthetic',GIT_COMMITTER_EMAIL='fixture@example.com')
    merged=git('commit-tree',tree,'-p',before,input='Synthetic accepted merge\n',env=identity)
    git('update-ref','refs/heads/main',merged,before)
    git('update-ref','-d','refs/heads/agent-fix/issue-700001')
    (root/'merged-sha').write_text(merged)
    print('synthetic merge complete')
elif args[:2] in (['issue','comment'],['issue','edit'],['pr','comment'],['pr','ready']):
    print('fixture operation recorded')
else:
    print('unsupported fixture gh operation: '+json.dumps(args),file=sys.stderr)
    sys.exit(1)
"#).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        let name = if recover { "self_improve::lifecycle_tests::live_fallback_pipeline_resumes_after_independent_reviewer_recovers" }
            else { "self_improve::lifecycle_tests::live_fallback_pipeline_preserves_draft_without_independent_capacity" };
        child.args(["--exact", name, "--ignored", "--nocapture"])
            .env(CHILD, root).env("GH_BIN", &gh)
            .env("AUGMENTAGENT_REASONER_CHAIN", "claude,codex")
            .env("AUGMENTAGENT_GH_OWNER", "synthetic-owner")
            .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "synthetic-owner")
            .env("AUGMENTAGENT_AUTOPR_LANE", "build")
            .env("AUGMENTAGENT_AUTOPR_AUTOMERGE", "1")
            .env("AUGMENTAGENT_GIT_AUTHOR_NAME", "Synthetic")
            .env("AUGMENTAGENT_GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("AUGMENTAGENT_GATE_TARGET_DIR", root.join("target"))
            .env("CARGO_TARGET_DIR", root.join("target"))
            // #1048: handoff journals, review history and the provider logs
            // resolve under the fixture, never the owner's live state dir.
            .env("XDG_STATE_HOME", root.join("state"))
            .env_remove("AUGMENTAGENT_TOOL_AUDIT_LOG")
            .env_remove("AUGMENTAGENT_TOKEN_USAGE_LOG")
            .env_remove("DISCORD_WEBHOOK_URL");
        for (key, file) in [
            ("AUGMENTAGENT_SELFIMPROVE_LOCK", "pipeline.lock"),
            ("AUGMENTAGENT_COOLDOWN_FILE", "cooldown.json"),
            ("AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE", "attempted.json"),
            ("AUGMENTAGENT_AUTOPR_HISTORY_FILE", "history.json"),
            ("AUGMENTAGENT_AUTOPR_BASELINE_FILE", "baseline.json"),
            ("AUGMENTAGENT_AUTOPR_COUNTER_FILE", "counter.json"),
            ("AUGMENTAGENT_AUTOPR_OPENED_FILE", "opened-prs.json"),
            ("AUGMENTAGENT_AUTOPR_UNREVIEWABLE_FILE", "unreviewable.json"),
        ] { child.env(key, root.join(file)); }
        let result = child.output().unwrap();
        assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    assert!(augmentagent_channel_core::state_dir::isolate_for_tests().starts_with(&root), "lifecycle state must stay in the fixture");
    let repo = root.join("repo");
    let remote = root.join("remote.git");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let output = std::process::Command::new("git").current_dir(cwd).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    git(&root, &["init", "--bare", "--initial-branch=main", remote.to_str().unwrap()]);
    git(&repo, &["init", "--initial-branch=main"]);
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"synthetic-lifecycle\"\nversion=\"0.1.0\"\nedition=\"2021\"\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn add(a:i32,b:i32)->i32 { a-b }\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/target/\n/.self-improve-worktrees/\n").unwrap();
    let lockfile = std::process::Command::new("cargo").args(["generate-lockfile", "--offline"])
        .current_dir(&repo).output().unwrap();
    assert!(lockfile.status.success(), "{}", String::from_utf8_lossy(&lockfile.stderr));
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.name=Synthetic", "-c", "user.email=fixture@example.com", "commit", "-qm", "Synthetic baseline"]);
    git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&repo, &["push", "-u", "origin", "main"]);
    let baseline = git(&repo, &["rev-parse", "HEAD"]);
    augmentagent_channel_core::CooldownLatch::system().latch("claude",
        chrono::Utc::now()+chrono::Duration::minutes(30), "synthetic unavailable primary");
    let report = run_once(&repo, false).await.unwrap();
    assert!(report.message.contains("draft PR opened"), "{}\nfixture transport: {}\nattempt history: {}", report.message,
        std::fs::read_to_string(root.join("gh-calls.jsonl")).unwrap_or_default(),
        std::fs::read_to_string(root.join("history.json")).unwrap_or_default());
    let pr: Vec<String> = serde_json::from_slice(&std::fs::read(root.join("created-pr.json")).unwrap()).unwrap();
    assert!(pr.iter().any(|arg| arg=="--draft"), "missing reviewer must prevent merge");
    let body = &pr[pr.iter().position(|arg| arg=="--body").unwrap()+1];
    assert!(body.contains("unavailable"), "draft must explain missing independent capacity: {body}");
    let calls: Vec<serde_json::Value> = std::fs::read_to_string(root.join("gh-calls.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(!calls.iter().any(|args| args[0]=="pr" && args[1]=="merge"));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), baseline);
    assert_eq!(git(&remote, &["rev-parse", "main"]), baseline);
    assert!(git(&remote, &["rev-parse", "refs/heads/agent-fix/issue-700001"]).len()>10);
    assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).lines().filter(|line| line.starts_with("worktree ")).count(),1);
    assert!(git(&repo, &["status", "--porcelain"]).trim().is_empty());
    // Resume while the independent provider is still unavailable: no
    // revision may run, and temporary capacity loss must not retire the PR.
    let before_wait = git(&remote, &["rev-parse", "refs/heads/agent-fix/issue-700001"]);
    let history_before_wait = std::fs::read(root.join("history.json")).ok();
    let unavailable = augmentagent_channel_core::build_reasoner();
    let waiting = resume_draft_pr(&repo, &unavailable, 1, 700001, "agent-fix/issue-700001", false).await.unwrap();
    // #1037 — held, unbilled, and named for what it is: the one independent
    // reviewer is latched, which is neither missing capacity nor provenance.
    assert!(waiting.message.contains("independent review unavailable")
        && waiting.message.contains("still draft"), "{}", waiting.message);
    assert!(!waiting.billed, "no builder ran, so no daily-cap slot: {}", waiting.message);
    assert!(unavailable.mutation_providers().is_empty());
    assert_eq!(std::fs::read(root.join("history.json")).ok(), history_before_wait);
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/agent-fix/issue-700001"]), before_wait);
    let calls = std::fs::read_to_string(root.join("gh-calls.jsonl")).unwrap();
    assert!(calls.contains("reviewer latched until"), "{calls}");
    assert!(!calls.contains("provenance"), "{calls}");
    assert!(!calls.contains("review round"), "capacity loss must not spend revision rounds");
    assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).lines().filter(|line| line.starts_with("worktree ")).count(),1);
    if recover {
        // Only the fixture's latch is cleared; production quota state and
        // provider history stay intact. The fresh reasoner reloads authors.
        augmentagent_channel_core::CooldownLatch::system().clear("claude");
        let reasoner = augmentagent_channel_core::build_reasoner();
        let resumed = resume_draft_pr(&repo, &reasoner, 1, 700001, "agent-fix/issue-700001", false).await.unwrap();
        assert!(resumed.message.contains("resumed and MERGED"), "{}\n{}", resumed.message,
            std::fs::read_to_string(root.join("gh-calls.jsonl")).unwrap_or_default());
        let merged = std::fs::read_to_string(root.join("merged-sha")).unwrap();
        assert_ne!(merged, baseline.trim());
        assert_eq!(git(&remote, &["rev-parse", "main"]).trim(), merged);
        let deployed = root.join("deployed");
        git(&root, &["clone", remote.to_str().unwrap(), deployed.to_str().unwrap()]);
        assert_eq!(git(&deployed, &["rev-parse", "HEAD"]).trim(), merged);
        std::fs::create_dir_all(deployed.join("tests")).unwrap();
        std::fs::write(deployed.join("tests/acceptance.rs"),
            "#[test] fn accepted_behavior(){for (a,b) in [(2,3),(0,0),(-2,-3),(-4,7),(i32::MAX,-1),(i32::MIN,1)] {assert_eq!(synthetic_lifecycle::add(a,b),a+b);}}\n").unwrap();
        let qa = std::process::Command::new("cargo").args(["test", "--offline"])
            .current_dir(&deployed).output().unwrap();
        assert!(qa.status.success(), "{}{}", String::from_utf8_lossy(&qa.stdout), String::from_utf8_lossy(&qa.stderr));
        assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).lines().filter(|line| line.starts_with("worktree ")).count(),1);
    }
}
