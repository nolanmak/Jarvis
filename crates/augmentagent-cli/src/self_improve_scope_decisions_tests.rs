use super::*;
fn assessment() -> Assessment {
    let mut slots = SLOTS
        .into_iter()
        .map(|slot| {
            (
                slot.into(),
                Slot::Resolved {
                    decision: "Preserve existing behavior".into(),
                    evidence: vec![Evidence {
                        path: None,
                        quote: "synthetic request".into(),
                    }],
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    slots.insert(
        "rollout".into(),
        Slot::ExternalDecision {
            question: "Is the optional account qualified?".into(),
            alternatives: vec![
                Alternative::Classified(Choice {
                    decision: "Use existing local path; optional backend disabled".into(),
                    effects: vec![],
                    reversible: true,
                    default_off: true,
                    new_account_dependency: false,
                }),
                Alternative::Classified(Choice {
                    decision: "Enable paid external backend".into(),
                    effects: vec![Effect::Spend, Effect::Credentials],
                    reversible: false,
                    default_off: false,
                    new_account_dependency: true,
                }),
            ],
            evidence: vec![Evidence {
                path: None,
                quote: "synthetic request".into(),
            }],
        },
    );
    Assessment { slots }
}
#[test]
fn only_all_sensitive_alternatives_escalate() {
    let mut a = assessment();
    let Slot::ExternalDecision { alternatives, .. } = a.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    alternatives.remove(0);
    alternatives.push(Alternative::Classified(Choice {
        decision: "Publish sensitive records".into(),
        effects: vec![Effect::SecurityPrivacy],
        reversible: false,
        default_off: false,
        new_account_dependency: false,
    }));
    // #2289: the owner is told, but the flagged-off parts still build.
    let built = assumptions(&a, false).unwrap();
    assert_eq!(built.len(), 1, "an escalation must not park the workflow");
    let escalation = built[0].escalation.as_deref().unwrap();
    assert!(escalation.contains("spend") && escalation.contains("credentials"));
    assert!(built[0].decision.contains("default-off flag"));
    assert!(built[0].decision.contains("Enable paid external backend"));
    assert!(built[0].decision.contains("Publish sensitive records"));
    assert_eq!(
        super::super::validate(&a, "synthetic request", &BTreeMap::new())
            .unwrap()
            .len(),
        1,
        "the escalation is still reported to the owner"
    );
}
#[test]
fn missing_decision_proceeds_with_the_most_conservative_recorded_assumption() {
    let mut a = assessment();
    let Slot::ExternalDecision { alternatives, .. } = a.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    // Unclassified legacy output still proceeds with a conservative pick.
    *alternatives = vec![
        "Enable the hosted service with a new paid account and API key".into(),
        "Keep the existing local path with the integration disabled by default".into(),
    ];
    let picked = assumptions(&a, false).unwrap();
    assert_eq!(picked.len(), 1);
    assert!(picked[0].decision.starts_with("Keep the existing local path"));
    assert!(picked[0].escalation.is_none());
    // A routine choice that is not default-off no longer fails validation;
    // the most conservative routine option wins and is constrained.
    let Slot::ExternalDecision { alternatives, .. } = a.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    *alternatives = vec![
        Alternative::Classified(Choice {
            decision: "Turn the new behavior on for everyone".into(),
            effects: vec![],
            reversible: false,
            default_off: false,
            new_account_dependency: false,
        }),
        Alternative::Classified(Choice {
            decision: "Ship it behind a disabled flag".into(),
            effects: vec![],
            reversible: true,
            default_off: true,
            new_account_dependency: false,
        }),
    ];
    let picked = assumptions(&a, false).unwrap();
    assert_eq!(picked[0].decision, "Ship it behind a disabled flag");
    assert!(super::super::validate(&a, "synthetic request", &BTreeMap::new())
        .unwrap()
        .is_empty());
    let Slot::ExternalDecision { alternatives, .. } = a.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    alternatives.remove(1);
    alternatives.push(Alternative::Classified(Choice {
        decision: "Buy the hosted plan".into(),
        effects: vec![Effect::Spend],
        reversible: true,
        default_off: false,
        new_account_dependency: true,
    }));
    let picked = assumptions(&a, false).unwrap();
    assert_eq!(picked[0].decision, "Turn the new behavior on for everyone");
    assert_eq!(picked[0].constraints, CONSTRAINTS, "constraints keep it reversible and default-off");
}
#[test]
fn unknown_or_unsafe_routine_classification_is_a_validator_error() {
    let mut a = assessment();
    let Slot::ExternalDecision { alternatives, .. } = a.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    let Alternative::Classified(c) = &mut alternatives[0] else {
        unreachable!()
    };
    c.new_account_dependency = true;
    assert!(require_classified(&a).is_err());
    let mut a = assessment();
    let Slot::ExternalDecision { alternatives, .. } = a.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    *alternatives = vec![
        "unknown account choice".into(),
        "another unknown choice".into(),
    ];
    assert!(
        require_classified(&a).is_err(),
        "legacy ambiguous output requires repair, not owner escalation"
    );
}
#[tokio::test]
async fn assumption_is_recorded_and_visible_once_even_after_uncertain_comment_write() {
    const CHILD: &str = "JARVIS_SCOPE_ASSUMPTION_ROOT";
    const TEST:&str="self_improve::acceptance::readiness::decisions::tests::assumption_is_recorded_and_visible_once_even_after_uncertain_comment_write";
    if std::env::var_os(CHILD).is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let gh = root.join("gh.py");
        augmentagent_test_support::write_executable(
            &gh,
            r#"#!/usr/bin/python3
import json,os,pathlib,sys
r=pathlib.Path(os.environ['JARVIS_SCOPE_ASSUMPTION_ROOT']);a=sys.argv[1:];p=r/'comments.json'
rows=json.loads(p.read_text()) if p.exists() else []
if a[:1]==['api'] and '/comments?' in a[1]:print(json.dumps(rows))
elif a[:2]==['issue','comment']:
 body=a[a.index('--body')+1];rows.append({'body':body});p.write_text(json.dumps(rows))
 if (r/'fail-after-post').exists():sys.exit(1)
else:raise AssertionError(a)
"#,
        )
        .unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env(CHILD, &root)
            .env("GH_BIN", gh)
            .env(
                "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
                root.join("state/workflow.db"),
            )
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    git(&["init", "-q", "-b", "agent-fix/issue-7"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/fixture-owner/repo.git",
    ]);
    std::fs::write(repo.join("fixture.txt"), "synthetic request\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "synthetic baseline",
    ]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let path = workflow::path();
    let guard = workflow::WorkflowGuard::acquire_fixture(
        &path,
        workflow::Start {
            repo_root: &repo,
            worktree: &repo,
            repository: "fixture-owner/repo",
            source_issue: Some(7),
            pr: None,
            branch: "agent-fix/issue-7",
            contract: "synthetic request",
            environment: "synthetic environment",
        },
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::write(root.join("fail-after-post"), "").unwrap();
    assert!(
        record(&repo, AcceptanceSource::Issue(7), &guard, &assessment())
            .await
            .is_err()
    );
    std::fs::remove_file(root.join("fail-after-post")).unwrap();
    assert_eq!(
        record(&repo, AcceptanceSource::Issue(7), &guard, &assessment())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        record(&repo, AcceptanceSource::Issue(7), &guard, &assessment())
            .await
            .unwrap()
            .len(),
        1
    );
    let comments: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(root.join("comments.json")).unwrap()).unwrap();
    assert_eq!(comments.len(), 1);
    assert!(comments[0]["body"]
        .as_str()
        .unwrap()
        .starts_with("Assumed decision"));
    assert!(!comments[0]["body"].as_str().unwrap().contains("Owner decision needed"));
    // #2289: an all-sensitive decision is escalated in the same visible
    // record, and the flagged-off parts still proceed.
    let mut sensitive = assessment();
    let Slot::ExternalDecision { alternatives, .. } = sensitive.slots.get_mut("rollout").unwrap() else {
        unreachable!()
    };
    alternatives.remove(0);
    alternatives.push(Alternative::Classified(Choice {
        decision: "Rotate production credentials".into(),
        effects: vec![Effect::Credentials],
        reversible: false,
        default_off: false,
        new_account_dependency: false,
    }));
    let escalated = record(&repo, AcceptanceSource::Issue(7), &guard, &sensitive)
        .await
        .unwrap();
    assert_eq!(escalated.len(), 1);
    let comments: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(root.join("comments.json")).unwrap()).unwrap();
    assert_eq!(comments.len(), 2);
    let body = comments[1]["body"].as_str().unwrap();
    assert!(body.starts_with("Assumed decision"));
    assert!(body.contains("Owner decision needed") && body.contains("credentials"));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let count:i64=conn.query_row("SELECT COUNT(*) FROM scope_stage_response WHERE json_extract(response,'$.kind')='assumed_decision'",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 2);
    assert_eq!(guard.binding().head.len(), 40);
    guard.finish().unwrap();
}
