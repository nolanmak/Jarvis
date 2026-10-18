# Owner phone calls — issue #1496

Status: implementation in progress. Nothing here establishes production phone
availability or live ConversationRelay qualification. The previous one-way
Twilio test established credential validity and outbound dialing only.

Issue: https://github.com/nolanmak/Jarvis/issues/1496

## Current design

`augmentagent-phone` is shared by chat adapters and the agent tool facade. It
uses the existing store and does not own an independent agent conversation.
Its provider boundary is `Telephony`; `TwilioClient` implements bounded HTTP
requests without redialing an uncertain create. Callback URLs have a fixed
operator-configured HTTPS origin. Signature verification includes every form
field and uses the configured public URL instead of forwarded client headers.

`augmentagent-store::phone` owns additive SQLite tables for destination state,
verification challenges, dial claims, relay claims and cleanup attempts. Dial
reservation happens in an immediate transaction; only its first caller receives
a send permit. A pending/uncertain call keeps the owner's active-call slot even
after restart. Call receipts cannot serialize phone destinations. Explicit
operator import is one-time; a removed destination leaves a tombstone.

Phone grants require a stored surface owner and control conversation, plus an
explicit operator-owned mapping to a canonical owner ID. Verification is allowed
only in the stored direct conversation. A capability is bounded to its original
owner, conversation and owner-binding revision. It expires after five minutes
and rechecks authorization on use. Integration adapters must never mint grants
from ingested document content or model-supplied identities.

Verification uses a random six-digit code spoken by a fixed TwiML message.
The database contains only an HMAC bound to owner and request ID; the key is a
separate operator-provided secret. Private command handlers must consume codes
before history/agent ingestion. Codes expire after five minutes and five wrong
attempts. A pending replacement leaves the prior destination active.

Normal outbound calls redirect to a signed start callback. That records the
answer timestamp before returning a generic DTMF prompt. Only an accepted call
receives ConversationRelay TwiML. A watchdog enforces the 15-second acceptance
window independently of Twilio Gather's post-prompt timeout. Relay partial
prompts are disabled explicitly. Finalized utterances get distinct turn IDs;
identical words in separate utterances are not incorrectly deduplicated.

## Test-first evidence

Each initial suite was run before its implementation. Red runs exited 101 with
unresolved APIs; the final-output regression instead failed its behavioral
assertion before the fix. Local transcripts are under `/tmp/1496-*.log` in the
implementation workspace (not release receipts).

| Slice | First failing command | Expected failure |
| --- | --- | --- |
| Durable calls | `cargo test -p augmentagent-store --test phone_calls` | Missing phone module/methods |
| Verification and acceptance | Same command, expanded tests | Missing verification/accept APIs |
| Owner service | `cargo test -p augmentagent-phone --test service` | Missing service/authorization types |
| Twilio adapter | `cargo test -p augmentagent-phone --lib` | Missing client/signature validation |
| Signed callbacks | Service suite, expanded tests | Missing callback handlers |
| Relay protocol | `cargo test -p augmentagent-phone --test relay` | Missing protocol state |
| Final speech suppression | Relay suite, changed auto-final wording | Automatic final hit a conflicting speech receipt |
| Relay claims and maintenance | Store/service suites, expanded tests | Missing claim/maintenance APIs |

Run the current deterministic suites with:

```sh
AUGMENTAGENT_GH_DISABLE=1 cargo test -p augmentagent-store --test phone_calls
AUGMENTAGENT_GH_DISABLE=1 cargo test -p augmentagent-phone
```

## Acceptance audit — not complete

| Criteria | Evidence so far | Remaining proof/work |
| --- | --- | --- |
| AC01 | Disabled-service test; fixed-origin validation | Environment/readiness wiring, deployed ingress and runbook |
| AC02 | Grant, revocation, expiry, forged target tests | Production surface and MCP boundaries |
| AC03 | Ten concurrent reservations; restart/uncertain tests | Provider reconciliation and runtime concurrency fault injection |
| AC04 | Known Twilio signature fixture; signed status callbacks; reordered events; real local HTTP ingress | Live callbacks and final-duration accounting |
| AC05 | Signed start/accept tests; fake-clock watchdog | Runtime watchdog and voicemail/live timing |
| AC06 | Existing shared session runner identified | Agent bridge, tools/approvals and mixed text/phone serialization |
| AC07 | Relay protocol, barge-in generations, final suppression, one-time claim; real local WSS-protocol round trip and disconnect cleanup | Native durable turn integration, cancellation/streaming |
| AC08 | Service contract parameterized across three surface refs | Actual chat command/tool injection, preset inventory, durable mirrors |
| AC09 | Rate/daily budgets, restart, cleanup claims | Runtime scheduling, provider-duration reconciliation, failure receipts |
| AC10 | None for live conversations | Full live host/provider/origin and audible-latency matrix |
| AC11 | No legacy voice removed yet | Replacement qualification, migration and retirement |
| AC12 | Targeted local tests | OS CI, full regression map and live receipts |
| AC13–15 | Verification/store/private-service tests | Actual chat routers, explicit account-link setup, live number setup |

The old conversational voice stack must not be removed before replacement
qualification. Its shared session/history functionality and inbound voice-file
capture remain separate migration concerns. This document is a progress record,
not permission to weaken any requirement of #1496.

