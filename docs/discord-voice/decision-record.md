# Discord voice implementation record (issue #1220)

Status: in progress. This record distinguishes observed evidence from required live gates.

The current AC01–AC12 coverage and missing proof are tracked in [acceptance-map.md](acceptance-map.md).

## Baseline at start of issue

- Base revision: `fb0bbdbb63a3fddc2065f3c95b08732dd0b49a62` (`github/main`).
- Rust `augmentagent-approval-discord` owns the production Discord gateway. Its `GatewayIntents` currently omit `GUILD_VOICE_STATES`; the handler processes text and interaction events.
- Text queries invoke `WikiQuerier::answer`, which calls the configured `FallbackReasoner`. The `AuditCtx.session_id` is `channel_id:message_id`, an audit identifier. A recent Discord message history is copied into each prompt. There is no native CLI session continuity for this path.
- The current Codex adapter passes `--ephemeral` on every call. Native Codex continuation requires a separate session-aware path that omits this flag, captures `thread.started.thread_id`, then calls `codex exec resume`. The Claude adapter currently creates a fresh print-mode invocation each turn; a session-aware path must pass `--session-id` on creation and `--resume` thereafter while preserving its existing settings, MCP, working-directory, and audit behavior. A bound session must not silently use fallback to another provider.
- Rust `augmentagent-channel-voice` handles Telegram voice memos, not live Discord audio.
- Host probe: Node `v22.22.1`, npm `10.9.4`, Codex CLI `0.156.1`, Claude Code `2.1.281`, Cargo `1.94.1`. `ffmpeg` was absent from PATH.
- The current `@discordjs/voice` documentation requires Node >=24.17.0 and calls audio receive undocumented by Discord. A separate pinned Node 24 runtime for the sidecar is proposed; replacing the dashboard's Node 22 globally is unnecessary.
- Discord's official voice documentation requires gateway voice state/server events and a separate voice connection. The Rust gateway should remain the sole bot gateway owner; the sidecar must use a custom adapter, with Rust forwarding the event and send surfaces.
- The host's private `.env` has a configured Discord bot token and owner allowlist. It has neither `DEEPGRAM_API_KEY` nor `ELEVENLABS_API_KEY` configured there. Other secret stores have not been checked. No credential values were printed.
- The sidecar uses a pinned, local Node `24.17.0` binary so the existing Node 22 dashboard can continue unchanged. Its pinned `@discordjs/voice` `0.19.2` and DAVE Linux x64 GNU native binary load under that runtime. `npm ci`, typecheck, and the sidecar tests passed. `opusscript` and `omit=optional` avoid a vulnerable unused native Opus install; the DAVE binary is pinned as a direct dependency so omission of optional packages does not break encrypted calls. `npm audit --omit=optional --audit-level=high` reports zero vulnerabilities. This only proves local package loading and adapter behavior, not a live Discord voice connection.

## Disposable native CLI compatibility probes

Executed on this Linux host in fresh `/tmp` directories, with synthetic prompts and no agent tools. Both calls in each pair exited zero. These probes do not establish production tool/permission parity.

| CLI | Create invocation | Continue invocation | Observed identity |
| --- | --- | --- | --- |
| Codex 0.156.1 | `codex exec --json --skip-git-repo-check -s read-only -C <temp> <prompt>` | `codex exec resume --json --skip-git-repo-check <thread-id> <prompt>` | `thread.started.thread_id` was `01a0e4b4-fb0b-7e52-b7ce-05db92396078` on both turns; both exact synthetic responses returned. |
| Claude 2.1.281 | `claude -p --model haiku --tools '' --output-format json --session-id <uuid> <prompt>` | `claude -p --model haiku --tools '' --output-format json --resume <uuid> <prompt>` | `session_id` was `acd0f428-bec9-412c-b025-7243ac58e0ac` on both turns; both exact synthetic responses returned. |

Codex's first resume attempt without `--skip-git-repo-check` exited one because the disposable directory was not a trusted Git repository. The retry with that flag passed. The temporary probe logs are local, untracked artifacts.

## Current implementation

A durable `discord_conversations` table stores the actual native session identity separately from audit IDs. Its primary key is `(guild_id, channel_id)`, and `(provider, native_session_id)` is unique. Identical rebinds are idempotent. A different session for the same conversation is rejected. The production text query path uses this binding when `AUGMENTAGENT_DISCORD_VOICE_ENABLED=1`. A durable turn-claim ledger marks uncertain turns before CLI execution so a daemon restart cannot silently replay a tool call. Text and finalized speech share a per-conversation queue and native session lease.

The Rust gateway forwards binding-scoped voice state/server events to a private Node 24 sidecar. `/voice start`, `status`, `stop`, and `interrupt` are wired from the existing text conversation. The sidecar has a strict version-1 Unix-socket frame parser, a generation-scoped gateway adapter, one binding per guild, owner-only receive, Deepgram/ElevenLabs STT and TTS adapters, an interruptible playback queue, and speech receipts. The Rust bridge mirrors committed transcripts to the originating text channel, submits them to the same native session scheduler, mirrors replies, and queues unspoken final replies. These paths have deterministic fake-transport tests. They have **not** passed a live Discord DAVE/audio/provider test yet.

