# Discord model switching

Tracking: #1136–#1143. The first implementation uses the Rust `FallbackReasoner` and its constrained Codex CLI bridge for Codex, Qwen and GLM. The selected model supplies turns; the bridge remains responsible for declared tools, workspace scope, approvals, handoff journals, audit and command isolation. Qwen and GLM have distinct `ProviderKind` identities even though they use the Codex CLI as transport.

`/model` is currently a text command in a Discord DM or the configured query channel, intercepted after owner authorization and before history, attachment download or inference. It is not registered as a native Discord slash interaction. The equivalent `model` prefix is accepted. Commands are `/model list`, `/model status`, `/model set qwen|glm|codex`, `/model reset`, and `scope:default` on set/reset. The default scope is the current channel or thread. A call snapshots its model selection; switching affects subsequent calls. The persisted state is `~/.config/augmentagent/model-selection.json` or `AUGMENTAGENT_MODEL_SELECTION_CONFIG`, and it contains profile names only, never credentials. The existing `model-router.json` retains the 9Router inference key. `AUGMENTAGENT_MODEL_QWEN_ENABLED=1` and `AUGMENTAGENT_MODEL_GLM_ENABLED=1` allow their respective profiles to be selected after each endpoint passes live verification; leave them unset while the endpoints are paused.

Selecting `codex` uses the existing native Codex login when one is available, even when 9Router is configured for Runpod. A 9Router Codex account is used only when native Codex authentication is unavailable. Selection is refused if the active Jarvis process has no corresponding provider entry; configure it and restart the daemon first.

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

Model quality, context size and latency differ. Full feature parity is a release gate: every row needs deterministic fake-model tests and a live receipt from the actual Jarvis host for all three models. A text response or catalog entry does not establish tool execution. GLM is paused until live inference succeeds. Qwen passed a 9Router Responses text call, a function-call/result round trip, and a Codex CLI text probe on 2026-09-18. The full Jarvis bridge workflow and actual-host Discord test remain the release gate.

Independent automated review uses native Claude or Codex transport with a separate login; a 9Router account label is not evidence of a different backend. Reviewer calls pin direct routing for both passes. If native Codex authentication disappears before dispatch, review fails closed instead of using a gateway account.

## Operator notes

The Runpod adapter source is in `scripts/runpod-adapter`. It normalizes 9Router Chat Completions content parts into Ollama requests and emits Chat Completions SSE. It is deployed separately from Jarvis; production packaging and actual-host acceptance are tracked in #1143. The local gateway at `macbook-air-2.tailfdbc7f.ts.net` depends on that Mac staying awake and connected to Docker and Tailscale. Move the gateway to the actual always-on Jarvis host for unattended work.

The adapter stores prompt-free job state in `RUNPOD_ADAPTER_JOURNAL` (default `/app/state/jobs.sqlite3`). Mount `/app/state` on owner-private persistent storage so an adapter restart retains Runpod job IDs and unresolved submissions. A caller may send a stable `Idempotency-Key` for one logical turn; repeating it returns HTTP 409 without another Runpod submission. The adapter returns `X-Adapter-Request-Id`, and authenticated `GET /v1/jobs/REQUEST_ID` reports the recorded state. `SUBMISSION_UNKNOWN`, `POLL_UNKNOWN`, `RESULT_UNKNOWN`, and `CANCELLATION_UNKNOWN` require operator reconciliation. `CANCELLED` means the queue API explicitly confirmed that job ID; closing a load-balancer stream is recorded as `CANCELLATION_UNSUPPORTED`. A client or 9Router path that does not forward the same idempotency key on retry does not yet have duplicate-submission protection.
