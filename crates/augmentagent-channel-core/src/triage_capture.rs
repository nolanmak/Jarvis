//! Bounded controller-owned evidence capture for optional triage calibration.
//! No logger/public output. Incomplete attempts cannot become replay evidence.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock},
};
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Evidence {
    pub native_claude: bool,
    pub requested_model: String,
    pub observed_model: String,
    pub native_session: String,
    pub tools: Vec<serde_json::Value>,
    pub complete: bool,
    pending: BTreeSet<String>,
    bytes: usize,
    overflow: bool,
}
static ACTIVE: OnceLock<Mutex<BTreeMap<String, Arc<Mutex<Evidence>>>>> = OnceLock::new();
fn active() -> &'static Mutex<BTreeMap<String, Arc<Mutex<Evidence>>>> {
    ACTIVE.get_or_init(Default::default)
}
pub struct Capture {
    pub session: String,
    state: Arc<Mutex<Evidence>>,
}
impl Capture {
    pub fn new() -> Self {
        let session = format!("triage-evidence:{}", uuid::Uuid::new_v4());
        let state = Arc::new(Mutex::new(Evidence::default()));
        active()
            .lock()
            .unwrap()
            .insert(session.clone(), state.clone());
        Self { session, state }
    }
    pub fn evidence(&self) -> Evidence {
        self.state.lock().unwrap().clone()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        active().lock().unwrap().remove(&self.session);
    }
}
fn with(session: &str, f: impl FnOnce(&mut Evidence)) {
    let state = active().lock().unwrap().get(session).cloned();
    if let Some(state) = state {
        f(&mut state.lock().unwrap());
    }
}
pub(crate) fn start(session: &str, requested: &str, native: bool) {
    with(session, |e| {
        *e = Evidence::default();
        e.native_claude = native;
        e.requested_model = requested.into();
    });
}
pub(crate) fn line(session: &str, line: &str) {
    with(session, |e| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            e.overflow = true;
            return;
        };
        if let Some(model) = v
            .get("model")
            .or_else(|| v.pointer("/message/model"))
            .and_then(|v| v.as_str())
        {
            if !e.observed_model.is_empty() && e.observed_model != model {
                e.overflow = true;
            }
            e.observed_model = model.into();
        }
        if let Some(id) = v.get("session_id").and_then(|v| v.as_str()) {
            if !e.native_session.is_empty() && e.native_session != id {
                e.overflow = true;
            }
            e.native_session = id.into();
        }
        for event in crate::tool_audit::parse_stream_event(line) {
            let mut row = match event {
                crate::tool_audit::ToolEvent::Use { id, name, input } => {
                    if !e.pending.insert(id.clone()) {
                        e.overflow = true;
                    }
                    serde_json::json!({"kind":"use","id":id,"tool":name,"input":input})
                }
                crate::tool_audit::ToolEvent::Result {
                    id,
                    is_error,
                    content,
                } => {
                    if !e.pending.remove(&id) {
                        e.overflow = true;
                    }
                    serde_json::json!({"kind":"result","id":id,"is_error":is_error,"content":content})
                }
            };
            let id = row["id"].as_str().unwrap_or("");
            let original = v
                .pointer("/message/content")
                .and_then(|v| v.as_array())
                .and_then(|blocks| {
                    blocks.iter().find(|block| {
                        block
                            .get("id")
                            .or_else(|| block.get("tool_use_id"))
                            .and_then(|v| v.as_str())
                            == Some(id)
                    })
                });
            if id.is_empty() || original.is_none() {
                e.overflow = true;
            }
            if let Some(original) = original {
                if original["type"] == "tool_result" {
                    match original.get("content") {
                        Some(serde_json::Value::String(_)) => {}
                        Some(serde_json::Value::Array(items))
                            if items
                                .iter()
                                .all(|item| item["type"] == "text" && item["text"].is_string()) => {
                        }
                        _ => e.overflow = true,
                    }
                }
                row["original_block"] = original.clone();
            }
            e.bytes = e.bytes.saturating_add(
                serde_json::to_vec(&row)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX),
            );
            if e.bytes > 128 * 1024 || e.tools.len() >= 64 {
                e.overflow = true;
            } else {
                e.tools.push(row);
            }
        }
    });
}
pub(crate) fn finish(session: &str) {
    with(session, |e| {
        e.complete = e.native_claude
            && !e.overflow
            && e.pending.is_empty()
            && !e.observed_model.is_empty()
            && !e.native_session.is_empty()
    });
}
tokio::task_local! {static SUPPLEMENT:(String,String);}
pub async fn with_supplement<T>(
    value: &str,
    model: &str,
    call: impl std::future::Future<Output = T>,
) -> T {
    SUPPLEMENT.scope((value.into(), model.into()), call).await
}
pub(crate) fn append(native_claude: bool, prompt: &mut String) -> Option<String> {
    if native_claude {
        if let Ok((value, model)) = SUPPLEMENT.try_with(Clone::clone) {
            if !value.is_empty() && !model.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&value);
                return Some(model);
            }
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captures_complete_tool_bytes_and_actual_model_without_audit_truncation() {
        let c = Capture::new();
        start(&c.session, "opus", true);
        line(
            &c.session,
            r#"{"type":"system","model":"claude-opus-fixture","session_id":"actual-native"}"#,
        );
        line(
            &c.session,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"notes.md"}}]}}"#,
        );
        let content = "exact evidence ".repeat(2000);
        line(&c.session,&serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"read","content":content}]}}).to_string());
        finish(&c.session);
        let e = c.evidence();
        assert!(e.complete);
        assert_eq!(e.observed_model, "claude-opus-fixture");
        assert_eq!(e.tools[1]["content"], content);
    }
    #[test]
    fn incomplete_routed_and_oversized_capture_cannot_supply_labels() {
        for routed in [true, false] {
            let c = Capture::new();
            start(&c.session, "opus", !routed);
            line(
                &c.session,
                r#"{"type":"system","model":"fixture","session_id":"native"}"#,
            );
            line(
                &c.session,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"missing","name":"Read","input":{}}]}}"#,
            );
            finish(&c.session);
            assert!(!c.evidence().complete);
        }
        let c = Capture::new();
        start(&c.session, "opus", true);
        line(
            &c.session,
            r#"{"type":"system","model":"fixture","session_id":"native"}"#,
        );
        line(
            &c.session,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"big","name":"Read","input":{}}]}}"#,
        );
        line(&c.session,&serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"big","content":"x".repeat(130*1024)}]}}).to_string());
        finish(&c.session);
        assert!(!c.evidence().complete);
    }
    #[tokio::test]
    async fn guidance_only_applies_to_native_replayed_route_and_pins_exact_model() {
        with_supplement("tested guidance", "claude-fixture", async {
            let mut native = "original".to_owned();
            assert_eq!(append(true, &mut native), Some("claude-fixture".into()));
            assert!(native.ends_with("tested guidance"));
            let mut fallback = "original".to_owned();
            assert_eq!(append(false, &mut fallback), None);
            assert_eq!(fallback, "original");
        })
        .await;
        let mut outside = "original".to_owned();
        assert_eq!(append(true, &mut outside), None);
        assert_eq!(outside, "original");
    }
}
