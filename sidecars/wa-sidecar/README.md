# AugmentAgent WhatsApp sidecar

Go sidecar (`go.mau.fi/whatsmeow`) that owns the WhatsApp linked-device
session. The Rust daemon talks to it via NDJSON over
`${XDG_RUNTIME_DIR}/augmentagent/wa.sock` — same wire shape as the browser
sidecar (`sidecars/browser/`).

Implements the WhatsApp DM channel and the agent control/approval surface.
(These were tracked as `#12` / `#102` in the archived `nolanmak/AugmentAgent`
repo; those numbers do not map to the same issues in the canonical
[nolanmak/MyAgentAssistant](https://github.com/nolanmak/MyAgentAssistant) repo.)
Rust foundation crate: `crates/augmentagent-channel-whatsapp`.

> **Build status:** the Rust side, the JSON-RPC contract, and mock-socket
> tests are complete and green (`cargo test -p augmentagent-channel-whatsapp`
> — 36 tests). The Go sidecar source is committed but **uncompiled**: this
> host has no Go toolchain. `go mod tidy && go build` once Go is installed
> (tracked as "Go sidecar build pending" — formerly `#74` in the archived
> AugmentAgent repo).

## Layout

```
sidecars/wa-sidecar/
  go.mod      # whatsmeow + sqlite + qr deps (go mod tidy fills go.sum)
  main.go     # UDS NDJSON server, 4 ops, lifecycle events, QR pairing
  setup.sh    # one-shot: go mod tidy + go build -> ./wa-sidecar
  README.md   # this file
```

## Wire protocol

NDJSON over a Unix stream socket. See the `main.go` package doc and
`crates/augmentagent-channel-whatsapp/src/api.rs` for the exact envelope.

**Methods (request/response):** `status`, `list_chats`, `fetch_history`,
`send_text`.

**Events (sidecar-initiated):** `qr`, `pair-success`, `connected`,
`logged-out`, `received-message`.

Typed error kinds: `NotPaired`, `NotConnected`, `SendFailed`, `BadRequest`,
`Internal`.

## Setup (one-time, once Go is on the box)

```bash
./sidecars/wa-sidecar/setup.sh        # go mod tidy + build
```

Then pair a device:

```bash
augmentagent whatsapp login --phone 15551234567
# scan the QR printed on the terminal with the phone's
# WhatsApp -> Linked Devices -> Link a Device
```

The whatsmeow session persists to
`~/.local/state/augmentagent/whatsmeow.db`; subsequent sidecar starts
reconnect silently. A server-side logout emits `logged-out` and the daemon
flips the `whatsapp_devices` row to `logged_out` — re-run `whatsapp login`.

## Ban-risk gate

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

- Single client (the daemon). A new connection replaces the old write
  target; events buffer in the daemon's `mpsc` and drain on each poll.
- The sidecar reconnects whatsmeow internally; if the websocket drops,
  `send_text` returns `NotConnected` and the next inbound event re-arms it.
- Run under systemd as `augmentagent-wa-sidecar.service` (Restart=always),
  mirroring `augmentagent-browser-sidecar.service`.
