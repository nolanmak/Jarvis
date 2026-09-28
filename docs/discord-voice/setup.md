# Discord voice test-guild setup (issue #1220)

This is a **test-guild procedure**, not a release claim. Live DAVE receive/playback, both providers, interruption, latency, and the other gates in issue #1220 still require measured evidence before this feature can be enabled in production.

The existing Rust daemon remains the only Discord bot gateway owner. A separate Node 24 process handles audio over a private Unix socket. The sidecar needs Deepgram and/or ElevenLabs keys, but never needs `DISCORD_BOT_TOKEN`.

## Build and configure

From the deployment checkout (`~/AugmentAgent` in the supplied user unit):

```sh
cd ~/AugmentAgent/sidecars/discord-voice
npm ci
npm run typecheck
npm test
npm run build
./node_modules/node/bin/node --version  # pinned 24.17.0
```

Create `~/.config/augmentagent/discord-voice.env` from `sidecars/discord-voice/voice.env.example`, fill only the selected provider keys, and set its mode to `0600`. `AUGMENTAGENT_DISCORD_STT_PROVIDER` and `AUGMENTAGENT_DISCORD_TTS_PROVIDER` are independent (`deepgram` or `elevenlabs`); the defaults are Deepgram. ElevenLabs TTS also requires `ELEVENLABS_VOICE_ID`. Do not put the bot token in this file.

```sh
cd ~/AugmentAgent
mkdir -p ~/.config/augmentagent ~/.config/systemd/user
install -m 0600 sidecars/discord-voice/voice.env.example ~/.config/augmentagent/discord-voice.env
cp systemd/augmentagent-discord-voice.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now augmentagent-discord-voice.service
systemctl --user status augmentagent-discord-voice.service
```

The checked-in unit assumes this repository lives at `~/AugmentAgent`; adjust `WorkingDirectory` and `ExecStart` in a user override if it does not. `ExecStartPre` creates `%t/augmentagent` as a private directory, and the sidecar creates a mode-0600 socket at `%t/augmentagent/discord-voice.sock`.

After the voice unit is running, run its read-only doctor with the same provider environment. It checks the pinned Node runtime, selected provider keys without printing them, socket ownership/mode, and an active local listener. A pass means the service is ready for a **live test**, not that Discord audio or a provider has been verified.

```sh
cd ~/AugmentAgent/sidecars/discord-voice
npm run build
./node_modules/node/bin/node --env-file="$HOME/.config/augmentagent/discord-voice.env" dist/scripts/doctor.js
```

For the daemon, set `AUGMENTAGENT_DISCORD_VOICE_ENABLED=1` in its private `.env`, keep the existing owner allowlist, and restart `augmentagent.service` after the sidecar is listening. By default the daemon uses `$XDG_RUNTIME_DIR/augmentagent/discord-voice.sock`. If it runs outside a user systemd session, set `AUGMENTAGENT_DISCORD_VOICE_SOCKET` to the same **absolute** path as the sidecar. There is no root-owned `/run/augmentagent` fallback. To order startup without making text depend on audio, a user drop-in for `augmentagent.service` may add `After=augmentagent-discord-voice.service` and `Wants=augmentagent-discord-voice.service`.

## Test-guild UX

Use the configured owner account and an existing guild text channel/thread. Join an existing regular voice channel, then run `/voice start` in the text conversation. The command reports the native agent and session ID and the joining state; `/voice status` reports the actual audio state. Spoken transcripts and replies mirror into that same text conversation. Typed messages remain usable and use the same native session. `/voice interrupt` stops the current spoken reply. `/voice stop` disconnects audio while preserving the text/native session. Leaving voice also detaches audio. Starting again from the same text conversation reuses its native session; another text thread is isolated.

Keep the first test session disposable. Do not use a personal long-lived native agent session as a destructive fixture. The bot must have Discord voice connect/speak permissions in the test guild. Use the issue's acceptance checklist to record actual audio, transcript, and session-ID evidence separately from mocked tests.

The opt-in live recorder requires a test guild ID, the existing text channel/thread ID, the existing voice channel ID, an agent, and a disposable run ID. First run it with `--preflight-only` to verify arguments and local service readiness; it does not contact Discord or providers. For an interactive run, add `--output` pointing to a **new** local JSON file and omit `--preflight-only`. It prompts for what you actually observed in Discord, stores the report with mode `0600`, and marks it `operator-recorded-unverified`. Compare it with bot logs, native session traces, and measured audio before checking any acceptance criterion. Run separately for each agent and provider pairing; this script does not manufacture an audible pass.

