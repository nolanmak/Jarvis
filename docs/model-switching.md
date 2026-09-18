# Discord model switching

Tracking: #1136–#1143. The first implementation uses the Rust `FallbackReasoner` and its constrained Codex CLI bridge for Codex, Qwen and GLM. The selected model supplies turns; the bridge remains responsible for declared tools, workspace scope, approvals, handoff journals, audit and command isolation. Qwen and GLM have distinct `ProviderKind` identities even though they use the Codex CLI as transport.

`/model` is currently a text command in a Discord DM or the configured query channel, intercepted after owner authorization and before history, attachment download or inference. It is not registered as a native Discord slash interaction. The equivalent `model` prefix is accepted. Commands are `/model list`, `/model status`, `/model set qwen|glm|codex`, `/model reset`, and `scope:default` on set/reset. The default scope is the current channel or thread. A call snapshots its model selection; switching affects subsequent calls. The persisted state is `~/.config/augmentagent/model-selection.json` or `AUGMENTAGENT_MODEL_SELECTION_CONFIG`, and it contains profile names only, never credentials. The existing `model-router.json` retains the 9Router inference key. `AUGMENTAGENT_MODEL_QWEN_ENABLED=1` and `AUGMENTAGENT_MODEL_GLM_ENABLED=1` allow their respective profiles to be selected after each endpoint passes live verification; leave them unset while the endpoints are paused.

Selecting `codex` uses the existing native Codex login when one is available, even when 9Router is configured for Runpod. A 9Router Codex account is used only when native Codex authentication is unavailable. Selection is refused if the active Jarvis process has no corresponding provider entry; configure it and restart the daemon first.

`/model status` reports the effective profile, whether it comes from the conversation or daemon default, its readiness or pause reason, and whether fallback is disabled. It checks configuration only; it does not start a Runpod worker.
The same runtime pause flags are checked immediately before dispatch. If an operator pauses Qwen or GLM after a selection was persisted, the selected call fails before inference and cannot fall through to Codex. Unpinned automatic chains skip paused Runpod entries.

## Execution paths and parity gate

| Surface | Existing owner | Required model-switch check |
| --- | --- | --- |
| Discord ask, follow-up history, attachments | approval-discord handler + `WikiQuerier` | Switch mid-conversation; preserve context and original attachments |
| File/wiki read and write, shell, MCP | Codex constrained bridge and policy | Same tool inventory, scopes, guards and audit for each profile |
| Code mode and build VM | `Reasoner` defaults + bridge | Approved edit/test loop under each profile |
| Approvals and outbound delivery | Discord broker + persisted actions | Selection cannot bypass approval or duplicate a write |
| Scheduled loops and background calls | reasoner presets / fallback | Default vs pinned job semantics and restart persistence |
| Self-improve and independent review | fallback and review history | Actual model identity and independent author/reviewer eligibility |
| Node Agents SDK path | `src/agent.ts` | Confirm deployment use; route through shared policy if active |

The executable gate inventory below records current evidence and names what
still needs a test. An ignored opt-in fixture is not a CI receipt. Deterministic
coverage does not substitute for the live three-model Discord acceptance run.

The common MCP bridge keeps bounded receipts for JSON-RPC tool call IDs in one
bridge process. Resending the same call returns its prior result when the reply
fits the receipt bound; reusing an ID with different arguments is refused. If
execution may have happened but the reply was lost or too large to retain, a
repeat returns an inspection-required tool error. Restart
recovery still depends on the persistent handoff and approval journals and
remains a separate parity gate.

