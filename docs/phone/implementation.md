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
