# Discord voice implementation record (issue #1220)

Status: in progress. This record distinguishes observed evidence from required live gates.

## Production baseline

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

## First implementation slice

A durable `discord_conversations` table stores the actual native session identity separately from audit IDs. Its primary key is `(guild_id, channel_id)`, and `(provider, native_session_id)` is unique. Identical rebinds are idempotent. A different session for the same conversation is rejected. This is storage only; the existing text query path does not yet use it.

The sidecar now has a strict version-1 Unix-socket frame parser, mode-0600 IPC listener, generation-scoped gateway adapter, and one-binding-per-guild voice connection coordinator. Its `start` call can ask the Rust-owned gateway to join a voice channel, and `stop`/owner departure destroys the local connection. Rust forwarding, provider streams, playback, and commands are not yet wired; these tests use fake gateway/connection objects.

TDD evidence: `cargo test -p augmentagent-store --test discord_conversation` failed first with unresolved `DiscordConversation` and missing `Store` methods. After the schema and API were added, all three tests passed.

## Required next gates

1. Prove native create/submit/resume for Codex and Claude in disposable sessions, including actual session IDs and one writer. Do not use the current audit ID as proof.
2. Implement the shared scheduler and route production text through it without weakening model selection, tool scope, or approvals.
3. Prove Discord DAVE join, owner audio receive/decode, and outbound playback in a test guild before relying on the proposed gateway adapter. Confirm Node 24 runtime and Opus/FFmpeg support on the target host.
4. Complete both streaming provider pairs, command UX, MCP speech tools, interruption, recovery, and the issue's AC01–AC12 evidence. Do not merge or close the issue until those gates pass.

## Rollback

The new table is additive and not yet used by production traffic. To roll back this slice, deploy the previous binary. The table can remain inert. If removing data is required after exporting any bindings, stop the daemon and run `DROP TABLE discord_conversations` against a backup/copy first; never do that as part of an automatic downgrade.