An ephemeral second Unix socket exposes `speak({text, utterance_id})`, `speech_status`, `voice_status`, and `voice_interrupt` to the native CLIs. The server issues a random grant only for an owner-authorized active binding, checks its guild/conversation/generation on every call, and revokes it after the turn. The model does not supply a Discord target. The server prefixes utterance IDs with the grant and refuses a changed text for a reused ID; the sidecar also deduplicates playback. The literal utterance ID `final` marks that turn's final output, so its normal final answer is not played twice. Codex receives the grant through its stdio MCP environment; Claude receives it in a private temporary MCP config file, not in process arguments. Tool speech is mirrored to the originating text channel, and a mirror failure is visible as `mirrored:false` in the receipt response.

The sidecar now checks an existing socket before startup: it reclaims a same-user stale Unix socket, but refuses a live listener or a regular file. A checked-in Linux user unit runs the pinned Node 24 runtime with a provider-only environment file, and the daemon defaults to `$XDG_RUNTIME_DIR/augmentagent/discord-voice.sock` when the feature is on. The Rust bridge retries a missing startup socket or a dropped live socket three times with 1/2/4-second delays. A dropped connection clears the old binding and pending IPC requests before retrying; reconnect never automatically rejoins voice or replays a turn. Focused tests cover delayed sidecar startup, recovery with an explicit new start, and exhausted retries. A real process-crash/restart and provider-stream recovery remain live gates.

TDD evidence: `cargo test -p augmentagent-store --test discord_conversation` failed first with unresolved `DiscordConversation` and missing `Store` methods. After the schema and API were added, all three tests passed.

For the speech-tool boundary, `cargo test -p augmentagent-approval-discord --test voice_tool_contract` initially failed because the `voice_tool` module did not exist. The passing contract now covers a wrong conversation, invalid/expired grant, target spoofing, duplicate utterance ID without a second sidecar `speak`, changed text for a reused ID, stop invalidation, receipt, and status. `voice_bridge::tests::final_output_delivered_by_tool_is_not_played_again` exercises the final-output marker. `discord_voice_session_tests::active_voice_binding_injects_tool_only_into_its_owner_conversation_turn` proves the CLI query handler injects tools only for the active owner conversation.

### Native speech-tool compatibility probe (2026-09-27)

Disposable, ephemeral native CLI calls used a synthetic local Unix-socket endpoint that returned `state=probe-listening` or a queued receipt. They did **not** contact Discord, Deepgram, or ElevenLabs. The endpoint saw one `voice_status` and one `speak({text:"SYNTHETIC_VOICE_1220",utterance_id:"final"})` from each CLI. Codex `0.156.1` reported completed `mcp_tool_call` events for `voice_status` and `speak`, then returned `STATUS=probe-listening` and `RECEIPT=queued`. Claude `2.1.281` initialized with the `voice` MCP server connected, used `ToolSearch` to discover each tool, called both, then returned the same status and receipt markers. The fake endpoint was stopped after the probes. This verifies native discovery/call transport for both installed CLIs, not audible output or actual provider compatibility.

The selected tool calls and results are checked in as [native-mcp-probe-2026-09-27.json](native-mcp-probe-2026-09-27.json). The raw temporary CLI logs and synthetic fixture socket were removed after extraction.

The sidecar also passes a 50-cycle fake-connection attach/detach test with exactly 50 destroys and no retained binding after each stop. This covers the coordinator handle boundary only. It does not yet prove that 50 real Discord receivers, provider sockets, and playback jobs are released.

## Required next gates

1. Prove Discord DAVE join, owner audio receive/decode, and outbound playback in a test guild. Mocked adapter and native MCP tests cannot satisfy this gate.
2. Prove Deepgram and ElevenLabs STT/TTS live, including a mixed pair. Provider keys are absent from the checked daemon `.env`; do not count contract tests as live compatibility.
3. Complete the audio fixtures, measured interruption/latency gates, provider-stream recovery and real process-crash/restart proof, 50-cycle cleanup, and full acceptance-ID evidence. Confirm typed/voice approval and attachment parity in the live path.
4. Confirm the new sidecar/Rust CI workflow is green and complete full CLI QA. The Linux service, config example, setup guide, read-only doctor, and opt-in operator live recorder are checked in. The doctor checks local readiness; the recorder marks observations unverified. Neither is live audio evidence until run in a test guild and corroborated with logs and session traces. Keep the PR draft until AC01–AC12 and CI are green.

## Rollback

The schema is additive and used only when `AUGMENTAGENT_DISCORD_VOICE_ENABLED=1`. To roll back, disable that flag, stop the sidecar, and deploy the previous binary. The native binding and turn-ledger tables may remain inert. If removing data is required after exporting bindings, stop the daemon and use the documented database backup/rollback procedure; never automatically drop a table that may hold a live native session identity.
