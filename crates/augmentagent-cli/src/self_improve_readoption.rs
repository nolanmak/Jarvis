//! #1531 — explicit owner readoption of a legacy automatic hold.
//!
//! The legacy sweep only lifts automatic holds it can prove obsolete. Any
//! other automatic hold (#1550/#1595/#1596) stays until the owner readopts that
//! exact issue here. Readoption records new authenticated owner authority,
//! keeps the original hold history on GitHub, and only returns the issue to
//! the pool: the next workflow still passes fresh scope and review. It never
//! lifts a manual owner hold, an owner pause or a security verdict, and it
//! never readmits by reason across issues.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract::{Actor, Origin, WorkflowStage};
use serde_json::{json, Value};

#[derive(clap::Args)]
pub(crate) struct Args {
    /// OWNER/REPO of the current checkout.
    #[arg(long, required_unless_present = "list")]
    repo: Option<String>,
    #[arg(long, conflicts_with = "pr", required_unless_present_any = ["pr", "list"])]
    issue: Option<u64>,
    /// PR readoption ships with explicit PR adoption (#1537).
    #[arg(long)]
    pr: Option<u64>,
    /// Why the owner readopts this target; recorded with the authority.
    #[arg(long, required_unless_present = "list")]
    reason: Option<String>,
    /// Caller-chosen idempotency key; a retry with it resumes, never repeats.
    #[arg(long = "request-id", required_unless_present = "list")]
    request_id: Option<String>,
    /// List held issues the legacy sweep could not lift, with their action.
    #[arg(long, conflicts_with_all = ["repo", "issue", "pr", "reason", "request_id"])]
    list: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug)]
pub(crate) struct Refusal {
    code: i32,
    reason: &'static str,
    detail: String,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.detail)
    }
}
impl std::error::Error for Refusal {}
impl Refusal {
    /// Bounded, stable reason code for adapters (Discord) that echo it.
    pub(crate) fn reason(&self) -> &'static str { self.reason }
}
fn refuse(code: i32, reason: &'static str, detail: impl Into<String>) -> anyhow::Error {
    Refusal { code, reason, detail: detail.into() }.into()
}

const TABLE: &str = "CREATE TABLE IF NOT EXISTS owner_readoption(
    request_id TEXT PRIMARY KEY, repository TEXT NOT NULL, target TEXT NOT NULL,
    number INTEGER NOT NULL, reason TEXT NOT NULL, principal TEXT NOT NULL,
    prior_hold TEXT NOT NULL, status TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL)";

#[derive(Debug)]
pub(crate) struct Request {
    pub repository: String,
    pub issue: u64,
    pub reason: String,
    pub request_id: String,
}

fn validate(args: &Args) -> Result<Request> {
    if let Some(pr) = args.pr {
        return Err(refuse(2, "pr-readoption-unsupported", format!("PR #{pr}: PR readoption ships with explicit PR adoption (#1537)")));
    }
    let repository = crate::autoship_workflow::submission::normalized_repository(args.repo.as_deref().unwrap_or_default())
        .map_err(|e| refuse(2, "invalid-request", format!("{e:#}")))?
        .to_ascii_lowercase();
    let issue = args.issue.filter(|n| *n > 0).ok_or_else(|| refuse(2, "invalid-request", "positive --issue required"))?;
    let reason = args.reason.as_deref().unwrap_or_default().trim().to_owned();
    if reason.is_empty() || reason.len() > 500 || reason.chars().any(char::is_control) {
        return Err(refuse(2, "invalid-request", "--reason must be 1-500 printable characters"));
    }
    let request_id = args.request_id.clone().unwrap_or_default();
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    {
        return Err(refuse(2, "invalid-request", "--request-id must be 1-128 of [A-Za-z0-9_.:-]"));
    }
    Ok(Request { repository, issue, reason, request_id })
}

