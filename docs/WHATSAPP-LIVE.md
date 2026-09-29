# Live WhatsApp connection (in progress)

Jarvis uses a WhatsApp linked device through the local Go sidecar. It does not
need a WhatsApp API key. Pairing requires a QR scan from the phone that owns
the account. The sidecar's SQLite store holds the actual linked-device keys;
the Rust keyring entry is only a device index.

## Pair an account

1. Build the pinned sidecar: `./sidecars/wa-sidecar/setup.sh`. Set
   `AUGMENTAGENT_WA_SIDECAR_BIN` to that binary if it is not installed as
   `augmentagent-wa-sidecar` on `PATH`. The login command attaches to a running
   sidecar when one exists, or starts a temporary sidecar for pairing.
2. Choose the account layout:
   - **Self-chat:** `augmentagent whatsapp login --phone +15551234567 --self-chat`.
     The linked account's own 1:1 chat is the owner control chat.
   - **Dedicated Jarvis account:** `augmentagent whatsapp login --phone
     +15551234567 --owner-jid 15557654321@s.whatsapp.net`. The owner is the
     other phone; only that 1:1 chat is the control chat.
3. On the phone being linked, open **WhatsApp → Linked devices → Link a device**
   and scan the terminal QR. A refreshed QR replaces the old one. `Ctrl-C` or
   the timeout cancels login without saving a local device index.
4. Run `augmentagent whatsapp status` and `augmentagent whatsapp devices
   --json`. Status reports the sidecar's actual paired/connected state
   separately from the local owner configuration and channel enablement.

On a headless host without an OS keyring, set `AUGMENTAGENT_WHATSAPP_AUTH` to
an auth JSON file path before login. Jarvis writes it atomically with private
file permissions. The sidecar's `AUGMENTAGENT_WA_STORE` file must also persist
across restarts. Neither file belongs in Git.

## Allow chats

Inbound reading requires `augmentagent whatsapp allow-inbound
<chat-jid>`. Outbound sending requires `augmentagent whatsapp allow-outbound
<chat-jid>` and `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED=1`. Use
`deny-inbound` or `deny-outbound` to remove access. `subscribe <chat-jid>
--mode priority|digest|store_only` records a 1:1 chat for the paired account;
`subscriptions --json` and `unsubscribe <id>` manage those entries.

`list-chats --json` lists contacts known to the sidecar. It is not a complete
WhatsApp conversation archive. The separate
[WhatsApp history setup](WHATSAPP-HISTORY.md) covers read-only Desktop exports.

`unlink <phone>` checks the sidecar's exact device identity before logging out
and removing the local index, owner configuration, and subscriptions. Repeating
it for an already absent phone is safe.

## Current readiness

Pairing, account inspection, chat permissions, and account removal are CLI
operations. The long-running `serve` path and `poll-once` are still being
wired to the live channel (#1231). Durable replay and outbox recovery (#1229),
owner command handling (#1230 and later issues), and a supervised sidecar
installation (#1243) are also required before using WhatsApp as a regular
Discord replacement. `status` reports `actively_listening: false` until the
serve path is enabled.
