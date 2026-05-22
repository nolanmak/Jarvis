//! Code-Mode dispatcher: the Rust side of the host-bridge that the Deno
//! sandbox (see `sidecars/code-mode-runner/runner.ts`) speaks NDJSON to.
//!
//! Architecturally there are two halves:
//!
//! * The **runner** ([`super::runner::run_program`]) owns the subprocess,
//!   spawns Deno with no `--allow-*` flags, frames NDJSON over its stdio,
//!   and drives the RPC loop.
//! * The **dispatcher** (this module) owns the allowlist + per-tool wiring.
//!   It is the only object the runner asks "what does `wiki.draftHint` mean?"
//!   — every `{call, args, id}` frame the sandbox emits is fed to
//!   [`Dispatcher::call`].
//!
//! Keeping the two halves separated by an `async_trait` lets the integration
//! tests in `tests/code_mode_*.rs` swap in a `StubDispatcher` that returns
//! canned values, so the harness exercises the real Deno sidecar without
//! depending on a live SQLite store, Composio calendar client, or rate
//! governor.
//!
//! ## Trace bookkeeping
//!
//! Every call (success or failure) is recorded as a [`ToolCallRecord`] in
//! declaration order. The runner drains the accumulated `Vec<ToolCallRecord>`
//! once the sandbox emits its terminal `{"final"}` frame and hands it back
//! to the caller as part of [`super::CodeModeOutcome`].
//!
//! Callers serialise this vector via `serde_json::to_string` before passing
//! it to `Store::log_action_code_mode` — the store API takes
//! `trace_json: &str`, by design (matches `meta_json` / `exemplar_ids` /
//! `raw_json` elsewhere in the schema).

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};
use thiserror::Error;

use augmentagent_store::{ActionStatus, Email, Store};

use super::trace::{now_ms, summarize_value, ToolCallRecord};
use crate::governor::{
    ActionKind, ActionRequest, Platform, RateGovernor, Risk, TargetAttrs,
};

/// Errors a dispatcher can hand back to the sandbox. The runner reflects
/// these as `{"id": <n>, "error": "<message>"}` frames; the program sees
/// them as a thrown `Error` it can catch.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// Tool name was not in the v1 allowlist. The runner will not crash —
    /// the program receives a structured `Error` and may decide how to
    /// recover (typically it logs and falls through to a safe default).
    #[error("unknown_tool: {0}")]
    UnknownTool(String),

    /// Args couldn't be decoded into the shape the tool expects. The
    /// message should name the offending field where possible.
    #[error("bad_args: {0}")]
    BadArgs(String),

    /// `RateGovernor::permit` (or whatever quota check the dispatcher
    /// performs) refused the action.
    #[error("permit_denied: {0}")]
    PermitDenied(String),

    /// Any other failure produced by the backing implementation
    /// (database, HTTP call, etc.). Carries the underlying error message
    /// stringified so the sandbox doesn't need to know the concrete type.
    #[error("internal: {0}")]
    Internal(String),
}

impl DispatchError {
    /// Human-facing wire form sent back to the sandbox. Mirrors the
    /// `Display` impl but is centralised so the runner doesn't have to
    /// format the error itself.
    pub fn wire_message(&self) -> String {
        self.to_string()
    }
}

/// Async allowlist trait the runner calls for every `tools.*` RPC.
///
/// Implementations should:
///
/// 1. Match `name` against the v1 manifest exactly (no normalisation).
/// 2. Decode `args` (a `Value::Array` of positional args) into the
///    per-tool shape they need.
/// 3. Record the resulting [`ToolCallRecord`] in their internal trace
///    buffer so the runner can return it to the caller.
/// 4. Return `Ok(result_value)` or `Err(DispatchError)`.
///
/// Trait is `Send + Sync` so the runner can hold an `&dyn Dispatcher`
/// across `.await` points; concrete implementations typically use
/// `Mutex<Vec<ToolCallRecord>>` for trace bookkeeping.
#[async_trait]
pub trait Dispatcher: Send + Sync {
    /// Handle one RPC call. `args` is the JSON array the program passed
    /// positionally (`tools.foo(a, b)` → `[a, b]`).
    async fn call(&self, name: &str, args: Value) -> Result<Value, DispatchError>;

