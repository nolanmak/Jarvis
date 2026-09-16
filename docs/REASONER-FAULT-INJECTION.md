# Reasoner fault-injection rig (#655/#666)

Deterministic failover testing for the multi-provider reasoner chain, with
zero provider quota spent. Every provider CLI is replaced by a committed
stub that emits the exact stream a real CLI emits when it refuses, hangs, or
answers, so the whole seam — chain order, typed errors, the cooldown latch,
the "latched provider is never spawned" skip — is exercised against a real
built binary.

## The stubs

`scripts/reasoner-fault-injection/`, one script per behaviour:

| stub | impersonates | outcome |
|---|---|---|
| `fake-claude-ok.sh` | `claude -p --output-format stream-json` | answers `PONG-FROM-FAKE-CLAUDE`, exit 0 |
| `fake-claude-quota.sh` | ditto, out of quota | the session-limit refusal as a **successful** completion (#448's shape) → `RateLimited` |
| `fake-claude-hang.sh` | ditto, wedged | accepts the prompt, never answers → `Timeout` once the watchdog fires |
| `fake-codex-ok.sh` | `codex exec --json` | JSONL stream ending in `PONG-FROM-FAKE-CODEX`, exit 0 |
| `fake-codex-usage-limit.sh` | ditto, out of quota | `turn.failed` + exit 1 → `RateLimited` |
| `fake-codex-empty.sh` | ditto, turn finishes with no final message | a completed tool call, `turn.completed`, no `agent_message`, exit 0 → untyped `TurnFailure` (content-level, #1040) |
| `fake-codex-context-window.sh` | ditto, request overflows the context window | `turn.failed` context-window message + exit 1 → untyped `TurnFailure` (content-level, #1040) |
| `fake-codex-stream-disconnected.sh` | ditto, API connection lost mid-turn | `Reconnecting…` notice, then `turn.failed` "stream disconnected" + exit 1 → `Unavailable` |
| `fake-gemini-ok.sh` | `gemini --output-format json` | `{"response":"PONG-FROM-FAKE-GEMINI"}`, exit 0 |
| `fake-gemini-429.sh` | ditto, out of quota | `RESOURCE_EXHAUSTED` error object + exit 1 → `RateLimited` |

Each stub records one line per spawn in `$HOME/.fake-cli/<provider>.count`,
which is how a test asserts the negative — "the latched provider was never
spawned again", "the fallback was never probed". `HOME` is the state channel
because the codex and gemini adapters spawn with `env_clear()` (the #128
posture): a `FAKE_*` variable would reach the claude stub and nothing else.

## Env knobs

| var | effect |
|---|---|
| `AUGMENTAGENT_REASONER_CHAIN` | the chain to exercise, e.g. `claude,codex` |
| `CLAUDE_CLI` / `CODEX_CLI` / `GEMINI_CLI` | point an adapter at a stub |
| `AUGMENTAGENT_COOLDOWN_FILE` | **always set this** — see the warning below |
| `AUGMENTAGENT_CODEX_HOME` | codex is dropped from the chain without an `auth.json` here |
| `GEMINI_API_KEY` | likewise, gemini is dropped without a resolvable key |
| `AUGMENTAGENT_REASONER_TIMEOUT_SECS` | shrink the watchdog for the hang scenario |
| `HOME` | scratch dir; also where the stubs keep their counters |
| `AUGMENTAGENT_E2E_BIN` | binary the integration test drives (default: the cargo test build) |
| `AUGMENTAGENT_E2E_SCRATCH` | parent dir for each scenario's scratch home, latch and db (default: the system temp dir) |

> **Never run the rig without `AUGMENTAGENT_COOLDOWN_FILE`.** The latch is
> process-shared, durable state: a stubbed quota refusal writes
> `~/.local/state/augmentagent/reasoner-cooldowns.json` and the owner's live
> daemon then stops calling Claude for the next 30 minutes.

## Scenarios

`crates/augmentagent-cli/tests/reasoner_failover_e2e.rs` drives
`augmentagent reasoner-selftest` — one text-only round trip through the
production `build_reasoner()` — as a subprocess, once per scenario: quota
refusal → codex, hung primary → codex, whole chain refusing, gemini serving
and latching, and healthy primary with no fallback spawn.

### Content-level codex failures (#1040)

These scenarios put codex first (`AUGMENTAGENT_REASONER_CHAIN=codex,claude`,
claude served by `fake-claude-ok.sh`). If the chain advances by mistake, the
claude stub records a spawn.

| scenario | codex stub | expected |
|---|---|---|
| `codex_empty_output_neither_latches_nor_fails_over` | `fake-codex-empty.sh` | exit 1, `codex produced no assistant text`, no codex latch, **0 claude spawns** |
| `codex_context_window_failure_neither_latches_nor_fails_over` | `fake-codex-context-window.sh` | exit 1, no codex latch, **0 claude spawns** |
| `codex_usage_limit_still_latches_and_fails_over` (control) | `fake-codex-usage-limit.sh` | claude serves, codex latched `codex rate limit`, second run skips codex |
| `codex_transport_failure_still_latches_and_fails_over` (control) | `fake-codex-stream-disconnected.sh` | claude serves, codex latched `codex unavailable: stream disconnected…` |

The routing table behind these scenarios is
`crates/augmentagent-channel-core/src/turn_failure.rs`. Only quota,
transport, auth and binary failures latch. Content-level failures (an empty
turn, context overflow, a policy refusal, the bridge refusing further tool
calls) never latch and never advance the chain. Text that matches no row
after the turn began gets the same treatment, because tools may already have
run. The unit tests pin every row of that table (`turn_failure::tests`,
`codex::tests::turn_failed_routing_is_pinned_per_failure_class`).

The rig cannot show the other half of #1040: a write request whose journal
already holds completed operations is not dispatched again. The selftest is
text-only and carries no operation journal, so that half is covered by
`fallback::tests::completed_write_without_summary_is_not_redispatched_by_fallback_or_caller_retry`,
its negative control, and the test that pins the unchanged
resume-from-receipts path.

### Why the selftest stays text-only

Text-only is the widest probe that needs no tool fixture: every provider in
the chain is eligible for it. It is no longer a statement about codex
eligibility. Since PR #1021, codex serves every class, `ReadTools`,
`WriteTools` and `FullAgentic` included (`providers::allowed_for`), through
the scoped Jarvis tool bridge. Gemini serves `TextOnly` and `ReadTools`, and
cerebras serves `TextOnly` only. Tool-class routing is covered by the
`fallback.rs` unit tests
(`codex_fallback_routes_each_capability_and_keeps_healthy_primary_preferred`)
and by the live receipts in `docs/CODEX-FALLBACK.md`, not by this rig.

## Minting a PR-gate receipt

`scripts/agent-pr-verify-gate.sh` blocks `gh pr create` when the diff touches
`crates/augmentagent-channel-core/src/reasoner.rs` until a receipt exists at
`.claude/agent-test-receipts/<HEAD-sha>.txt`. For a failover change, this run
is that receipt:

```bash
cargo build --release
mkdir -p .claude/agent-test-receipts
set -o pipefail   # else `tee` masks a RED run and mints a GREEN receipt (#793)
AUGMENTAGENT_E2E_BIN=./target/release/augmentagent \
  cargo test -p augmentagent-cli --test reasoner_failover_e2e -- --nocapture --test-threads=1 \
  | tee ".claude/agent-test-receipts/$(git rev-parse HEAD).txt"
```

The transcript shows, per scenario, the chain, the latches taken, who served
the call, and `reasoner call served by FALLBACK provider (…)`.

**What a rig receipt does NOT prove.** The stubs never read the prompt they
are handed and never run a tool. So a receipt from this rig is honest
evidence for the *failover seam* only — chain construction, eligibility,
typed error mapping, latching, skipping. If your change touches a system
prompt, a tool allowlist, an MCP wiring, or anything else the model has to
actually act on, the rig says nothing about it and you still owe the live
exercise the gate message describes (`augmentagent --wiki-dir ./wiki wiki ask
"…"`). Say which of the two you did in the receipt.

## Not covered by the rig

Three questions from #666 need real provider quota and human judgement, and
no stub can answer them: whether a stdio MCP server's tool call completes
under `codex exec` + sandbox (openai/codex#24135), whether a gemini
`BeforeTool` deny hook actually blocks in `-p` mode, and what each provider's
real token floor is. Those stay owner-run spikes.
