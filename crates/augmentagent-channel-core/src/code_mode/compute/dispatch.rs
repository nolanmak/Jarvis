//! Compute bridge with metadata-only audit records. Neither generated source
//! nor returned stdout/stderr is suitable for public failure reports.
use super::ComputeService;
use crate::code_mode::{trace::now_ms, DispatchError, Dispatcher, ToolCallRecord};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub struct ComputeDispatcher {
    service: Arc<ComputeService>,
    trace: Mutex<Vec<ToolCallRecord>>,
    failed: AtomicBool,
}
impl ComputeDispatcher {
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
    pub fn new(service: Arc<ComputeService>) -> Self {
        Self {
            service,
            trace: Mutex::new(Vec::new()),
            failed: AtomicBool::new(false),
        }
    }
}
#[async_trait]
impl Dispatcher for ComputeDispatcher {
    async fn call(&self, name: &str, args: Value) -> Result<Value, DispatchError> {
        if name != "compute.run" {
            self.failed.store(true, Ordering::SeqCst);
            return Err(DispatchError::UnknownTool(name.into()));
        }
        let args = args
            .as_array()
            .filter(|args| args.len() == 1)
            .ok_or_else(|| {
                self.failed.store(true, Ordering::SeqCst);
                DispatchError::BadArgs("compute.run requires one request object".into())
            })?;
        let request = &args[0];
        if !request.is_object() {
            self.failed.store(true, Ordering::SeqCst);
            return Err(DispatchError::BadArgs(
                "compute.run requires a request object".into(),
            ));
        }
        let result = self.service.execute(request.clone()).await;
        if result.as_ref().map_or(true, |value| value["ok"] != true) {
            self.failed.store(true, Ordering::SeqCst);
        }
        // Only host-selected keys and primitive/count metadata enter the
        // shared Code Mode trace. Even dependency names may be attacker text.
        let args_summary = json!({
            "runtime":"python",
            "dependencyCount":request["dependencies"].as_array().map(Vec::len),
            "inputCount":request["inputs"].as_array().map(Vec::len).unwrap_or(0),
            "outputCount":request["outputs"].as_array().map(Vec::len).unwrap_or(0),
        });
        let result_summary = result.as_ref().ok().map(|value| {
            json!({
                "ok":value["ok"].as_bool(),
                "environmentReused":value["environmentReused"].as_bool(),
                "artifactCount":value["artifacts"].as_array().map(Vec::len).unwrap_or(0),
            })
        });
        self.trace
            .lock()
            .expect("compute trace lock")
            .push(ToolCallRecord {
                call: "compute.run".into(),
                args_summary,
                result_summary,
                error: result
                    .as_ref()
                    .err()
                    .map(|_| "Compute transport failed.".into()),
                timestamp_ms: now_ms(),
            });
        result.map_err(|_| {
            DispatchError::Internal(
                "Compute transport failed; inspect private task diagnostics.".into(),
            )
        })
    }
    fn drain_trace(&self) -> Vec<ToolCallRecord> {
        std::mem::take(&mut *self.trace.lock().expect("compute trace lock"))
    }
}
