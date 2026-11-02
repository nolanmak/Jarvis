//! Content-free native phone timing. Missing starts remain unknown; neither
//! tool arguments/results nor provider tool IDs escape into these measurements.
use std::{collections::HashMap, sync::Mutex};
use tokio::time::Instant;
struct Timing {
    call: String,
    task: String,
    started: Instant,
    tools: Mutex<HashMap<String, (&'static str, Instant)>>,
}
tokio::task_local! {static TIMING:Timing;}
pub(crate) async fn scope<F: std::future::Future>(call: &str, task: &str, future: F) -> F::Output {
    TIMING
        .scope(
            Timing {
                call: call.into(),
                task: task.into(),
                started: Instant::now(),
                tools: Mutex::new(HashMap::new()),
            },
            future,
        )
        .await
}
pub(crate) fn spawned(provider: &str) {
    let _=TIMING.try_with(|t|tracing::info!(call_id=%t.call,task_id=%t.task,provider,native_spawn_ms=t.started.elapsed().as_millis() as u64,"phone native subprocess admitted"));
}
fn tool_name(name: &str) -> &'static str {
    match name {
        "Bash" => "Bash",
        "Read" => "Read",
        "WebSearch" => "WebSearch",
        "WebFetch" => "WebFetch",
        _ => "other",
    }
}
fn started(id: &str, name: &str) {
    if id.len() > 256 {
        return;
    }
    let _=TIMING.try_with(|t| {
        let mut tools=t.tools.lock().unwrap_or_else(|p|p.into_inner());
        if tools.len()<128 && !tools.contains_key(id) {
            tracing::info!(call_id=%t.call,task_id=%t.task,tool=tool_name(name),native_elapsed_ms=t.started.elapsed().as_millis() as u64,"phone native tool started");
            tools.insert(id.into(),(tool_name(name),Instant::now()));
        }
    });
}
fn finished(id: &str, failed: bool) {
    let _=TIMING.try_with(|t| {
        if let Some((name,start))=t.tools.lock().unwrap_or_else(|p|p.into_inner()).remove(id) {
            tracing::info!(call_id=%t.call,task_id=%t.task,tool=name,tool_ms=start.elapsed().as_millis() as u64,failed,"phone native tool finished");
        }
    });
}
pub(crate) fn claude_event(event: &crate::tool_audit::ToolEvent) {
    match event {
        crate::tool_audit::ToolEvent::Use { id, name, .. } => started(id, name),
        crate::tool_audit::ToolEvent::Result { id, is_error, .. } => finished(id, *is_error),
    }
}
pub(crate) fn codex_event(event: &serde_json::Value) {
    let item = &event["item"];
    if !matches!(
        item["type"].as_str(),
        Some("mcp_tool_call" | "command_execution")
    ) {
        return;
    }
    let Some(id) = item["id"].as_str() else {
        return;
    };
    match event["type"].as_str() {
        Some("item.started") => started(id, item["tool"].as_str().unwrap_or("other")),
        Some("item.completed") => finished(
            id,
            item["status"] == "failed" || item["result"]["isError"] == true,
        ),
        _ => {}
    }
}