| Feature | Responsible code | Deterministic receipt | Remaining release gate |
| --- | --- | --- | --- |
| Selection, restart persistence, pause and snapshot | `model_selection.rs`, `fallback.rs` | `model_selection::tests`, `paused_persisted_runpod_selection_never_reaches_inference_or_fallback` | Discord set/call race on the daemon host |
| Owner-only Discord command and conversation history | `event_handler.rs`, `WikiQuerier` | Parser and store tests in `model_selection::tests`; model-control history exclusion test in `event_handler::tests` | Fake Discord sequence and live Qwen→GLM→Codex continuity |
| File scope, shell, image bytes, unknown tools, malformed arguments and local MCP | `codex.rs`, `codex_tools.rs`, `scripts/codex-tool-bridge.py` | Linux `all_selected_profiles_share_scoped_file_tools_and_audit_identity`; bridge duplicate-call and lost-reply tests; `selftest_tool_probe_requires_audited_read_for_each_profile`; pinned 9Router Chat and Responses parallel tool-result fixture | Live model-selected tool use for each provider |
| Document/PDF attachment and computer use | `images.rs`, bridge Read, remote worker tools | Ignored Codex-only opt-in fixtures in `codex.rs`; patched 9Router opt-in fixture refuses unsupported current/history images | Cross-profile deterministic and live attachment/computer-use fixtures; Qwen is currently declared text-only at the gateway and returns 422 for images |
| Retrieval, wiki and memory MCP | `mcp.rs`, wiki and memory tools | Ignored Codex-only `live_wiki_query_profile_executes_files_and_memory_mcp`; no shared CI fixture | Three-profile fake and live memory continuity |
| Approval, drafts and outbound delivery | Discord approval broker, tool audit and journals | Existing approval/broker suites | Three-profile denied/approved edit and delivery receipts |
| Code mode and build VM | `codex_tools.rs`, build runner | Ignored Codex-only VM fixture; no shared CI fixture | Three-profile approved edit/test through the VM |
| Scheduled loops and explicit job pins | Loop runner, `FallbackReasoner` | `user_loop_model_pin_survives_reopen_and_legacy_rows_inherit`, `legacy_user_loops_schema_adds_nullable_model_column`, `discord_loop_captures_selected_model_for_future_runs`, `occurrence_identity_survives_restart_and_advances_after_recorded_run`, `run_create_pins_model_and_rejects_other_profiles` | Live selected-model loop tool/approval run and daemon restart |
| Independent review | `review_history.rs`, `self_improve.rs` | Native-only reviewer admission and dispatch tests | Actual reviewer/backend lineage receipt |
| Runpod retry, cancellation and restart | `scripts/runpod-adapter/server.py` | `python3 -m unittest -q test_server.py`; unkeyed retry and legacy journal migration; pinned 9Router image account/key/409/media CI gate | Real queue cancel and restart; repeat gateway test on daemon host |
| Host deployment configuration | `scripts/runpod-adapter/verify_deployment.py` | `python3 -m unittest -q test_verify_deployment.py`; local catalog-only preflight | Repeat on actual daemon host, then run paid cold/warm inference |
| Node Agents SDK entry point | `src/index.ts` imports `src/agent.ts` for mail triage and dashboard query; dashboard router | Router and dashboard tests | Determine whether the Node process runs on the daemon host; migrate its separate tool/approval path if active |

Model quality, context size and latency differ. Full feature parity is a release gate: every row needs deterministic fake-model tests and a live receipt from the actual Jarvis host for all three models. A text response or catalog entry does not establish tool execution. GLM is paused until live inference succeeds. Qwen passed a 9Router Responses text call, a function-call/result round trip, and a Codex CLI text probe on 2026-09-18. The full Jarvis bridge workflow and actual-host Discord test remain the release gate.

For recurring tasks, `augmentagent loop create --model qwen|glm|codex` pins each
future occurrence to that profile. Omitting `--model` inherits the daemon
default when the loop runs. A Discord `/loop` created under a conversation
`/model` override captures that override; a loop created with only the daemon
default continues to inherit the default. The natural-language loop parser
uses the selected profile for the creation turn. `loop list` shows a pin or
`default`. Existing stored loops migrate with no pin and retain their prior
default behavior. A pinned profile that is paused fails closed when due.

On 2026-09-18, a fresh 4×H200 CUDA 13.0 aggregate capacity read showed Low
stock at $14.36/hour, but the Secure Cloud pool showed Out. One direct GLM
inference probe waited about 90 seconds and allocated zero workers. The client
request was interrupted; the endpoint was restored to min/max workers zero,
CUDA minimum 12.8 and its prior idle timeout. A follow-up worker read showed
zero workers. This attempt did not produce model output, so GLM remains paused.

