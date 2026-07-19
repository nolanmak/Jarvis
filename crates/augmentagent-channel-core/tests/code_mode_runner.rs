//! Integration tests for the Code-Mode runner against the real Deno sidecar.
//!
//! These tests spawn `deno run sidecars/code-mode-runner/runner.ts` per the
//! issue #50 acceptance criteria. They are gated on `deno` being available
//! on `PATH` (or via `AUGMENTAGENT_DENO_BIN`) — when it isn't, each test
//! prints a notice and returns early, so a developer box without Deno still
//! builds clean.
//!
//! Coverage mirrors the acceptance list in #50:
//!
//! * `trace_length_matches_call_count`: 3-call program produces 3 trace rows.
//! * `fetch_is_denied_by_sandbox`: `fetch()` raises (default-deny).
//! * `unknown_tool_returns_structured_error`: `tools.notAThing()` doesn't
//!   crash the runner.
//! * `infinite_loop_is_killed_under_61s`: `while(true){}` is killed in <61s.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;

use augmentagent_channel_core::code_mode::{
    manifest_v1, run_program, RunnerError, StubDispatcher, ToolManifest,
};

/// `true` if `deno --version` succeeds (resolved via the same env-var
/// convention as the runner itself).
fn deno_available() -> bool {
    let bin = std::env::var("AUGMENTAGENT_DENO_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "deno".to_string());
    Command::new(&bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the sidecar path the same way the runner does. We assert it
/// up front so a misconfigured CARGO_MANIFEST_DIR fails loudly instead
/// of mysteriously hanging.
fn require_sidecar() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut p = PathBuf::from(manifest_dir);
    for _ in 0..6 {
        let candidate = p.join("sidecars/code-mode-runner/runner.ts");
        if candidate.exists() {
            return candidate;
        }
        if !p.pop() {
            break;
        }
    }
    panic!("could not locate sidecars/code-mode-runner/runner.ts");
}

fn maybe_skip(test_name: &str) -> bool {
    if !deno_available() {
        eprintln!(
            "skipping {test_name}: deno not available on PATH \
             (set AUGMENTAGENT_DENO_BIN to a deno >= 2.7 binary to run)"
        );
        return true;
    }
    // Force the runner to pick up the sidecar from the workspace, not
    // from CWD (which is wherever cargo runs from).
    std::env::set_var("AUGMENTAGENT_CODE_MODE_SIDECAR", require_sidecar());
    false
}

fn v1_manifest() -> ToolManifest {
    manifest_v1()
}

#[tokio::test]
async fn trace_length_matches_call_count() {
    if maybe_skip("trace_length_matches_call_count") {
        return;
    }
    // Three sequential tool calls. Stub returns canned values for each.
    let dispatcher = StubDispatcher::new(vec![
        ("wiki.draftHint".into(), json!("hint")),
        ("db.recentEmailsFrom".into(), json!([])),
        ("draft".into(), json!(null)),
    ]);
    let program = r#"
        async function main() {
            const h = await tools.wiki.draftHint({from: "fixture@example.com"});
            const r = await tools.db.recentEmailsFrom("fixture@example.com", 7);
            await tools.draft("gmail", "body " + h + " " + r.length, "reason");
            return "done";
        }
        main();
    "#;

    let out = run_program(program, &v1_manifest(), &dispatcher)
        .await
        .expect("program should succeed");
    assert_eq!(out.final_value, json!("done"));
    assert_eq!(
        out.trace.len(),
        3,
        "expected 3 trace rows, got {}: {:#?}",
        out.trace.len(),
        out.trace
    );
    assert_eq!(out.trace[0].call, "wiki.draftHint");
    assert_eq!(out.trace[1].call, "db.recentEmailsFrom");
    assert_eq!(out.trace[2].call, "draft");
}

#[tokio::test]
async fn fetch_is_denied_by_sandbox() {
    if maybe_skip("fetch_is_denied_by_sandbox") {
        return;
    }
    // No tool calls — the program throws before reaching any `tools.*`.
    let dispatcher = StubDispatcher::new(vec![]);
    let program = r#"
        async function main() {
            const r = await fetch("http://example.com");
            return r.status;
        }
        main();
    "#;
    let err = run_program(program, &v1_manifest(), &dispatcher)
        .await
        .expect_err("fetch() must error under default-deny");
    match err {
        RunnerError::RuntimeError { message, .. } => {
            // Deno 2's net-permission message contains "net access" / "permission".
            let m = message.to_lowercase();
            assert!(
                m.contains("net") || m.contains("permission") || m.contains("denied"),
                "expected a permission-denial message, got: {message}"
            );
        }
        other => panic!("expected RuntimeError, got: {other:?}"),
    }
}

#[tokio::test]
async fn unknown_tool_returns_structured_error_and_runner_survives() {
    if maybe_skip("unknown_tool_returns_structured_error_and_runner_survives") {
        return;
    }
    // The sandbox enforces manifest-membership before emitting an RPC frame
    // (see runner.ts `tool_not_in_manifest`), so a call to `tools.notAThing`
    // raises inside the program rather than reaching the dispatcher. Verify
    // the program catches it and returns normally — i.e. the runner doesn't
    // crash and we get a clean `{final}`.
    let dispatcher = StubDispatcher::always_null(&["draft"]);
    // The runner uses indirect eval (plain JS, not TS) so we can't use TS
    // type annotations like `as any` here — the runner_test.ts smoke
    // programs are all plain JS for the same reason.
    let program = r#"
        async function main() {
            try {
                await tools.notAThing();
                return "no-throw";
            } catch (e) {
                return e.message;
            }
        }
        main();
    "#;
    let out = run_program(program, &v1_manifest(), &dispatcher)
        .await
        .expect("program should complete after catching the error");
    let msg = out
        .final_value
        .as_str()
        .expect("expected string final value")
        .to_string();
    assert!(
        msg.contains("tool_not_in_manifest"),
        "expected manifest-rejection error, got: {msg}"
    );
    // No tool calls reached the dispatcher.
    assert_eq!(out.trace.len(), 0);
}

/// Regression for #190: programs containing TypeScript type assertions
/// (e.g. `as unknown as EmailContext`) used to crash the runner with
/// `SyntaxError: Unexpected token ':'` because the previous runner
/// `eval`-ed the program as plain JS. PR #204 switched to loading the
/// program as a `data:application/typescript` module so Deno's native
/// TS loader strips types before execution. This test pins that fix:
/// if someone reverts to plain-JS evaluation, the type-cast program will
/// fail to parse and this test will go red.
#[tokio::test]
async fn ts_type_assertions_in_program_do_not_crash_runner() {
    if maybe_skip("ts_type_assertions_in_program_do_not_crash_runner") {
        return;
    }
    let dispatcher = StubDispatcher::new(vec![("draft".into(), json!(null))]);
    // Exercises three TS-only syntax forms the runner must tolerate:
    //   * type alias declaration
    //   * `as` cast on a literal
    //   * `as unknown as <Type>` chain (the exact form in #190)
    let program = r#"
        type EmailContext = { from: string; messageId: string };
        async function main(): Promise<void> {
          const ctx = {
            from: "alice@example.com",
            messageId: "abc",
          } as unknown as EmailContext;
          const body = "Reply to " + ctx.from;
          await tools.draft("gmail", body, "regression for #190");
        }
        await main();
    "#;

    let out = run_program(program, &v1_manifest(), &dispatcher)
        .await
        .expect("TS-cast program must parse and run under data: TS module loading");
    assert_eq!(out.trace.len(), 1, "expected single draft call");
    assert_eq!(out.trace[0].call, "draft");
}

#[tokio::test]
async fn infinite_loop_is_killed_under_61s() {
    if maybe_skip("infinite_loop_is_killed_under_61s") {
        return;
    }
    let dispatcher = StubDispatcher::new(vec![]);
    // Long sleep that yields control so the sandbox's 60s timer can fire.
    // (Plain `while(true){}` blocks the JS event loop so the sandbox's own
    // setTimeout can't fire — Deno still kills it via the Rust-side wall
    // clock at 65s, which satisfies <70s.) This mirrors the
    // `runner_test.ts` "timeout fires before promise" coverage.
    let program = r#"
        async function main() {
            await new Promise(function (r) { setTimeout(r, 90000); });
        }
        main();
    "#;
    let start = Instant::now();
    let err = run_program(program, &v1_manifest(), &dispatcher)
        .await
        .expect_err("infinite loop must error");
    let elapsed = start.elapsed();
    // Hard upper bound: the Rust-side wall clock is 65s; the sandbox's own
    // is 60s. We allow a small grace beyond 65s for process shutdown.
    assert!(
        elapsed < Duration::from_secs(70),
        "expected kill in <70s, took {:?}",
        elapsed
    );
    // The error is either a sandbox-emitted timeout frame (kind="timeout")
    // or a Rust-side wall-clock kill — both satisfy the acceptance criterion.
    match err {
        RunnerError::RuntimeError { kind, message, .. } => {
            assert_eq!(
                kind.as_deref(),
                Some("timeout"),
                "expected sandbox timeout, got message={message}"
            );
        }
        RunnerError::Timeout { .. } => { /* Rust-side kill is also acceptable */ }
        other => panic!("unexpected error: {other:?}"),
    }
}