    /// Snapshot of the trace accumulated so far. The runner calls this once,
    /// after the sandbox emits `{"final": ...}` (or an error frame), and
    /// hands the vector back to the caller as part of `CodeModeOutcome`.
    fn drain_trace(&self) -> Vec<ToolCallRecord>;
}

/// Test-only dispatcher used by the integration tests and by anyone wiring
/// up Code Mode for a new channel — they can drop this in to verify the
/// runner half before adding real tool wiring.
///
/// On `call`, looks up the (name, fixed-result) mapping passed in at
/// construction time. Unknown names → `DispatchError::UnknownTool`.
/// Every call (including errors) is appended to the internal trace.
///
/// Args and results are passed through [`summarize_value`] before they're
/// pushed onto the trace, so a deliberately verbose stub still keeps the
/// audit row small.
pub struct StubDispatcher {
    handlers: Vec<(String, Value)>,
    trace: Mutex<Vec<ToolCallRecord>>,
}

impl StubDispatcher {
    /// Build a stub that returns `result` for any call whose `name`
    /// matches an entry in `handlers`. Order doesn't matter — first match
    /// wins, but the v1 manifest has no overlapping names.
    pub fn new(handlers: Vec<(String, Value)>) -> Self {
        Self {
            handlers,
            trace: Mutex::new(Vec::new()),
        }
    }

    /// Convenience: stub that always returns `null` for the listed names.
    pub fn always_null(names: &[&str]) -> Self {
        Self::new(names.iter().map(|n| ((*n).to_string(), Value::Null)).collect())
    }
}

#[async_trait]
impl Dispatcher for StubDispatcher {
    async fn call(&self, name: &str, args: Value) -> Result<Value, DispatchError> {
        let ts = now_ms();
        let hit = self
            .handlers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone());

        match hit {
            Some(result) => {
                let rec = ToolCallRecord {
                    call: name.to_string(),
                    args_summary: summarize_value(&args),
                    result_summary: Some(summarize_value(&result)),
                    error: None,
                    timestamp_ms: ts,
                };
                self.trace.lock().expect("trace mutex poisoned").push(rec);
                Ok(result)
            }
            None => {
                let err = DispatchError::UnknownTool(name.to_string());
                let rec = ToolCallRecord {
                    call: name.to_string(),
                    args_summary: summarize_value(&args),
                    result_summary: None,
                    error: Some(err.wire_message()),
                    timestamp_ms: ts,
                };
                self.trace.lock().expect("trace mutex poisoned").push(rec);
                Err(err)
            }
        }
    }

    fn drain_trace(&self) -> Vec<ToolCallRecord> {
        std::mem::take(&mut *self.trace.lock().expect("trace mutex poisoned"))
    }
}

// =============================================================================
// DefaultDispatcher — v1 allowlist wired to real backing impls.
// =============================================================================

/// Per-message context the production dispatcher needs to land an `actions`
/// row when `tools.draft` fires.
///
/// `channel` is the canonical platform string (`"gmail"`, `"linkedin"`, …)
/// the program passes as the first arg to `tools.draft(channel, body,
/// reason)`. The dispatcher rejects a draft whose channel arg doesn't match
/// the dispatcher's own `channel` — this prevents a code-mode program from
/// landing a draft on a different channel than the triage flow set up.
#[derive(Debug, Clone)]
pub struct MessageContext {
    /// Canonical channel name (e.g. `"gmail"`, `"linkedin"`).
    pub channel: String,
    /// Email record the program is drafting against. `augmentagent_store::Email`
    /// is the cross-channel model — `kind`/`platform` discriminate.
    pub email: Email,
    /// Account / entity id this message belongs to. `None` for the legacy
    /// single-account Gmail path. Carried through into the `ActionRequest`
    /// the rate governor sees.
    pub account_id: Option<String>,
}

/// Minimal calendar adapter the dispatcher needs for `calendar.busy`.
///
/// Lives behind a trait rather than depending on
/// `augmentagent-channel-calendar::CalendarApi` directly so channel-core
/// doesn't drag the calendar crate (and its reqwest deps) into its tree.
/// Channels wire a thin adapter around `CalendarApi::list_events`.
#[async_trait]
pub trait CalendarBusyLookup: Send + Sync {
    /// Return a JSON array describing busy windows in `[start_iso, end_iso]`
    /// on `calendar_id` for `entity_id`. Shape is whatever the channel finds
    /// useful for the model — `[{start, end, summary}]` is the v1 convention.
    async fn busy(
        &self,
        entity_id: &str,
        calendar_id: &str,
        start_iso: &str,
        end_iso: &str,
    ) -> Result<Value, String>;
}

