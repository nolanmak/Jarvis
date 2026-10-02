# macOS voice services

The Discord voice sidecar and Telegram voice-capture listener run as separate
per-user LaunchAgents. They share neither provider credentials nor a bot token.
The declared native CI target is macOS 15 on Apple Silicon; Intel and real
account round trips remain unverified under [#1255](https://github.com/nolanmak/Jarvis/issues/1255).
LaunchAgents start in a logged-in user session. Unattended startup before login
is tracked by [#1258](https://github.com/nolanmak/Jarvis/issues/1258).

## Discord voice

Use Node 24.17.0 and npm with the checkout at any absolute path. From the
checkout:

```bash
cd sidecars/discord-voice
bash setup.sh
cd ../..
install -d -m 0700 "$HOME/.config/augmentagent"
install -m 0600 sidecars/discord-voice/voice.env.example \
  "$HOME/.config/augmentagent/discord-voice.env"
# Edit the private file with the selected test provider credentials.
python3 scripts/install-sidecar.py discord-voice
augmentagent service --unit augmentagent-discord-voice.service status
augmentagent logs --unit augmentagent-discord-voice.service
```

The installer requires the pinned `node_modules/node/bin/node` runtime, built
JavaScript, and a mode-0600 provider file. Only the Deepgram/ElevenLabs
provider selectors and keys listed in `voice.env.example` are loaded. The bot
token stays with the main daemon, and no secret value goes into a plist or
process argument. The socket is
`/tmp/augmentagent-<uid>/discord-voice.sock`, with an owner-private directory
and a singleton lease. Enable the main daemon's
`AUGMENTAGENT_DISCORD_VOICE_ENABLED=1` only after the sidecar is ready; its Mac
socket default matches the installer. Existing owner allowlists and approval
controls still apply.
The updater rebuilds and restarts this service only when the installed voice
source changes. A failed dependency install or restart withholds its build
stamp for retry.

## Telegram capture

The listener uses the existing CLI and a separate Keychain slot. It needs a
release CLI binary, a local whisper.cpp binary, and the medium.en model:

```bash
cargo build --release --locked -p augmentagent-cli --bin augmentagent
bash scripts/build-whisper.sh
bash scripts/build-whisper.sh --verify-only
python3 scripts/install-sidecar.py telegram-capture
augmentagent service --unit augmentagent-telegram-capture.service status
augmentagent logs --unit augmentagent-telegram-capture.service
```

The builder pins whisper.cpp `v1.9.4` at commit
`927cfce34f31707e17f2bff35c349632fb9e2c3a` and verifies the medium.en
model SHA-256 `cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356`.
It uses CMake with two build jobs by default; set
`AUGMENTAGENT_WHISPER_BUILD_JOBS=1` on a smaller Mac. The model download is
about 1.5 GB. The installer rejects missing native artifacts before changing
the loaded service. Opt in with a controlled test Telegram bot through the
existing `augmentagent voice login` command and keep the chat allowlist in
`~/.config/augmentagent/telegram-allowed-chats.json`. A missing token causes
`voice serve` to exit cleanly, as on Linux.
After a Rust update, the updater restarts a running installed capture listener;
it leaves an installed but idle listener alone.

For either job, use `augmentagent service --unit augmentagent-discord-voice.service restart`
or the corresponding Telegram capture unit after changing credentials or
artifacts. Use `bash scripts/uninstall-sidecar.sh discord-voice` or
`bash scripts/uninstall-sidecar.sh telegram-capture` to remove only that job;
private state, Keychain items, and account sessions remain. Native CI checks
package installability, synthetic audio, socket contracts, and service
rendering. Controlled Discord/Telegram account sessions, consent, login and
sleep/wake recovery still require a real Mac and are not claimed by CI.
