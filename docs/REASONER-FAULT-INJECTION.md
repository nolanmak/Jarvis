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

Text-only is deliberate. Triage-shaped presets (`Read`/`Grep`/`Glob`) are
`ReadTools`, and codex is not eligible for those (`providers::allowed_for`,
gated on #664) — a "triage falls over to codex" scenario cannot exist until
that policy changes, and the policy is right as it stands.

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