fn output(request: &Request, principal: &str, prior_hold: &Value, replayed: bool) -> Value {
    json!({
        "schema_version": 1,
        "accepted": true,
        "replayed": replayed,
        "repository": request.repository,
        "target": {"kind": "issue", "number": request.issue},
        "request_id": request.request_id,
        "principal": principal,
        "prior_hold": prior_hold,
        "actor": Actor::Owner,
        "origin": Origin::Cli,
        "stage": WorkflowStage::Intake,
        "reason_code": "owner_readopted",
        "next_action": "fresh_scope_and_review",
        "hold_history": "preserved",
        "execution": "not-dispatched",
    })
}

fn db(path: &Path) -> Result<rusqlite::Connection> {
    // Create the protected store with its permissions before raw access.
    drop(crate::autoship_workflow::WorkflowStore::open(path)?);
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(TABLE)?;
    Ok(conn)
}

type Recorded = (String, String, u64, String, String, String, String);
fn recorded(conn: &rusqlite::Connection, request_id: &str) -> Result<Option<Recorded>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT repository,target,number,reason,principal,prior_hold,status FROM owner_readoption WHERE request_id=?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()?)
}

fn applied(conn: &rusqlite::Connection, request: &Request) -> Result<()> {
    conn.execute(
        "UPDATE owner_readoption SET status='applied',updated_at_ms=?2 WHERE request_id=?1",
        rusqlite::params![request.request_id, chrono::Utc::now().timestamp_millis()],
    )?;
    Ok(())
}

pub(crate) async fn apply(repo: &Path, request: &Request) -> Result<Value> {
    // Authenticate before replaying anything: a revoked grant replays nothing.
    let principal = targeted::authorize_repository(repo, &request.repository)
        .await
        .map_err(|e| refuse(2, "target-authorization-failed", format!("{e:#}")))?;
    let controller = principal.strip_prefix("github:").unwrap_or_default().to_owned();
    let path = workflow::path();
    let conn = db(&path).map_err(|e| refuse(4, "workflow-state-unavailable", format!("{e:#}")))?;
    let prior = recorded(&conn, &request.request_id)?;
    if let Some((repository, target, number, reason, principal, hold, status)) = &prior {
        if (repository.as_str(), target.as_str(), *number, reason.as_str())
            != (request.repository.as_str(), "issue", request.issue, request.reason.as_str())
        {
            return Err(refuse(2, "request-id-conflict", "request id already records a different readoption"));
        }
        if status == "applied" {
            return Ok(output(request, principal, &serde_json::from_str(hold)?, true));
        }
    }
    let number = request.issue;
    let controls = workflow::FreshIssueControls::open()
        .map_err(|e| refuse(4, "workflow-state-unavailable", format!("{e:#}")))?;
    if controls
        .paused(&request.repository, number)
        .map_err(|e| refuse(4, "workflow-state-unavailable", format!("{e:#}")))?
    {
        return Err(refuse(3, "explicit-owner-hold", "the workflow is owner-paused; resume it instead"));
    }
    let (ok, raw, error) = run(&gh_bin(), &["api", &format!("repos/{}/issues/{number}", request.repository)], repo).await?;
    if !ok {
        return Err(refuse(4, "target-unavailable", truncate(&error, 200)));
    }
    let issue: Value = serde_json::from_str(&raw)?;
    if issue["number"].as_u64() != Some(number) || issue.get("pull_request").is_some_and(|v| !v.is_null()) {
        return Err(refuse(2, "invalid-target", "not an issue of this repository"));
    }
    if issue["state"] != "open" {
        return Err(refuse(3, "target-not-open", "closed issues are reconciled, not readopted"));
    }
    if is_issue_blast_radius(&format!("{}\n{}", issue["title"].as_str().unwrap_or(""), issue["body"].as_str().unwrap_or(""))) {
        return Err(refuse(3, "security-hold", "security-sensitive issues keep their hold"));
    }
    if !has_gave_up_label(&issue) {
        // A retry after the label write landed completes the same request.
        if let Some((_, _, _, _, principal, hold, _)) = &prior {
            applied(&conn, request)?;
            return Ok(output(request, principal, &serde_json::from_str(hold)?, true));
        }
        return Err(refuse(3, "no-automatic-hold", format!("#{number} carries no `{GAVE_UP_LABEL}` hold")));
    }
    let events = legacy_readmission::timeline(repo, &request.repository, number)
        .await
        .map_err(|e| refuse(4, "hold-history-unavailable", format!("{e:#}")))?;
    let Some(hold) = legacy_readmission::readoptable_hold(&events, &controller) else {
        return Err(refuse(3, "protected-hold", "the hold is an explicit owner hold or a security verdict"));
    };
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO owner_readoption VALUES(?1,?2,'issue',?3,?4,?5,?6,'recorded',?7,?7) ON CONFLICT(request_id) DO NOTHING",
        rusqlite::params![request.request_id, request.repository, number, request.reason, principal, serde_json::to_string(&hold)?, now],
    )?;
    let (ok, _, error) = run(&gh_bin(), &["issue", "edit", &number.to_string(), "--remove-label", GAVE_UP_LABEL], repo).await?;
    if !ok {
        // Authority stays recorded; the same request id retries the write.
        return Err(refuse(4, "label-removal-failed", truncate(&error, 200)));
    }
    applied(&conn, request)?;
    legacy_readmission::note_readoption(&path, &request.repository, number, None)?;
    info!(issue = number, principal = %principal, "owner readopted legacy automatic hold; fresh scope and review follow");
    Ok(output(request, &principal, &hold, false))
}