/// v1 allowlist dispatcher.
///
/// Routes every entry in [`super::manifest_v1`] to its backing Rust function.
/// The terminal `tools.draft` call calls `RateGovernor::permit` (when wired)
/// and then `Store::log_action_code_mode` to land the `actions` row with
/// `mode='code'`; everything else is a read-only data lookup.
///
/// Holds references rather than owning so the channel can drop / re-borrow
/// its own `Store` without an extra clone. Lifetime `'a` ties the dispatcher
/// to the channel's per-poll scope — code mode runs synchronously inside one
/// `process_email` invocation.
pub struct DefaultDispatcher<'a> {
    /// Backing store. Used by `db.*` reads and the terminal `tools.draft`
    /// write.
    pub store: &'a Store,
    /// Optional calendar adapter for `calendar.busy`. When `None`,
    /// `calendar.busy` returns an empty array so the program can keep going.
    pub calendar: Option<&'a dyn CalendarBusyLookup>,
    /// Optional rate governor for the terminal draft. When `None`, the
    /// permit step is skipped (test path only).
    pub governor: Option<&'a dyn RateGovernor>,
    /// Per-message context — message id, channel, account.
    pub message_ctx: MessageContext,
    /// Generated TS program source — persisted alongside the action row.
    pub generated_source: String,
    /// Precomputed wiki hint string (channel runs `WikiReader::draft_hint`
    /// once before code-mode and hands the result in). `wiki.draftHint`
    /// returns this verbatim.
    pub wiki_hint: String,
    /// Calendar id passed to `CalendarBusyLookup::busy`. v1 hard-codes
    /// `"primary"` per the architect note in #50; kept as a field so a
    /// future channel can override without forking the dispatcher.
    pub calendar_id: String,
    trace: Mutex<Vec<ToolCallRecord>>,
    last_action_id: Mutex<Option<String>>,
}

impl<'a> DefaultDispatcher<'a> {
    /// Construct a dispatcher with an empty trace. Builders below attach
    /// optional wiring (calendar, governor, wiki hint, calendar id).
    pub fn new(
        store: &'a Store,
        message_ctx: MessageContext,
        generated_source: impl Into<String>,
    ) -> Self {
        Self {
            store,
            calendar: None,
            governor: None,
            message_ctx,
            generated_source: generated_source.into(),
            wiki_hint: String::new(),
            calendar_id: "primary".to_string(),
            trace: Mutex::new(Vec::new()),
            last_action_id: Mutex::new(None),
        }
    }

    /// Attach a calendar busy-lookup adapter. Without it, `calendar.busy`
    /// returns an empty array.
    pub fn with_calendar(mut self, c: &'a dyn CalendarBusyLookup) -> Self {
        self.calendar = Some(c);
        self
    }

    /// Attach a rate governor so the terminal `draft` call goes through
    /// `permit()` before landing the action row. Without it, the gate is
    /// skipped — useful in tests, never in production.
    pub fn with_governor(mut self, g: &'a dyn RateGovernor) -> Self {
        self.governor = Some(g);
        self
    }

    /// Precompute the wiki hint the channel already builds via
    /// `WikiReader::draft_hint(&email)` and stash it for `wiki.draftHint`.
    pub fn with_wiki_hint(mut self, hint: impl Into<String>) -> Self {
        self.wiki_hint = hint.into();
        self
    }

    /// Override the calendar id queried by `calendar.busy`. Default is
    /// `"primary"`.
    pub fn with_calendar_id(mut self, id: impl Into<String>) -> Self {
        self.calendar_id = id.into();
        self
    }

    /// Action id of the most recent `tools.draft` write. `None` until the
    /// program reaches its terminal `draft` call. Used by tests and by the
    /// `code-mode dry-run` CLI subcommand to round-trip the action row.
    pub fn last_action_id(&self) -> Option<String> {
        self.last_action_id
            .lock()
            .expect("last_action_id mutex poisoned")
            .clone()
    }

