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

For the daemon, set `AUGMENTAGENT_DISCORD_VOICE_ENABLED=1` in its private `.env`, keep the existing owner allowlist, and restart `augmentagent.service` after the sidecar is listening. By default the daemon uses `$XDG_RUNTIME_DIR/augmentagent/discord-voice.sock`. If it runs outside a user systemd session, set `AUGMENTAGENT_DISCORD_VOICE_SOCKET` to the same **absolute** path as the sidecar. There is no root-owned `/run/augmentagent` fallback. To order startup without making text depend on audio, a user drop-in for `augmentagent.service` may add `After=augmentagent-discord-voice.service` and `Wants=augmentagent-discord-voice.service`.

## Test-guild UX

Use the configured owner account and an existing guild text channel/thread. Join an existing regular voice channel, then run `/voice start` in the text conversation. The command reports the native agent and session ID and the joining state; `/voice status` reports the actual audio state. Spoken transcripts and replies mirror into that same text conversation. Typed messages remain usable and use the same native session. `/voice interrupt` stops the current spoken reply. `/voice stop` disconnects audio while preserving the text/native session. Leaving voice also detaches audio. Starting again from the same text conversation reuses its native session; another text thread is isolated.

Keep the first test session disposable. Do not use a personal long-lived native agent session as a destructive fixture. The bot must have Discord voice connect/speak permissions in the test guild. Use the issue's acceptance checklist to record actual audio, transcript, and session-ID evidence separately from mocked tests.

## Troubleshooting and rollback

```sh
journalctl --user -u augmentagent-discord-voice.service -n 100 --no-pager
systemctl --user status augmentagent.service augmentagent-discord-voice.service
ls -l "$XDG_RUNTIME_DIR/augmentagent/discord-voice.sock"
```

- “Join an existing server voice channel” means the owner was not in a regular guild voice channel when `/voice start` ran.
- “Voice sidecar is unavailable” means the daemon could not connect to the socket at startup. Check the unit, its provider-only environment file, the socket path, and service ordering, then restart the daemon. The sidecar safely reclaims a stale socket left by a crashed previous process, but refuses to replace a live listener or a regular file.
- A provider-key error at start means the independently selected STT or TTS provider lacks a key (or ElevenLabs TTS lacks a voice ID). A provider 401/403/429/5xx must be treated as a failed live gate, not a mocked success.
- An “uncertain turn” means the native CLI may have run tools before a failure. Inspect the durable turn ledger and native session before any manual recovery; do not resubmit the same turn automatically.

For rollback, set `AUGMENTAGENT_DISCORD_VOICE_ENABLED=0`, restart the daemon, and stop/disable the voice unit. The text conversation and native-session rows remain available; rollback does not delete them.
