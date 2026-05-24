//! Code-Mode tool manifest, sandbox glue, and audit-trace plumbing.
//!
//! This module owns three layers of the Code-Mode pipeline:
//!
//! * The **v1 manifest** (`manifest`) — the LLM-allowed tool surface, with
//!   both a `.d.ts` declaration block (for the system prompt) and a flat
//!   allowlist of dotted names (for the Deno runner).
//! * The **runtime / Deno wiring** (`runner` + `dispatch`) — subprocess +
//!   NDJSON RPC loop plus the allowlist trait and per-tool handlers.
//! * The **audit trace** (`trace`) — [`ToolCallRecord`] shape and the
//!   `summarize_value` clip helpers that keep one runaway call from
//!   blowing up the `actions.toolCallTrace` row.
//!
//! The pieces are split so the manifest can be unit-tested without
//! spawning subprocesses, and the runner can be integration-tested with a
//! `StubDispatcher` instead of a live SQLite + Composio backing store.
//!
//! The reasoner-side `extract_ts_block` helper (and `CodeModeError`) lives
//! here too — produced by I5 (#51) so this module is also the entry point
//! between "model text → fenced TS program" and "program → sandbox →
//! action row."

pub mod dispatch;
pub mod failure;
pub mod manifest;
pub mod runner;
pub mod trace;

pub use dispatch::{
    CalendarBusyLookup, DefaultDispatcher, DispatchError, Dispatcher, MessageContext,
    StubDispatcher,
};
pub use failure::{
    handle_code_mode_failure, report_classic_fallback, DraftOutcome, FailureCtx, FailureRecord,
    FailureStage, GhCliIssueRunner, GhIssueRunner,
};
pub use manifest::{manifest_v1, ToolDef, ToolManifest};
// The runtime layer renames its error type to `RunnerError` to avoid
// colliding with the reasoner-side [`CodeModeError`] this module already
// owns. Downstream callers compose the two (reasoner produces source →
// runner consumes source) but they're distinct failure modes.
pub use runner::{
    check_deno_available, resolve_deno_bin, run_program, CodeModeOutcome, DenoResolution,
    DenoSource, RunnerError, RUST_WALL_CLOCK_MS,
};
pub use trace::{summarize_value, ToolCallRecord, SUMMARY_MAX_BYTES};

/// Failure modes specific to the Code-Mode call pipeline.
///
/// I5 only owns the reasoner-side variants (`NoCodeBlock`, `ReasonerFailed`).
/// I4's dispatcher / sandbox layer surfaces its own
/// [`RunnerError`](crate::code_mode::RunnerError) for runner / timeout /
/// budget faults; downstream callers match by name and treat the two
/// enums as composable.
#[derive(Debug, thiserror::Error)]
pub enum CodeModeError {
    /// The reasoner returned an assistant message that did not contain a
    /// fenced ```ts``` or ```typescript``` code block. Callers may retry via
    /// [`crate::reasoner::Reasoner::call_code_mode_with_repair`].
    #[error("no fenced ```ts/```typescript code block in assistant response")]
    NoCodeBlock,

    /// The underlying `claude` CLI call itself failed (spawn / IO / stream
    /// parse). Wraps the original error so callers can inspect the cause.
    #[error("reasoner call failed: {0}")]
    ReasonerFailed(#[source] anyhow::Error),
}

/// Extract the first fenced ```ts``` or ```typescript``` block from `text`.
///
/// The language tag is matched case-insensitively (the model occasionally
/// emits `TypeScript`). Other fenced blocks (e.g. ```js, ```python, bare
/// ``` ``` blocks) are skipped — this extractor is stricter than "first
/// fenced block" on purpose so the model is forced to label its program.
///
/// The returned string is the raw inner source with leading / trailing
/// whitespace trimmed; the body's internal whitespace is preserved verbatim.
///
/// Returns [`CodeModeError::NoCodeBlock`] if no matching fenced block is
/// found.
pub fn extract_ts_block(text: &str) -> Result<String, CodeModeError> {
    // Manual scan rather than a regex: cheap, dep-free, and easy to extend
    // if we later need to accept fence opener attributes like
    // ```ts title="program".
    let mut cursor = 0usize;
    while cursor < text.len() {
        let Some(rel) = text[cursor..].find("```") else {
            break;
        };
        let opener = cursor + rel + 3;
        // Read the language tag — everything from the opener up to the next
        // newline (or EOF). Then take the first whitespace-delimited token.
        let line_end = text[opener..]
            .find('\n')
            .map(|p| opener + p)
            .unwrap_or(text.len());
        let tag = text[opener..line_end]
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if tag == "ts" || tag == "typescript" {
            // Body starts at the byte after the opener-line newline. If the
            // opener has no newline before EOF, treat that as malformed.
            if line_end >= text.len() {
                return Err(CodeModeError::NoCodeBlock);
            }
            let body_start = line_end + 1;
            let close_rel = text[body_start..]
                .find("```")
                .ok_or(CodeModeError::NoCodeBlock)?;
            let body_end = body_start + close_rel;
            return Ok(text[body_start..body_end].trim().to_string());
        }
        // Not a ts/typescript block — jump past this fence's closer (if any)
        // and keep scanning. If there's no closer, no ts block can exist.
        match text[line_end..].find("```") {
            Some(close_rel) => {
                cursor = line_end + close_rel + 3;
            }
            None => return Err(CodeModeError::NoCodeBlock),
        }
    }
    Err(CodeModeError::NoCodeBlock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ts_block_trimmed() {
        let resp = "Here is the program:\n\n```ts\nasync function main(): Promise<void> {\n  await tools.draft(\"gmail\", \"hi\", \"r\");\n}\n```\n";
        let got = extract_ts_block(resp).expect("must extract");
        assert_eq!(
            got,
            "async function main(): Promise<void> {\n  await tools.draft(\"gmail\", \"hi\", \"r\");\n}"
        );
    }

    #[test]
    fn extracts_typescript_block() {
        let resp = "```typescript\nconst x = 1;\n```";
        let got = extract_ts_block(resp).expect("must extract typescript-tagged block");
        assert_eq!(got, "const x = 1;");
    }

    #[test]
    fn case_insensitive_language_tag() {
        let resp = "```TypeScript\nfoo();\n```";
        assert_eq!(extract_ts_block(resp).unwrap(), "foo();");
    }

    #[test]
    fn skips_non_ts_block_then_finds_ts() {
        let resp = "Some context.\n```js\n// not this one\n```\nAnd the real program:\n```ts\nbar();\n```\n";
        assert_eq!(extract_ts_block(resp).unwrap(), "bar();");
    }

    #[test]
    fn missing_fence_errors_with_nocodeblock() {
        let err = extract_ts_block("just prose, no fence").unwrap_err();
        assert!(matches!(err, CodeModeError::NoCodeBlock));
    }

    #[test]
    fn untagged_fence_is_rejected() {
        // A bare ``` ... ``` block has no `ts`/`typescript` tag → not a match.
        let resp = "```\nasync function main() {}\n```";
        let err = extract_ts_block(resp).unwrap_err();
        assert!(matches!(err, CodeModeError::NoCodeBlock));
    }
}