    fn record_success(&self, name: &str, args: &Value, result: &Value) {
        let rec = ToolCallRecord {
            call: name.to_string(),
            args_summary: summarize_value(args),
            result_summary: Some(summarize_value(result)),
            error: None,
            timestamp_ms: now_ms(),
        };
        self.trace.lock().expect("trace mutex poisoned").push(rec);
    }

    fn record_failure(&self, name: &str, args: &Value, err: &str) {
        let rec = ToolCallRecord {
            call: name.to_string(),
            args_summary: summarize_value(args),
            result_summary: None,
            error: Some(err.to_string()),
            timestamp_ms: now_ms(),
        };
        self.trace.lock().expect("trace mutex poisoned").push(rec);
    }

    /// Snapshot the trace without draining. Used by `tools.draft` to embed
    /// the full call history into the persisted `toolCallTrace` column.
    fn snapshot(&self) -> Vec<ToolCallRecord> {
        self.trace.lock().expect("trace mutex poisoned").clone()
    }

    /// Map the dispatcher's `MessageContext` + the program's `(channel,
    /// reason)` arg into the `ActionRequest` the rate governor wants. v1 uses
    /// the conservative defaults (`Risk::Medium`, treat target as a stranger
    /// so the approval-required matrix kicks in for unknown senders).
    fn build_action_request(&self, channel: &str, reason: &str) -> ActionRequest {
        let platform = Platform::parse(channel).unwrap_or(Platform::Twitter);
        ActionRequest {
            platform,
            action: ActionKind::Reply,
            account_id: self
                .message_ctx
                .account_id
                .clone()
                .or_else(|| self.message_ctx.email.account_entity_id.clone())
                .unwrap_or_else(|| "default".to_string()),
            risk: Risk::Medium,
            cause: reason.to_string(),
            target_id: Some(self.message_ctx.email.from.clone()),
            target_attrs: Some(TargetAttrs {
                known_contact: false,
                mass_action: false,
                stranger: true,
            }),
        }
    }