## First implementation checkpoint

On this Linux workspace, the first checkpoint passes 10 phone storage tests,
18 phone crate tests (4 provider, 3 protocol, 11 service/HTTP/WebSocket), all 148
existing store unit tests, and `cargo clippy -p augmentagent-phone --all-targets
--no-deps -- -D warnings`. The socket integration tests use loopback WebSockets
and scripted agent/provider implementations, not Twilio or native agents. They
are not evidence of live TLS routing, model session continuity or audible latency.

Next implementation work: daemon startup/maintenance, persisted origin context,
shared agent-turn integration, conversation-bound MCP tools, early private chat
command routing for all three platforms, configuration/runbook, CI, then live
qualification and legacy retirement. Keep the goal and issue open until the
entire acceptance map is proved.

## Native-session and MCP checkpoint

The phone agent bridge now enters `run_surface_turn` with the persisted origin.
That common runner serializes phone and text before loading the native binding
or claiming a turn. A scripted-provider test runs text → three phone turns →
text for both Claude and Codex and observes one session; a concurrent-turn test
observes at most one native turn executing. These are offline harness tests,
not live model/tool/approval qualification.

The private Unix-socket tool broker issues revocable owner/conversation grants.
The `phone-tool` CLI subcommand provides bounded stdio MCP frames; its schemas
accept no destination, origin or verification-code fields. The agent option
adapter preserves other tools and only adds phone tools inside trusted transport
context. `WikiQuerier` retains the grant for the provider call, but its endpoint
still needs installation at daemon startup. No production surface is claimed
connected by this checkpoint.

The structured private setup command implementation consumes numbers and codes
without returning them in replies. The actual chat routers still must intercept
these **before** inbox persistence, history capture or model invocation. In
particular, Slack currently persists an event before owner dispatch, so adding
phone handling only in its existing command dispatcher would violate AC15.
WhatsApp's existing CLI runtime is not wired and its control path ignores
outbound messages; an authenticated self-chat adapter is still required.

Test-first records from this workspace:

| Behavior | Failing run | Passing run |
| --- | --- | --- |
| Shared text/phone queue | `/tmp/1496-serialization-red.log` (concurrent native turn marked uncertain) | `/tmp/1496-serialization-green.log` |
| Persisted immutable call origin | `/tmp/1496-origin-red.log` (missing API) | `/tmp/1496-origin-green.log` |
| Private MCP grant/number commands | `/tmp/1496-mcp-red.log` (missing API) | `/tmp/1496-mcp-green.log` |
| Same native session through phone | `/tmp/1496-agent-bridge-red.log` (missing bridge) | `/tmp/1496-agent-bridge-green.log` |
| Bounded MCP facade and error handling | `/tmp/1496-mcp-facade-red.log` (missing module) | `/tmp/1496-mcp-facade-green.log` |
| Scoped tool injection | `/tmp/1496-tools-injection-red.log` (missing adapter) | `/tmp/1496-tools-injection-green.log` |

The `/tmp` logs are local development evidence. Required CI and live receipts
remain outstanding. AC06 still requires real tool and approval behavior; AC08
and AC13 still require actual Discord/Slack/WhatsApp adapter coverage. Runtime
watchdog scheduling, provider-duration accounting, configuration, durable mirror
adapters, live qualification and legacy retirement also remain open.

Validation for this checkpoint: 36 targeted integration tests passed (11 store,
13 phone service, 2 MCP facade, 2 phone-agent bridge/configuration, 8 common
session runner), plus `cargo check -p augmentagent-cli` and phone-crate Clippy
with warnings denied. Existing unrelated CLI/docs warnings remain in the CLI
check. This is Linux local validation only.

## Runtime, configuration and CI checkpoint

`PhoneRuntime` owns the phone-only listener, private tool broker and watchdog.
The runtime test holds one provider hangup indefinitely while another call reaches
its acceptance deadline: the second hangup begins within one second. Behavioral
red tests also found duplicate cleanup attempts from disconnect and explicit
stop/removal; all paths now share in-flight cleanup guards and durable retries.
The test confirms restart stop marking, rejection of starts after shutdown, and
an unrelated HTTP route returning 404.

Configuration tests cover default-off, missing/invalid settings, loopback-only
binding and explicit identity links without inventing authentication bindings.
The configuration/runtime contract and remaining deployment work are documented
in [runtime.md](runtime.md). The actual daemon and chat adapters still need wiring.

Red/green records: `/tmp/1496-runtime-{red,green}.log`,
`/tmp/1496-concurrent-cleanup-{red,green}.log`,
`/tmp/1496-stop-paths-{red,green}.log`, `/tmp/1496-config-{red,green}.log`, and
`/tmp/1496-ci-runner-{red,green}.log`. The cleanup regressions failed behaviorally;
new runtime/config APIs first failed compilation. The CI runner's test rejects
empty, skipped, filtered, short and failed results. The phone workflow adds
Linux/macOS offline jobs; actual remote execution remains unverified.

Local Linux validation for this checkpoint: the required runner passed all nine
suites / 45 Rust tests (`/tmp/1496-phone-offline/receipt.json`), its Python
validation test passed, `cargo check -p augmentagent-cli --locked` passed with
preexisting warnings, and phone Clippy passed with warnings denied. Remote
macOS/Linux workflow results and live evidence are still absent.
