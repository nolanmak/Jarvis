//! Observation is detached from job execution: this module never claims work,
//! changes authority, releases a lease, or cancels provider descendants.
use crate::autoship_workflow::submission::Milestone;
use serde_json::{json,Value};
use std::{path::Path,time::Duration};

pub(crate) async fn observe(repo:&Path, path:&Path, admission:Value, until:Milestone, timeout:Duration)->(i32,Value) {
    let workflow=admission["workflow"].as_str().expect("accepted admission has workflow");
    let deadline=tokio::time::Instant::now()+timeout;
    let cap=std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP").ok().and_then(|s|s.parse::<u32>().ok()).unwrap_or(5).max(1);
    loop {
        let (code,mut body)=super::status_for_milestone(path,workflow,chrono::Utc::now().timestamp_millis(),cap,Some(until));
        body["admission"]=admission.clone();
        body["request_id"]=admission["request_id"].clone();
        body["requested_until"]=serde_json::to_value(until).unwrap();
        // Admission remains historical fact even if a later state read fails.
        body["accepted"]=true.into();
        if body["complete"]==true {
            if until==Milestone::Pr {
                let pr=body["milestones"]["pr"]["pr"].as_u64().expect("PR completion has publication receipt");
                let observed=match tokio::time::timeout_at(deadline,crate::self_improve::targeted::published_pr_head(repo,
                    body["repository"].as_str().expect("workflow repository"),pr)).await {
                    Ok(observed)=>observed,
                    Err(_)=>return timed_out(body),
                };
                let reason=match observed {
                    Ok(head) if body["head"].as_str().is_some_and(|expected|expected.eq_ignore_ascii_case(&head))=>None,
                    Ok(head)=>{body["remote_head"]=head.into();Some("published-head-changed")},
                    Err(_)=>Some("published-head-unverified"),
                };
                if let Some(reason)=reason {
                    body["complete"]=false.into();body["workflow_reason"]=body["reason"].clone();
                    body["reason"]=reason.into();body["next_action"]="revalidate_current_pr".into();
                    body["observation"]=json!({"state":"blocked","detached":true});
                    return (3,body);
                }
            }
            body["observation"]=json!({"state":"complete","detached":true});
            return (0,body);
        }
        let held=matches!(body["stage"].as_str(),Some("held"|"retired"|"merged"));
        let queued_blocker=body["stage"]=="queued" && matches!(body["last_attempt"]["outcome"].as_str(),Some("deferred"|"rejected"|"failed"));
        if code==2 || code==4 || held || queued_blocker {
            body["observation"]=json!({"state":"blocked","detached":true});
            return (if code==2 || code==4 {4} else {3},body);
        }
        let now=tokio::time::Instant::now();
        if now>=deadline {
            return timed_out(body);
        }
        tokio::time::sleep_until(deadline.min(now+Duration::from_millis(250))).await;
    }
}

fn timed_out(mut body:Value)->(i32,Value) {
    body["complete"]=false.into();
    body["workflow_reason"]=body["reason"].clone();
    body["reason"]="wait-timeout".into();
    body["observation"]=json!({"state":"timed-out","detached":true});
    (5,body)
}