    async fn dispatch_inner(&self, name: &str, args: &Value) -> Result<Value, DispatchError> {
        match name {
            "wiki.draftHint" => {
                // v1: channel precomputed the hint via `WikiReader::draft_hint(&email)`
                // and stashed it. We ignore the model's `EmailContext` arg and
                // return the stashed string verbatim — matches the manifest
                // doc ("Returns a short hint string the model can fold into
                // its draft").
                let v = json!(self.wiki_hint.clone());
                self.record_success(name, args, &v);
                Ok(v)
            }
            "db.recentEmailsFrom" => {
                let (sender, days) =
                    parse_recent_args(args).map_err(DispatchError::BadArgs)?;
                let since_ms = now_ms() - days * 86_400_000;
                // `recent_emails_since` returns `(from, subject, triage)`.
                // We filter by sender on the Rust side — for a typical
                // `days = 14..30` the scan is bounded by the 200-row LIMIT.
                let rows = self
                    .store
                    .recent_emails_since(since_ms, 200)
                    .map_err(|e| DispatchError::Internal(e.to_string()))?;
                let out: Vec<Value> = rows
                    .into_iter()
                    .filter(|(from, _, _)| from.eq_ignore_ascii_case(&sender))
                    .map(|(from, subject, triage)| {
                        json!({
                            "from": from,
                            "subject": subject,
                            "triage": triage,
                        })
                    })
                    .collect();
                let v = Value::Array(out);
                self.record_success(name, args, &v);
                Ok(v)
            }
            "db.threadHistory" => {
                let thread_id = parse_string_arg(args, "threadId")
                    .map_err(DispatchError::BadArgs)?;
                // `since_ms = 0` means "no lower bound" — the SQL returns the
                // whole thread, oldest first.
                let rows = self
                    .store
                    .recent_emails_for_thread(&thread_id, 0)
                    .map_err(|e| DispatchError::Internal(e.to_string()))?;
                let out: Vec<Value> = rows
                    .into_iter()
                    .map(|(from, subject, body)| {
                        // Trim each body to a 280-char snippet so the trace
                        // (and the model's context) stays bounded for big
                        // threads.
                        let snippet: String = body.chars().take(280).collect();
                        json!({
                            "from": from,
                            "subject": subject,
                            "snippet": snippet,
                        })
                    })
                    .collect();
                let v = Value::Array(out);
                self.record_success(name, args, &v);
                Ok(v)
            }
            "db.isProcessed" => {
                let msg_id = parse_string_arg(args, "messageId")
                    .map_err(DispatchError::BadArgs)?;
                let ok = self
                    .store
                    .is_message_processed(&msg_id)
                    .map_err(|e| DispatchError::Internal(e.to_string()))?;
                let v = json!(ok);
                self.record_success(name, args, &v);
                Ok(v)
            }
            "calendar.busy" => {
                let (start_iso, end_iso) =
                    parse_busy_args(args).map_err(DispatchError::BadArgs)?;
                let Some(cal) = self.calendar else {
                    // No calendar adapter wired — return an empty list rather
                    // than erroring so a defensively-coded program still
                    // produces a draft.
                    let v = Value::Array(Vec::new());
                    self.record_success(name, args, &v);
                    return Ok(v);
                };
                let entity = self
                    .message_ctx
                    .account_id
                    .clone()
                    .or_else(|| self.message_ctx.email.account_entity_id.clone())
                    .unwrap_or_default();
                let v = cal
                    .busy(&entity, &self.calendar_id, &start_iso, &end_iso)
                    .await
                    .map_err(DispatchError::Internal)?;
                self.record_success(name, args, &v);
                Ok(v)
            }
            "draft" => {
                let (channel, body, reason) =
                    parse_draft_args(args).map_err(DispatchError::BadArgs)?;
                if !channel.eq_ignore_ascii_case(&self.message_ctx.channel) {
                    let msg = format!(
                        "channel mismatch: program targeted {channel:?}, dispatcher \
                         is wired for {:?}",
                        self.message_ctx.channel
                    );
                    return Err(DispatchError::Internal(msg));
                }
                if let Some(gov) = self.governor {
                    let req = self.build_action_request(&channel, &reason);
                    if let Err(d) = gov.permit(req).await {
                        return Err(DispatchError::PermitDenied(d.to_string()));
                    }
                }

                // Record the terminal `draft` entry BEFORE the store write
                // so the persisted `toolCallTrace` includes it. Trace length
                // = number of `tools.*` calls (including `draft`) — matches
                // the architect's expectation.
                let success_value = Value::Null;
                self.record_success(name, args, &success_value);

                let trace_buf = self.snapshot();
                let trace_json = serde_json::to_string(&trace_buf).map_err(|e| {
                    DispatchError::Internal(format!("serialize trace: {e}"))
                })?;

                let email = &self.message_ctx.email;
                let action_id = self
                    .store
                    .log_action_code_mode(
                        &email.message_id,
                        email.thread_id.as_deref(),
                        &email.from,
                        &email.subject,
                        Some(&email.body),
                        Some(&body),
                        ActionStatus::Pending,
                        &self.generated_source,
                        &trace_json,
                    )
                    .map_err(|e| DispatchError::Internal(e.to_string()))?;
                *self
                    .last_action_id
                    .lock()
                    .expect("last_action_id mutex poisoned") = Some(action_id);
                let _ = reason; // included in the trace via args_summary
                Ok(success_value)
            }
            other => Err(DispatchError::UnknownTool(other.into())),
        }
    }
}

#[async_trait]
impl<'a> Dispatcher for DefaultDispatcher<'a> {
    async fn call(&self, name: &str, args: Value) -> Result<Value, DispatchError> {
        // Wrap the body so every error path automatically lands in the
        // trace. Success paths call `record_success` inside each arm.
        match self.dispatch_inner(name, &args).await {
            Ok(v) => Ok(v),
            Err(e) => {
                self.record_failure(name, &args, &e.wire_message());
                Err(e)
            }
        }
    }

    fn drain_trace(&self) -> Vec<ToolCallRecord> {
        std::mem::take(&mut *self.trace.lock().expect("trace mutex poisoned"))
    }
}

// =============================================================================
// Arg parsing helpers
// =============================================================================

