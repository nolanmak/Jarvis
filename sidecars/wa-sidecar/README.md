# AugmentAgent WhatsApp sidecar

Go sidecar (`go.mau.fi/whatsmeow`) that owns the WhatsApp linked-device
session. The Rust daemon talks to it via NDJSON over
`${XDG_RUNTIME_DIR}/augmentagent/wa.sock` — same wire shape as the browser
sidecar (`sidecars/browser/`).

Implements [#12](https://github.com/nolanmak/AugmentAgent/issues/12) (DM
channel) and [#102](https://github.com/nolanmak/AugmentAgent/issues/102)
(agent control/approval surface). Rust foundation crate:
`crates/augmentagent-channel-whatsapp`.

The sidecar builds with the pinned Go toolchain and committed `go.sum`. The
Rust contract suite launches the compiled Go process in `--offline-test` mode
without connecting to a WhatsApp account. Live pairing and daemon wiring are
tracked in [Jarvis #1228](https://github.com/nolanmak/Jarvis/issues/1228)
and [#1231](https://github.com/nolanmak/Jarvis/issues/1231).

On macOS, run `./sidecars/wa-sidecar/setup.sh` and then
`python3 scripts/install-sidecar.py wa-sidecar` from the repo root. The
LaunchAgent keeps the linked-device store across restarts and removal. See
[`docs/MACOS-SIDECARS.md`](../../docs/MACOS-SIDECARS.md) for status and logs.

## Layout

```
sidecars/wa-sidecar/
  go.mod      # pinned whatsmeow + sqlite dependencies
  go.sum      # committed dependency checksums
  main.go     # UDS NDJSON server, 4 ops, lifecycle events, QR pairing
  setup.sh    # one-shot: verified go build -> ./wa-sidecar
  README.md   # this file
```

## Wire protocol

NDJSON over a Unix stream socket. See the `main.go` package doc and
`crates/augmentagent-channel-whatsapp/src/api.rs` for the exact envelope.

**Methods (request/response):** `status`, `list_chats`, `fetch_history`,
`send_text`.

**Events (sidecar-initiated):** `qr`, `pair-success`, `connected`,
`logged-out`, `received-message`, `receipt`. Message events include optional
quote, mentions, media description and edit/revoke flags. Media download and
durable receipt handling are tracked in the parity epic.

Every request, response and event carries `"version":1`. Unknown versions
return `BadRequest`; malformed frames are rejected. `fetch_history` currently
returns `Unavailable` because the sidecar has no durable history store. It
never reports a false empty history. Typed errors also include `NotPaired`,
`NotConnected`, `SendFailed` and `Internal`.

## Build and offline contract test

```bash
./sidecars/wa-sidecar/setup.sh        # verified build from pinned modules
cd sidecars/wa-sidecar && go test ./... && go test -race ./... && go vet ./...
AUGMENTAGENT_WA_SIDECAR_TEST_BIN="$PWD/wa-sidecar" \
  cargo test -p augmentagent-channel-whatsapp --test sidecar_contract
```

The pairing CLI is not yet implemented. The sidecar buffers its latest QR for
a connecting client and never prints the pairing secret to service logs.

The whatsmeow session persists to
`~/.local/state/augmentagent/whatsmeow.db`; subsequent sidecar starts
reconnect silently. A server-side logout emits `logged-out`; #1228 wires the
CLI and device-state reconciliation.

## Ban-risk gate (#40 / #74 / #102)

WhatsApp bans bot-like accounts aggressively. The channel is conservative:

- **Inbound** is triaged only for chats explicitly opted in via
  `augmentagent whatsapp allow-inbound <chat_jid>`
  (`whatsapp_inbound_allowlist`).
- **Outbound** (including the control surface) additionally requires both
  `whatsapp allow-outbound <chat_jid>` and the global kill-switch env
  `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED=1`. The control surface further
  restricts sends to a single designated control chat (the user's self-chat
  or a dedicated thread).

## Operational notes

- The sidecar serves the daemon and CLI concurrently. Requests return only to
  their requesting client; live events reach all connected clients. Durable
  replay after a client disconnects remains tracked in #1229.
- Starting a second sidecar fails while the socket is active; only a stale
  socket is removed on startup.
- The sidecar reconnects whatsmeow internally; if the websocket drops,
  `send_text` returns `NotConnected` and the next inbound event re-arms it.
- Supervised installation and health checks are tracked in #1243.
