use anyhow::Result;
use async_trait::async_trait;
use augmentagent_channel_core::{draft_workflow, Reasoner, ReasonerOpts};
use augmentagent_store::{ActionStatus, Email, Store};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

struct Scripted {
    replies: Mutex<VecDeque<String>>,
    timeout: bool,
}

#[async_trait]
impl Reasoner for Scripted {
    async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
        if self.timeout {
            tokio::time::sleep(Duration::from_secs(91)).await;
            return Ok(json!({"action":"ready","facts":[],"gaps":[]}).to_string());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(self.replies.lock().unwrap().pop_front().unwrap())
    }
}

fn email(platform: &str, id: &str) -> Email {
    Email {
        message_id: id.into(),
        thread_id: Some("thread".into()),
        from: "peer@example.invalid".into(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        subject: "subject".into(),
        body: "PRIVATE_TRIGGER_BODY".into(),
        date: "2026-01-01T00:00:00Z".into(),
        account_entity_id: Some("account".into()),
        platform: platform.into(),
        kind: "dm".into(),
    }
}

fn receipt_for(store: &Store, context: &draft_workflow::PreparedDraftContext, mail: &Email) -> Result<Value> {
    let action = store.log_action(&mail.message_id, mail.thread_id.as_deref(), &mail.from, &mail.subject, Some(&mail.body), Some("draft"), ActionStatus::Pending)?;
    context.attach(store, &action)?;
    Ok(serde_json::from_str(&store.draft_context_receipts(&action)?.remove(0))?)
}

fn step_summary(receipt: &Value, platform: &str) -> Value {
    let steps = receipt.get("step_timings").and_then(Value::as_array).cloned().unwrap_or_default();
    let model_steps = steps.iter().filter(|step| step.get("kind").and_then(Value::as_str) == Some("model_decision")).count();
    let retrieval_steps = steps.iter().filter(|step| step.get("kind").and_then(Value::as_str) == Some("retrieval")).count();
    let all_elapsed = !steps.is_empty() && steps.iter().all(|step| step.get("elapsed_ms").and_then(Value::as_u64).is_some_and(|ms| ms > 0));
    json!({"platform":platform,"model_steps":model_steps,"retrieval_steps":retrieval_steps,"all_elapsed":all_elapsed,"consumer":receipt.get("budget_consumer").cloned().unwrap_or(Value::Null),"source_body_present":receipt.to_string().contains("PRIVATE_TRIGGER_BODY") || receipt.to_string().contains("PRIVATE_RETRIEVED_BODY")})
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("projects"))?;
    std::fs::write(wiki.join("projects/plan.md"), "PRIVATE_RETRIEVED_BODY")?;
    let store = Store::open(dir.path().join("data.db"))?;
    let mut output = Vec::new();

    for (platform, id) in [("imessage", "imessage-trigger"), ("gmail", "gmail-trigger")] {
        let mail = email(platform, id);
        let agent = Scripted { replies: Mutex::new(VecDeque::from([json!({"action":"search_wiki","query":"plan","reason":"locate plan"}).to_string(), json!({"action":"ready","facts":[],"gaps":[]}).to_string()])), timeout: false };
        let context = draft_workflow::prepare(&store, &agent, &mail, "reply", Some(&wiki)).await?;
        output.push(step_summary(&receipt_for(&store, &context, &mail)?, platform));
    }

    let timeout_mail = email("imessage", "timeout-trigger");
    let timeout_agent = Scripted { replies: Mutex::new(VecDeque::new()), timeout: true };
    let timeout_context = draft_workflow::prepare(&store, &timeout_agent, &timeout_mail, "reply", None).await?;
    let timeout_receipt = receipt_for(&store, &timeout_context, &timeout_mail)?;
    let timeout_steps = timeout_receipt.get("step_timings").and_then(Value::as_array).cloned().unwrap_or_default();
    let (_, warnings) = augmentagent_approval_discord::split_needs_input(&timeout_context.guard_draft("draft"));
    output.push(json!({"timeout_model_steps":timeout_steps.iter().filter(|step| step.get("kind").and_then(Value::as_str) == Some("model_decision")).count(),"timeout_consumer":timeout_receipt.get("budget_consumer").cloned().unwrap_or(Value::Null),"timeout_gap_visible":warnings.iter().any(|warning| warning.text.contains("time limit"))}));

    let zero_mail = email("imessage", "zero-trigger");
    let zero_agent = Scripted { replies: Mutex::new(VecDeque::new()), timeout: false };
    let zero_context = draft_workflow::prepare(&store, &zero_agent, &zero_mail, &"x".repeat(64_001), None).await?;
    let zero_receipt = receipt_for(&store, &zero_context, &zero_mail)?;
    let zero_steps = zero_receipt.get("step_timings").and_then(Value::as_array).cloned().unwrap_or_default();
    output.push(json!({"zero_model_steps":zero_steps.iter().filter(|step| step.get("kind").and_then(Value::as_str) == Some("model_decision")).count(),"zero_retrieval_steps":zero_steps.iter().filter(|step| step.get("kind").and_then(Value::as_str) == Some("retrieval")).count(),"zero_consumer":zero_receipt.get("budget_consumer").cloned().unwrap_or(Value::Null)}));
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