fn parse_recent_args(args: &Value) -> Result<(String, i64), String> {
    let arr = args
        .as_array()
        .ok_or_else(|| "expected positional arg array".to_string())?;
    if arr.len() < 2 {
        return Err(format!("expected 2 args (sender, days); got {}", arr.len()));
    }
    let sender = arr[0]
        .as_str()
        .ok_or_else(|| "arg 0 (sender) must be string".to_string())?
        .to_string();
    let days = arr[1]
        .as_i64()
        .or_else(|| arr[1].as_f64().map(|f| f as i64))
        .ok_or_else(|| "arg 1 (days) must be number".to_string())?;
    Ok((sender, days.max(0)))
}

fn parse_string_arg(args: &Value, label: &str) -> Result<String, String> {
    let arr = args
        .as_array()
        .ok_or_else(|| "expected positional arg array".to_string())?;
    if arr.is_empty() {
        return Err(format!("expected 1 arg ({label})"));
    }
    arr[0]
        .as_str()
        .ok_or_else(|| format!("arg 0 ({label}) must be string"))
        .map(|s| s.to_string())
}

fn parse_busy_args(args: &Value) -> Result<(String, String), String> {
    let arr = args
        .as_array()
        .ok_or_else(|| "expected positional arg array".to_string())?;
    if arr.len() < 2 {
        return Err(format!("expected 2 args (startIso, endIso); got {}", arr.len()));
    }
    let s = arr[0]
        .as_str()
        .ok_or_else(|| "arg 0 (startIso) must be string".to_string())?
        .to_string();
    let e = arr[1]
        .as_str()
        .ok_or_else(|| "arg 1 (endIso) must be string".to_string())?
        .to_string();
    Ok((s, e))
}

fn parse_draft_args(args: &Value) -> Result<(String, String, String), String> {
    let arr = args
        .as_array()
        .ok_or_else(|| "expected positional arg array".to_string())?;
    if arr.len() < 3 {
        return Err(format!(
            "expected 3 args (channel, body, reason); got {}",
            arr.len()
        ));
    }
    let channel = arr[0]
        .as_str()
        .ok_or_else(|| "arg 0 (channel) must be string".to_string())?
        .to_string();
    let body = arr[1]
        .as_str()
        .ok_or_else(|| "arg 1 (body) must be string".to_string())?
        .to_string();
    let reason = arr[2]
        .as_str()
        .ok_or_else(|| "arg 2 (reason) must be string".to_string())?
        .to_string();
    Ok((channel, body, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn stub_records_success_in_trace() {
        let d = StubDispatcher::new(vec![("wiki.draftHint".into(), json!("hint"))]);
        let v = d.call("wiki.draftHint", json!([{"from": "a"}])).await.unwrap();
        assert_eq!(v, json!("hint"));
        let trace = d.drain_trace();
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].call, "wiki.draftHint");
        assert_eq!(trace[0].result_summary, Some(json!("hint")));
        assert!(trace[0].error.is_none());
    }

    #[tokio::test]
    async fn stub_records_unknown_tool_error() {
        let d = StubDispatcher::new(vec![]);
        let err = d.call("nope", json!([])).await.unwrap_err();
        match err {
            DispatchError::UnknownTool(n) => assert_eq!(n, "nope"),
            other => panic!("wrong variant: {other:?}"),
        }
        let trace = d.drain_trace();
        assert_eq!(trace.len(), 1);
        assert!(trace[0].error.as_deref().unwrap().contains("unknown_tool"));
        assert!(trace[0].result_summary.is_none());
    }

    #[tokio::test]
    async fn drain_resets_trace() {
        let d = StubDispatcher::always_null(&["a"]);
        d.call("a", json!([])).await.unwrap();
        assert_eq!(d.drain_trace().len(), 1);
        // Second drain is empty.
        assert_eq!(d.drain_trace().len(), 0);
    }

    #[test]
    fn parse_recent_args_happy_path() {
        let (s, d) = parse_recent_args(&json!(["alice@example.com", 7])).unwrap();
        assert_eq!(s, "alice@example.com");
        assert_eq!(d, 7);
    }

    #[test]
    fn parse_recent_args_rejects_short_array() {
        let err = parse_recent_args(&json!([])).unwrap_err();
        assert!(err.contains("expected 2 args"));
    }

    #[test]
    fn parse_draft_args_three_strings() {
        let (c, b, r) =
            parse_draft_args(&json!(["gmail", "body", "reason"])).unwrap();
        assert_eq!(c, "gmail");
        assert_eq!(b, "body");
        assert_eq!(r, "reason");
    }
}