A later 2026-09-18 capacity read showed Secure Cloud 4×H200 and 8×H100
unavailable. Qwen's queue endpoint was changed from request-count to queue-delay
scaling so Runpod v2 could store `workers.idleTimeout: 5` alongside min/max 0.
For one bounded probe, max was raised to 1 and a direct async job returned
`READY` on the selected GGUF (`delayTime: 77.2 s`, `executionTime: 7.3 s`).
The endpoint nevertheless reported three A40 workers, then three IDLE workers
more than 12 seconds after completion. A worker restarted and downloaded the
16.8 GiB GGUF from Hugging Face again because no Runpod model-cache reference
was attached. Max was restored to 0, and a follow-up read showed zero workers.
Automatic scale-to-zero and cache-hit behavior therefore remain rollout gates;
leave Qwen paused until those pass on the actual operational configuration.
Runpod's model-cache reference was tested while the endpoint was paused. It
pins the entire Hugging Face repository, which currently holds about 362 GiB
across 27 files, rather than the chosen 16.8 GiB GGUF alone. The one queued
cache probe was cancelled before inference; its reference was cleared, max
workers restored to 0, and the worker list returned to zero. A single-file
cache or persistent-volume plan needs a separate cost and cold-start check.

Independent automated review uses native Claude or Codex transport with a separate login; a 9Router account label is not evidence of a different backend. Reviewer calls pin direct routing for both passes. If native Codex authentication disappears before dispatch, review fails closed instead of using a gateway account.

## Operator notes

The Runpod adapter source is in `scripts/runpod-adapter`. It normalizes 9Router Chat Completions content parts into Ollama requests and emits Chat Completions SSE. It is deployed separately from Jarvis; production packaging and actual-host acceptance are tracked in #1143. The local gateway on this Mac depends on that Mac staying awake and connected to Docker and Tailscale. Move the gateway to the actual always-on Jarvis host for unattended work.

If Jarvis runs on another tailnet machine while the gateway remains on this Mac, set its router `base_url` to the `/v1` URL shown by `tailscale serve status` and set `AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS` to that exact host and port in the daemon environment. Remote URLs require this exact host and port; cleartext is accepted only for a `.ts.net` name. A separately allowlisted HTTPS host is also accepted. The router key remains in the private router config. Cross-host redirect handling in the downstream CLI still needs an acceptance probe before treating remote routing as production ready.

The adapter stores prompt-free job state in `RUNPOD_ADAPTER_JOURNAL` (default `/app/state/jobs.sqlite3`). Mount `/app/state` on owner-private persistent storage so an adapter restart retains Runpod job IDs and unresolved submissions. A caller may send a stable `Idempotency-Key` for one logical turn; repeating it returns HTTP 409 without another Runpod submission. Stock 9Router 0.5.75 dropped that header and translated an upstream 409 into 503. `sidecars/9router/runpod-reconciliation.patch` fixes both on the pinned 9Router source and pins an explicit OpenAI-compatible model so 9Router cannot switch to its capacity adapter. It returns 422 before upstream inference when a current or historical attachment needs a capability the selected model does not declare; its Docker image and Linux installer are described in `docs/model-router.md`. The synthetic installed-router tests passed against the patched local image on 2026-09-18, including unchanged account quota failover and visible attachment refusal. The actual Jarvis host must run that patched build and pass the same checks. The adapter also journals an HMAC digest of each request body (keyed with its private adapter key). It refuses an identical unkeyed request while an earlier job is unresolved, and for one hour after a recorded completion. This survives adapter restarts and prevents immediate gateway/client retries from launching another paid job. An intentional identical request in that hour will also be refused; changed request bodies and adapter-key rotation do not share this guard. The adapter itself returns 409 for an uncertain submission or result rather than a retryable 502.

The adapter returns `X-Adapter-Request-Id`, and authenticated `GET /v1/jobs/REQUEST_ID` reports the recorded state. Authenticated `POST /v1/jobs/REQUEST_ID/reconcile` checks Runpod using the endpoint saved with the job, and updates the state only when Runpod returns the matching job ID. A submission with no known job ID remains unresolved and returns 409. Authenticated `POST /v1/jobs/REQUEST_ID/cancel` requests queue cancellation and reports `CANCELLED` only after Runpod confirms the matching job ID; an ambiguous submission reports `CANCELLATION_UNKNOWN`, and a load-balancer job reports `CANCELLATION_UNSUPPORTED`. If cancellation arrives while a queue submission is in flight, the journal records the intent, captures the eventual job ID, and immediately sends one cancellation request for it. `SUBMISSION_UNKNOWN`, `POLL_UNKNOWN`, `RESULT_UNKNOWN`, and `CANCELLATION_UNKNOWN` require operator reconciliation. Closing a load-balancer stream is also recorded as `CANCELLATION_UNSUPPORTED`.
Queue polling now also requires the returned Runpod job ID to match the
submitted one. Completed, failed, cancelled and timed-out journal states are
terminal under concurrent poll/reconcile/cancel responses: a stale response
cannot replace a confirmed outcome. If cancellation wins while a poll is in
flight, the adapter returns a conflict instead of delivering stale model
output; a mismatched status ID leaves `POLL_UNKNOWN` for inspection. A stale
queued/running status or poll error cannot erase an outstanding cancellation
request or its unresolved result.