```sh
cd ~/AugmentAgent/sidecars/discord-voice
npm run build
set -a
. "$HOME/.config/augmentagent/discord-voice.env"
set +a
npm run test:live -- \
  --guild-id TEST_GUILD_ID --text-channel-id TEST_TEXT_CHANNEL_ID \
  --voice-channel-id TEST_VOICE_CHANNEL_ID --agent codex --run-id disposable-codex-01 \
  --preflight-only
# For a real operator-recorded run, omit --preflight-only and add:
# --output "$HOME/voice-test-codex-01.json"
```

## Latency report

For AC09, capture at least 30 **live, no-tool** voice turns for each native agent on the same deployment and network. Keep the source logs, receipt JSON, and a recording or independent endpointing trace with the private test record. Export one JSON object using this shape (Unix epoch milliseconds throughout):

```json
{
  "schemaVersion": 1,
  "deployment": "test-guild-run-01",
  "network": "owner client and server locations / connection",
  "sttProvider": "deepgram",
  "ttsProvider": "elevenlabs",
  "turns": [
    {
      "agent": "codex",
      "turnId": "unique-turn-id",
      "noTool": true,
      "idleAtCommit": true,
      "speakTextChars": 42,
      "speechEndedAtMs": 0,
      "committedAtMs": 0,
      "handlerDispatchedAtMs": 0,
      "nativeSubmittedAtMs": 0,
      "firstTextOutputAtMs": 0,
      "answerCompletedAtMs": 0,
      "queuedAtMs": 0,
      "ttsRequestedAtMs": 0,
      "ttsFirstByteAtMs": 0,
      "firstPlaybackAtMs": 0
    }
  ]
}
```

Replace the zero placeholders with actual increasing timestamps. `speechEndedAtMs` comes from the independent owner audio/endpointing trace; the bot cannot infer physical speech end from the provider's committed transcript. `committedAtMs` is in the transcript event and daemon log. `handlerDispatchedAtMs`, `nativeSubmittedAtMs`, `firstTextOutputAtMs`, and `answerCompletedAtMs` are in the daemon's turn-ID logs. Native submission is recorded after the CLI prompt has been fully written. The remaining fields and `speakTextChars` come from the sidecar speech receipt and spoken text. Match turn IDs across these sources; use a consistent clock or record clock offsets when the owner capture runs on another machine. Include only turns where the scheduler was idle at transcript commit (`idleAtCommit: true`) and where the agent made no tool calls (`noTool: true`); verify both from traces. Keep raw evidence for every sample, including slow outliers.

```sh
cd ~/AugmentAgent/sidecars/discord-voice
npm run report:latency -- "$HOME/voice-timings.json" --output "$HOME/voice-latency-report.json"
```

The command rejects incomplete/nonchronological samples, requires 30 turns per agent, calculates nearest-rank p50/p95 for each stage, and exits nonzero if either required p95 gate fails. The report file is created mode `0600` and labeled `operator-supplied-timings-unverified`; review its source traces before treating a gate as passed. `firstPlaybackAtMs` is the local Discord player transition and does not prove remote audibility. Check the receiver's actual audio separately.

## Troubleshooting and rollback

```sh
journalctl --user -u augmentagent-discord-voice.service -n 100 --no-pager
systemctl --user status augmentagent.service augmentagent-discord-voice.service
ls -l "$XDG_RUNTIME_DIR/augmentagent/discord-voice.sock"
```

- “Join an existing server voice channel” means the owner was not in a regular guild voice channel when `/voice start` ran.
- “Voice sidecar is disconnected” means it was absent at startup or dropped during a call. The daemon tries the socket three times at 1/2/4-second intervals; check the unit, its provider-only environment file, and the socket path. After a successful reconnect, run `/voice start` again from the same text conversation. If all retries fail, restart the daemon after fixing the sidecar. The sidecar safely reclaims a stale socket left by a crashed previous process, but refuses to replace a live listener or a regular file.
- A provider-key error at start means the independently selected STT or TTS provider lacks a key (or ElevenLabs TTS lacks a voice ID). A provider 401/403/429/5xx must be treated as a failed live gate, not a mocked success.
- An “uncertain turn” means the native CLI may have run tools before a failure. Inspect the durable turn ledger and native session before any manual recovery; do not resubmit the same turn automatically.

For rollback, set `AUGMENTAGENT_DISCORD_VOICE_ENABLED=0`, restart the daemon, and stop/disable the voice unit. The text conversation and native-session rows remain available; rollback does not delete them.
