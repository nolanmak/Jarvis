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

On macOS, the sidecar and CLI share `/tmp/augmentagent-<uid>/wa.sock` by
default. Keep `AUGMENTAGENT_WA_STORE` in a persistent private directory;
temporary files are not suitable for the linked-device session. Pairing from a
Terminal does not prove a launchd-run daemon can read that credential; use the
macOS keychain access probe in `doctor` before relying on a managed service.

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

## Run owner text chat

After pairing, allow outbound to the configured owner control chat and set
`AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED=1` in the daemon environment. Run:

```sh
augmentagent --wiki-dir ./wiki serve --no-email true --dry-run false
```

The daemon starts the sidecar when needed and reconnects its local socket.
One paired account per sidecar is supported. It validates the actual device
against the stored binding before consuming messages. Owner text is processed
immediately, independently of the legacy four-hour triage cadence. `cancel`
stops an active request; `reset`/`new` starts a new native conversation; `help`
reports the currently supported commands. Set the default model in the
**Models & accounts** dashboard before opening a native conversation.

Both the owner sender and control chat must match. Self-chat also requires a
verified own-account message that is absent from the sidecar's durable send
ledger. Unknown origins, other accounts, agent echoes, groups, edits, revocations,
view-once and disappearing messages are denied. Private LID addresses use only
whatsmeow's authenticated phone-number alternate; missing alternates grant no
owner authority. Revoking the binding or outbound permission blocks delivery.

The daemon commits a turn claim before calling the agent and saves the answer
before sending. Restart reuses the saved answer; an interrupted turn is reported
and never silently rerun. Each reply chunk has a stable idempotency key. The
sidecar commits a message ID before network I/O, deduplicates confirmed sends,
and refuses ambiguous resends after a timeout/crash. Delivery receipts and own
message echoes can resolve a pending send. Inspect one without resending:

```sh
augmentagent whatsapp delivery-status 'reply:<chat-jid>:<incoming-message-id>:0'
```

`status` reports `actively_listening` only while a connected, non-dry-run
listener heartbeat is fresh. Pairing or a running sidecar alone is insufficient.
`--dry-run true` does not enable interactive replies.

## Remaining acceptance and features

The owner text path, durable inbound replay, outbound deduplication, echo gate,
and CLI account management have synthetic tests on Linux and Apple Silicon.
Real phone QR pairing and live account round trips are still required. The
sidecar protocol now requires `idempotency_key` for sends; upgrade the Rust
binary and Go sidecar together.

WhatsApp approval cards, recipient composition, files, voice, scheduled task
output, subscribed-chat triage in `serve`, and full command parity remain open.
The legacy `poll-once` CLI is not yet connected to the live consumer. This is
not yet a complete Discord replacement; these gaps and restart/network/sleep
soak acceptance remain part of #1225.