Definite pre-submission Runpod HTTP rejections retain their 401/403
authentication, 429 rate-limit or 400/404/422 request status with fixed redacted
messages. A 5xx or lost response may have accepted work and remains an
unresolved 409 in the adapter journal. This distinction applies to both queue
and load-balancer routes; the patched local 9Router preserves 409, and the
daemon-host gateway still needs the same acceptance test.

The adapter's authenticated Runpod client accepts only HTTPS URLs on `api.runpod.ai` or one endpoint subdomain of `api.runpod.ai`, with no URL credentials, nonstandard port, query or fragment. It rejects HTTP redirects. Local regression tests prove an unauthorized URL and a redirect target receive no request, so a changed upstream location cannot carry the Runpod key to a different host.
Both Qwen queue requests and GLM load-balancer requests cap generated tokens to the route's `max_output_tokens` (default 2048) before reaching Runpod. This bounds output length, not GPU cost or wall time.
For Qwen, `tool_choice: none` removes tool definitions from the Ollama request.
`auto` leaves them available. Forced or required tool choices return a local
400 before any Runpod submission because [Ollama's chat API](https://docs.ollama.com/api/chat)
defines `tools` but no `tool_choice` control. This restriction is explicit in
the adapter fixture; a later upstream capability change needs a new live test.
The Qwen adapter also requires each tool result to match one pending assistant
call ID and tool name. It orders parallel results to match their calls before
translation to Ollama and refuses missing, duplicate or forged results with
HTTP 400 before creating a Runpod job. The three-profile live parity gate
still has to prove the model can use those results correctly.

Before enabling either profile on a host, run the read-only preflight with that
host's owner-private `adapter.env` and `router-client.env` files:

```sh
python3 scripts/runpod-adapter/verify_deployment.py \
  --adapter-env /private/path/adapter.env \
  --router-env /private/path/router-client.env
```

It requires the Runpod/admin and client keys to be present, checks the adapter
health and both authenticated model catalogs, and refuses redirects. It sends
only GET requests to `/health` and `/models`; a passing result does not establish
that the paused GPU endpoints can serve inference. A remote router host must
be listed explicitly with `--allow-router-host HOST:PORT` (and in the daemon's
router allowlist). The verifier's missing-secret, unreachable-router and healthy
fake-service cases run in CI.

After activating an endpoint and setting its `AUGMENTAGENT_MODEL_*_ENABLED=1`
flag on the Jarvis host, use `augmentagent reasoner-selftest --profile qwen
--prompt 'Reply READY'` (or `glm` or `codex`) to probe exactly that profile
through the production reasoner. The option affects only this process and does
not persist a Discord selection. It performs inference and can incur Runpod
GPU time. Use it only after deliberate activation; the read-only preflight
above is the safe first check. The Linux fake-CLI CI test verifies that each
choice bypasses the default chain and records its provider/model identity,
but it does not certify live tools or quality.

For a real tool-use probe, run `augmentagent reasoner-selftest --profile qwen
--tool-probe` (and repeat with `glm` and `codex`). The command creates a
temporary synthetic file, gives the selected model only the Jarvis `Read`
tool, and requires both an exact file-content answer and a successful
selected-provider `Read` audit receipt. It writes that receipt to the normal
tool audit log, or `AUGMENTAGENT_TOOL_AUDIT_LOG` when set. This performs paid
inference for Runpod profiles. The Linux CI fixture runs a fake model through
the packaged bridge and proves the command rejects an answer that bypassed
the tool, but only a live run can prove the actual model chooses it.