fn list() -> Result<Value> {
    let rows = legacy_readmission::readoption_required(&workflow::path())?;
    Ok(json!({
        "schema_version": 1,
        "readoption_required": rows.into_iter().map(|(repository, issue, hold, action)| json!({
            "repository": repository, "issue": issue, "hold_reason": hold, "action": action,
        })).collect::<Vec<_>>(),
    }))
}

pub(crate) async fn run_command(args: &Args) -> i32 {
    let result = if args.list {
        list().map_err(|e| refuse(4, "workflow-state-unavailable", format!("{e:#}")))
    } else {
        match (validate(args), std::env::current_dir()) {
            (Ok(request), Ok(repo)) => apply(&repo, &request).await,
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e.into()),
        }
    };
    let (code, value) = match result {
        Ok(value) => (0, value),
        Err(error) => {
            let (code, reason) = error
                .downcast_ref::<Refusal>()
                .map_or((4, "workflow-state-unavailable"), |r| (r.code, r.reason));
            (code, json!({"schema_version": 1, "accepted": false, "reason": reason, "error": format!("{error:#}")}))
        }
    };
    if args.json {
        println!("{value}");
    } else {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD: &str = "AUTOSHIP_READOPTION_FIXTURE";
    const GH: &str = r#"#!/usr/bin/env python3
import json,os,pathlib,sys
r=pathlib.Path(os.environ['AUTOSHIP_READOPTION_FIXTURE']);a=sys.argv[1:]
with (r/'calls').open('a') as f:f.write(json.dumps(a)+'\n')
if a[:2]==['repo','view'] and 'viewerPermission' in a: print((r/'permission').read_text())
elif a[:2]==['repo','view']: print('Controller/Repo')
elif a[:2]==['api','graphql']: print(json.dumps({'data':{'viewer':{'login':'controller'}}}))
elif a[:1]==['api'] and '/timeline?' in a[1]: print((r/'timeline.json').read_text())
elif a[:1]==['api'] and '/issues/' in a[1]: print((r/'issue.json').read_text())
elif a[:2]==['issue','edit']:
 assert a[3:]==['--remove-label','agent-gave-up']
 if (r/'fail-edit').exists(): sys.exit(1)
 with (r/'edits').open('a') as f:f.write(a[2]+'\n')
 i=json.loads((r/'issue.json').read_text());i['labels']=[l for l in i['labels'] if l['name']!='agent-gave-up']
 (r/'issue.json').write_text(json.dumps(i))
else: sys.exit(2)
"#;

    fn history(reason: &str, comment: u64) -> Vec<Value> {
        let body = format!("{GAVE_UP_REASON_MARKER_PREFIX}{reason} -->\nAuto-PR gave up on this issue: `{reason}`.");
        vec![
            json!({"id":comment,"event":"commented","user":{"login":"controller"},"body":body,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}),
            json!({"id":comment+1,"event":"labeled","actor":{"login":"controller"},"label":{"name":GAVE_UP_LABEL},"created_at":"2026-01-01T00:00:01Z"}),
            // GitHub cross-references have no id (#1550).
            json!({"event":"cross-referenced","actor":{"login":"controller"},"created_at":"2026-01-02T00:00:00Z"}),
        ]
    }
    fn issue(labelled: bool) -> Value {
        let labels = if labelled { json!([{"name":"bug"},{"name":GAVE_UP_LABEL}]) } else { json!([{"name":"bug"}]) };
        json!({"number":7,"state":"open","title":"Phone setup drifts","body":"Fix the phone setup","labels":labels})
    }
    fn request(id: &str) -> Request {
        Request { repository: "controller/repo".into(), issue: 7, reason: "owner re-scoped after #2053".into(), request_id: id.into() }
    }
    fn refusal(error: anyhow::Error) -> &'static str {
        error.downcast_ref::<Refusal>().unwrap_or_else(|| panic!("untyped refusal: {error:#}")).reason
    }
    fn rows(path: &Path) -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut q = conn.prepare("SELECT request_id,status FROM owner_readoption ORDER BY created_at_ms,request_id").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[tokio::test]
    async fn owner_readoption_records_authority_and_preserves_protected_holds() {
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let root = tempfile::Builder::new().prefix("readoption-").tempdir_in("/tmp").unwrap();
            let root_path = root.path().canonicalize().unwrap();
            let gh = root_path.join("gh.py");
            augmentagent_test_support::write_executable(&gh, GH).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "self_improve::readoption::tests::owner_readoption_records_authority_and_preserves_protected_holds", "--nocapture"])
                .env(CHILD, &root_path).env("GH_BIN", gh).env("AUGMENTAGENT_GH_OWNER", "controller")
                .env("XDG_STATE_HOME", root_path.join("state"))
                .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root_path.join("state/workflow.db"))
                .output().unwrap();
            assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            return;
        }
        let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
        let write = |name: &str, value: &Value| std::fs::write(root.join(name), serde_json::to_vec(value).unwrap()).unwrap();
        let edits = || std::fs::read_to_string(root.join("edits")).unwrap_or_default();
        let path = workflow::path();
        std::fs::write(root.join("permission"), "ADMIN").unwrap();
        write("issue.json", &issue(true));

        // An explicit owner hold (no controller reason pair) and a security
        // verdict are never lifted, even by the owner's readoption.
        write("timeline.json", &json!([history("held:overtaken", 1)[1]]));
        assert_eq!(refusal(apply(&root, &request("manual")).await.unwrap_err()), "protected-hold");
        write("timeline.json", &json!(history(GIVE_UP_BLAST_RADIUS_NAMED, 1)));
        assert_eq!(refusal(apply(&root, &request("security")).await.unwrap_err()), "protected-hold");
        assert_eq!(edits(), "");

        // Write access is required before any state is read or recorded.
        std::fs::write(root.join("permission"), "READ").unwrap();
        write("timeline.json", &json!(history(GIVE_UP_SCOPER_PREDICTS_REFUSAL, 1)));
        assert_eq!(refusal(apply(&root, &request("r1")).await.unwrap_err()), "target-authorization-failed");
        std::fs::write(root.join("permission"), "ADMIN").unwrap();

        // #1595-like current automatic hold with id-less cross-references.
        // A failed label write keeps the authority and retries idempotently.
        std::fs::write(root.join("fail-edit"), "").unwrap();
        assert_eq!(refusal(apply(&root, &request("r1")).await.unwrap_err()), "label-removal-failed");
        assert_eq!(rows(&path), vec![("r1".into(), "recorded".into())]);
        std::fs::remove_file(root.join("fail-edit")).unwrap();
        let value = apply(&root, &request("r1")).await.unwrap();
        assert_eq!(value["replayed"], false);
        assert_eq!(value["principal"], "github:controller");
        assert_eq!(value["prior_hold"]["reason"], GIVE_UP_SCOPER_PREDICTS_REFUSAL);
        assert_eq!((value["actor"].as_str(), value["origin"].as_str()), (Some("owner"), Some("cli")));
        assert_eq!(edits(), "7\n");
        assert_eq!(rows(&path), vec![("r1".into(), "applied".into())]);

        // Replay is a no-op; reusing the id for other intent is refused.
        assert_eq!(apply(&root, &request("r1")).await.unwrap()["replayed"], true);
        let mut changed = request("r1");
        changed.reason = "different".into();
        assert_eq!(refusal(apply(&root, &changed).await.unwrap_err()), "request-id-conflict");
        // Nothing left to readopt: a new request records nothing.
        assert_eq!(refusal(apply(&root, &request("r2")).await.unwrap_err()), "no-automatic-hold");
        assert_eq!(edits(), "7\n");

        // A later automatic hold is a new event; readopting it again appends
        // authority and keeps the earlier record.
        write("issue.json", &issue(true));
        let mut events = history(GIVE_UP_SCOPER_PREDICTS_REFUSAL, 1);
        events.extend(history(GIVE_UP_OVERTAKEN, 10));
        write("timeline.json", &json!(events));
        let value = apply(&root, &request("r3")).await.unwrap();
        assert_eq!(value["prior_hold"]["label_event"], 11);
        assert_eq!(edits(), "7\n7\n");
        assert_eq!(rows(&path).len(), 2);

        // An owner pause is an explicit hold this command never overrides.
        write("issue.json", &issue(true));
        use crate::autoship_workflow::{Binding, WorkflowStore, workflow_id};
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = workflow_id("controller/repo", 7).unwrap();
        store.register(&id, "fixture", Binding { head: "a".repeat(40), contract: "c".repeat(64), evidence_hash: "e".repeat(64) }, 1).unwrap();
        store.set_owner_pause(&id, "github:controller", 0, true, 2).unwrap();
        drop(store);
        assert_eq!(refusal(apply(&root, &request("r4")).await.unwrap_err()), "explicit-owner-hold");
        assert_eq!(edits(), "7\n7\n");
    }

    /// #1713 D — Discord readopt goes through the same readoption service and
    /// the one durable Discord request table (kind=readopt, workflow NULL),
    /// persisted before the ack and idempotent per Discord request id.
    #[tokio::test]
    async fn discord_readopt_is_durable_idempotent_and_owner_only() {
        const DISCORD_CHILD: &str = "AUTOSHIP_DISCORD_READOPT_CHILD";
        if std::env::var_os(DISCORD_CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let root = tempfile::Builder::new().prefix("discord-readopt-").tempdir_in("/tmp").unwrap();
            let root_path = root.path().canonicalize().unwrap();
            let gh = root_path.join("gh.py");
            augmentagent_test_support::write_executable(&gh, GH).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "self_improve::readoption::tests::discord_readopt_is_durable_idempotent_and_owner_only", "--nocapture"])
                .env(DISCORD_CHILD, "1").env(CHILD, &root_path).env("GH_BIN", gh).env("AUGMENTAGENT_GH_OWNER", "controller")
                .env("XDG_STATE_HOME", root_path.join("state"))
                .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root_path.join("state/workflow.db"))
                .output().unwrap();
            assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            return;
        }
        let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
        let write = |name: &str, value: &Value| std::fs::write(root.join(name), serde_json::to_vec(value).unwrap()).unwrap();
        let edits = || std::fs::read_to_string(root.join("edits")).unwrap_or_default();
        let path = workflow::path();
        std::fs::write(root.join("permission"), "ADMIN").unwrap();
        write("issue.json", &issue(true));
        write("timeline.json", &json!(history(GIVE_UP_SCOPER_PREDICTS_REFUSAL, 1)));
        let context = |message: u64, author: u64| augmentagent_approval_discord::autoship::Context {
            bot: 11, owner: Some(22), author, channel: 33, message, guild: None };
        let text = "!autoship readopt https://github.com/controller/repo/issues/7 --reason legacy hold from the October audit";
        let readopt_rows = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let mut q = conn.prepare("SELECT request_id,kind,workflow IS NULL,target,json_extract(response,'$.accepted'),json_extract(response,'$.reason') FROM discord_control_request WHERE kind='readopt' ORDER BY request_id").unwrap();
            q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, String>(3)?, r.get::<_, bool>(4)?, r.get::<_, String>(5)?)))
                .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };

        let reply = crate::autoship_discord_command::gateway_command(&root, &path, &context(101, 22), text).await.unwrap().unwrap();
        assert!(reply.content.starts_with("Autoship") && reply.content.contains("readopted"), "{}", reply.content);
        assert_eq!(edits(), "7\n");
        assert_eq!(rows(&path), vec![("discord:11:101".into(), "applied".into())], "the readoption service recorded the owner authority");
        let recorded = readopt_rows();
        assert_eq!(recorded.len(), 1);
        let (id, kind, workflow_null, target, accepted, reason) = &recorded[0];
        assert_eq!((id.as_str(), kind.as_str(), *workflow_null, *accepted, reason.as_str()), ("discord:11:101", "readopt", true, true, "owner-readopted"));
        assert_eq!(serde_json::from_str::<Value>(target).unwrap(), json!({"repository":"controller/repo","kind":"issue","number":7}));

        // Redelivery replays the recorded reply; nothing is applied twice.
        let again = crate::autoship_discord_command::gateway_command(&root, &path, &context(101, 22), text).await.unwrap();
        assert!(again.is_none_or(|r| r.content == reply.content));
        assert_eq!(edits(), "7\n");
        assert_eq!(readopt_rows().len(), 1);

        // The same Discord request id cannot be reused for another target.
        let other = "!autoship readopt https://github.com/controller/repo/issues/8 --reason legacy hold from the October audit";
        let changed = crate::autoship_discord_command::gateway_command(&root, &path, &context(101, 22), other).await.unwrap().unwrap();
        assert!(changed.content.contains("could not be recorded"), "{}", changed.content);
        assert_eq!(readopt_rows().len(), 1);
        assert_eq!(edits(), "7\n");

        // PR readoption is refused through the same record, without writes.
        let pr = "!autoship readopt https://github.com/controller/repo/pull/42 --reason adopt it";
        let refused = crate::autoship_discord_command::gateway_command(&root, &path, &context(102, 22), pr).await.unwrap().unwrap();
        assert!(refused.content.contains("pr-readoption-unsupported"), "{}", refused.content);
        let recorded = readopt_rows();
        assert_eq!((recorded.len(), recorded[1].4, recorded[1].5.as_str()), (2, false, "pr-readoption-unsupported"));
        assert_eq!(edits(), "7\n");

        // Only the configured owner may issue it: no record, no call.
        let calls = std::fs::read_to_string(root.join("calls")).unwrap();
        assert!(crate::autoship_discord_command::gateway_command(&root, &path, &context(103, 99), text).await.is_err());
        assert_eq!(readopt_rows().len(), 2);
        assert_eq!(std::fs::read_to_string(root.join("calls")).unwrap(), calls, "an unauthorized author reaches no GitHub boundary");
    }

    #[test]
    fn pr_readoption_is_deferred_to_explicit_adoption() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        let cli = Cli::try_parse_from(["readopt", "--repo", "o/r", "--pr", "4", "--reason", "x", "--request-id", "a"]).unwrap();
        assert_eq!(refusal(validate(&cli.args).unwrap_err()), "pr-readoption-unsupported");
        assert!(Cli::try_parse_from(["readopt", "--repo", "o/r", "--reason", "x", "--request-id", "a"]).is_err(), "target required");
        assert!(Cli::try_parse_from(["readopt", "--list"]).is_ok());
        let cli = Cli::try_parse_from(["readopt", "--repo", "O/R", "--issue", "4", "--reason", "x", "--request-id", "bad id"]).unwrap();
        assert_eq!(refusal(validate(&cli.args).unwrap_err()), "invalid-request");
    }
}
