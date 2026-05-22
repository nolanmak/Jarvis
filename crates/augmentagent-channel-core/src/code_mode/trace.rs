//! Per-call audit trace types and helpers.
//!
//! The runner streams `{call, args, id}` and `{id, result|error}` frames over
//! the NDJSON bridge to / from the Deno sidecar. The Rust dispatcher captures
//! every call → result pair as a [`ToolCallRecord`] in declaration order; the
//! accumulated `Vec<ToolCallRecord>` is what `tools.draft` serializes into the
//! `toolCallTrace` column of the `actions` row (#48).
//!
//! `ToolCallRecord` is intentionally `serde_json::Value`-typed for both args
//! and result so the trace can hold arbitrary tool shapes without dragging
//! every backing crate's types onto a shared enum. The store treats the
//! serialized form as opaque bytes (`trace_json: &str`) — see
//! `Store::log_action_code_mode`.
//!
//! [`summarize_value`] clips an args / result JSON value down to a sane size
//! before persisting it. The Code Mode `toolCallTrace` column is meant for
//! postmortem, not bulk data, so we cap individual values at
//! [`SUMMARY_MAX_BYTES`] to keep one runaway tool call from blowing up the
//! `actions` row.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One audit entry per `tools.*` call from a Code-Mode program.
///
/// Built by `DefaultDispatcher::call` (or any custom `Dispatcher` impl) after
/// the underlying Rust backing function returns, and pushed into the per-run
/// trace buffer. The terminal `tools.draft` call serializes the whole buffer
/// into the `actions.toolCallTrace` column for audit / postmortem.
///
/// `error` is mutually exclusive with `result_summary` — exactly one of the
/// two is `Some` per record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// Dotted tool name as sent on the wire (e.g. `"db.recentEmailsFrom"`).
    pub call: String,
    /// Truncated copy of the args the program passed to the tool. Stored as a
    /// JSON `Value` so we can round-trip arbitrary shapes (numbers, strings,
    /// arrays, objects).
    pub args_summary: Value,
    /// Truncated copy of the tool's return value, when the call succeeded.
    /// `None` when the call returned an error (see `error`).
    pub result_summary: Option<Value>,
    /// Error message, when the call failed. Mutually exclusive with
    /// `result_summary`.
    pub error: Option<String>,
    /// Wall-clock ms-since-epoch when the record was appended to the trace
    /// (i.e. immediately after the dispatch returned).
    pub timestamp_ms: i64,
}

/// Current wall-clock ms-since-epoch. Pulled out so tests can stub it out;
/// today this just calls `chrono::Utc::now()` like the rest of the codebase.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Soft cap on the JSON-string length of one args/result summary. Anything
/// bigger is clipped via [`summarize_value`] so a single tool can't blow the
/// `actions.toolCallTrace` row size out.
pub const SUMMARY_MAX_BYTES: usize = 4_096;

/// Truncate a JSON value so its serialized form is within
/// [`SUMMARY_MAX_BYTES`]. Strings are clipped with a `…` marker; arrays and
/// objects keep their first 50 entries recursively. The output is always
/// valid JSON.
///
/// Best-effort byte cap — nested structures can slightly exceed the cap if
/// every leaf is at the limit. The goal is "audit-friendly," not strict
/// equality.
pub fn summarize_value(v: &Value) -> Value {
    let raw = v.to_string();
    if raw.len() <= SUMMARY_MAX_BYTES {
        return v.clone();
    }
    truncate(v)
}

fn truncate(v: &Value) -> Value {
    match v {
        Value::String(s) => {
            if s.len() <= SUMMARY_MAX_BYTES {
                Value::String(s.clone())
            } else {
                // Clip by char count, not byte count, so a UTF-8 boundary is
                // never split. The trailing `…` makes the truncation visible
                // in postmortem reads.
                let mut clipped: String = s.chars().take(SUMMARY_MAX_BYTES).collect();
                clipped.push('…');
                Value::String(clipped)
            }
        }
        Value::Array(arr) => Value::Array(arr.iter().take(50).map(truncate).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map.iter().take(50) {
                out.insert(k.clone(), truncate(val));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn small_value_passes_through() {
        let v = json!({"a": 1, "b": "two"});
        assert_eq!(summarize_value(&v), v);
    }

    #[test]
    fn big_string_is_clipped_with_ellipsis() {
        let big = "x".repeat(SUMMARY_MAX_BYTES * 3);
        let v = Value::String(big);
        let out = summarize_value(&v);
        let s = out.as_str().unwrap();
        // Bound is SUMMARY_MAX_BYTES ASCII chars + 3-byte ellipsis.
        assert!(s.len() <= SUMMARY_MAX_BYTES + 8);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn array_capped_at_50_entries() {
        // Build a 200-element array whose JSON-string form exceeds the cap so
        // the truncate path actually runs.
        let arr: Vec<Value> = (0..200).map(|_| json!("padding ".repeat(40))).collect();
        let v = Value::Array(arr);
        let out = summarize_value(&v);
        assert_eq!(out.as_array().unwrap().len(), 50);
    }

    #[test]
    fn tool_call_record_round_trips_via_serde() {
        let r = ToolCallRecord {
            call: "wiki.draftHint".into(),
            args_summary: json!({"from": "fixture@example.com"}),
            result_summary: Some(json!("hint text")),
            error: None,
            timestamp_ms: 1_700_000_000_000,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ToolCallRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn now_ms_is_positive_and_recent() {
        let t = now_ms();
        // After Jan 2020 (a sane lower bound that catches obvious bugs).
        assert!(t > 1_577_836_800_000);
    }

    #[test]
    fn object_capped_at_50_entries() {
        // Build a 100-key object whose serialized form exceeds the cap.
        let big_val = "v".repeat(200);
        let mut map = serde_json::Map::new();
        for i in 0..100 {
            map.insert(format!("k{i}"), Value::String(big_val.clone()));
        }
        let v = Value::Object(map);
        let out = summarize_value(&v);
        assert_eq!(out.as_object().unwrap().len(), 50);
    }

}
