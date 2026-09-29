# `augmentagent status --json` Schema, Version 1

The `augmentagent status --json` command emits a single JSON object on
stdout. This file locks the shape of that object at `schema_version: "1"`.
The `/setup` skill consumes this contract; any breaking change must bump
the version string, and the skill is allowed to refuse to parse a version
it does not recognize.

The pinned snapshot lives at
`crates/augmentagent-cli/tests/snapshots/status_schema__status_v1.snap`.
Treat this document as the human-readable counterpart of that snapshot —
they must agree.

## Full shape

```json
{
  "schema_version": "1",
  "host": "linux",
  "daemon": {
    "unit": "augmentagent.service",
    "active": true,
    "since_unix": 1747856073
  },
  "dashboard": {
    "unit": "augmentagent-dashboard.service",
    "active": true,
    "port": 3000,
    "reachable": true
  },
  "updater": {
    "unit": "augmentagent-update.timer",
    "timer_active": true,
    "last_run_unix": 1747850000
  },
  "core_keys": {
    "composio": true,
    "groq": true,
    "cerebras": false,
    "discord_bot": true
  },
  "channels": {
    "calendar":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "contacts":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "discord":   { "configured": true,  "armed": false, "accounts": 0, "last_poll_unix": null, "needs": [] },
    "gdrive":    { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "github":    { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "gmail":     { "configured": true,  "armed": false, "accounts": 2, "last_poll_unix": null, "needs": [] },
    "instagram": { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "linkedin":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "meetup":    { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "reddit":    { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "slack":     { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "telegram":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "twitter":   { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "voice":     { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "whatsapp":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] }
  },
  "queue": { "pending": 0 },
  "delivery": {
    "discord":  { "inbound_backlog": 0, "inbound_dead_letter": 0, "outbound_backlog": 0, "outbound_retrying": 0, "outbound_reconcile": 0, "outbound_dead_letter": 0 },
    "slack":    { "inbound_backlog": 1, "inbound_dead_letter": 0, "outbound_backlog": 3, "outbound_retrying": 1, "outbound_reconcile": 1, "outbound_dead_letter": 0 },
    "whatsapp": { "inbound_backlog": 0, "inbound_dead_letter": 0, "outbound_backlog": 0, "outbound_retrying": 0, "outbound_reconcile": 0, "outbound_dead_letter": 1 }
  },
  "interactive": {
    "slack": {
      "state": "connected",
      "healthy": true,
      "detail": null,
      "recovery": null,
      "workspaces": ["T00000001"],
      "dry_run": false,
      "last_event_unix": 1747856000,
      "last_send_unix": 1747856002,
      "state_since_unix": 1747850000,
      "heartbeat_unix": 1747856070,
      "app_installed": true,
      "owner_bound": true,
      "credentials": "usable",
      "reconnects": 0
    }
  },
  "credentials": {
    "backend": "macos-keychain",
    "persistent": true,
    "insecure_file_store": false,
    "note": null
  },
  "daemon_report": {
    "pid": 4242,
    "running": true,
    "started_unix": 1747850000,
    "dry_run": false,
    "credential_backend": "macos-keychain",
    "credential_persistent": true,
    "insecure_file_store": false
  },
  "config_issues": [],
  "summary": "ok"
}
```

## Field-by-field meaning

### Top level

- `schema_version` (string, required). Locked at `"1"`. The skill must
  read this before any other field and bail with a friendly "skill needs
  an update" message if it does not equal `"1"`.
- `host` (string, required). Always `"linux"` — AugmentAgent ships on
  Linux only and the CLI does not pretend otherwise. If the skill sees
  anything else, refuse to proceed.
- `summary` (string, required). One of `ok`, `degraded`, `needs_setup`,
  `daemon_down`, `dashboard_down`, `config_invalid`. The CLI also maps
  this to a process exit code:

  | summary          | exit code |
  | ---------------- | --------- |
  | `ok`             | 0         |
  | `degraded`       | 10        |
  | `needs_setup`    | 10        |
  | `daemon_down`    | 20        |
  | `dashboard_down` | 30        |
  | `config_invalid` | 40        |

  The skill branches on `summary` and trusts it; it never re-derives the
  classification from the underlying fields.

### daemon

`systemctl --user show augmentagent.service`.

- `unit` (string): full unit name including `.service`. Pass straight to
  `augmentagent service restart` and `augmentagent logs`.
- `active` (boolean): true iff `ActiveState=active`.
- `since_unix` (integer): `ActiveEnterTimestamp` parsed to a unix epoch
  in seconds. `0` means systemd reported `n/a` or the property was unset
  (treat as "unknown", not "epoch").

### dashboard

`systemctl --user show augmentagent-dashboard.service` plus a 2-second
HTTP probe against `/api/v1/stats`.

- `unit` (string): unit name including `.service`.
- `active` (boolean): true iff the unit is active.
- `port` (integer): `DASHBOARD_PORT` env var or `3000`.
- `reachable` (boolean): true iff the dashboard answered the probe with
  2xx or 401 (an `x-api-key`-gated 401 is real proof-of-life). Net
  errors and timeouts collapse to false.

### updater

`systemctl --user show augmentagent-update.timer`.

- `unit` (string): `augmentagent-update.timer`.
- `timer_active` (boolean): true iff the timer is `active`.
- `last_run_unix` (integer): `ActiveEnterTimestamp` of the timer (when
  it most recently armed), as unix seconds. `0` when never run.

### core_keys

One boolean per top-level credential the daemon needs. A `true` value
means the canonical sqlite `config` row OR the corresponding env var is
set and non-empty. Sqlite wins on conflict, mirroring
`getConfigStatus()` in `src/dashboard.ts`.

- `composio` — `COMPOSIO_API_KEY` / `config.composio_api_key`.
- `groq` — `GROQ_API_KEY` / `config.groq_api_key`.
- `cerebras` — `CEREBRAS_API_KEY` / `config.cerebras_api_key`.
- `discord_bot` — `DISCORD_BOT_TOKEN` / `config.discord_bot_token`.

### channels

An object keyed by channel name (lowercase, matches what
`augmentagent channel <name>` accepts). Locked-in keys, in the order
emitted by `BTreeMap` (alphabetical):

`calendar`, `contacts`, `discord`, `gdrive`, `github`, `gmail`,
`instagram`, `linkedin`, `meetup`, `reddit`, `slack`, `socialapi`,
`telegram`, `twitter`, `voice`, `whatsapp`.

Each value is an object:

- `configured` (boolean): the credential / prerequisite the serve loop
  actually checks is present (#374): keyring slot (e.g.
  `augmentagent/linkedin/default`), legacy credential file (each
  channel's `default_auth_path`, env overrides honoured), or store rows
  (slack workspaces, telegram bots, meetup subscriptions, gmail/drive/
  socialapi accounts).
- `armed` (boolean): the serve daemon would run a poller/listener for
  this channel right now. Always `false` for channels serve never
  spawns — `twitter` and `instagram` (posting/CLI only), `whatsapp`
  (unimplemented), `telegram` (inbound is CLI `poll-once`), `calendar`
  (driven by `augmentagent-calendar.timer`), `contacts` (CLI sync).
  Read-only: derived from the same gates as `configured`, not from the
  legacy config-table arming keys, which serve never consults.
- `accounts` (integer): connected-entity count where one exists in the
  store — `gmail`, `gdrive`, `socialapi` (accounts), `slack`
  (workspaces), `telegram` (bots), `meetup` (subscriptions); `0` for
  credential-only channels.
- `last_poll_unix` (integer or null): unix-seconds timestamp of the most
  recent successful poll. Always `null` today; reserved for #7.
- `needs` (array of strings): what's missing. `["login"]` when
  `configured=false`, `[]` otherwise. #1299: `["credentials_unreadable"]`
  instead of `["login"]` when a keyring slot the channel depends on could
  not be read by this process; such a channel is never reported as
  `configured` or `armed` (a failed read used to count as present). The schema reserves room for
  richer entries (`"refresh_token"`, `"webhook_url"`, etc.) that future
  PRs may add — the skill must treat unknown strings as opaque and
  surface them verbatim.

### queue

- `pending` (integer): number of rows in `actions` with status
  `pending`. Comes from `Store::pending_reply_count()`.

### delivery

Added in #1285 (additive; still schema `"1"`). Durable delivery state per
chat surface from `Store::surface_delivery_counts()`. `discord`, `slack`
and `whatsapp` are always present (zeros when idle); any other platform
with rows in the durable log is added. All values are integers.

- `inbound_backlog`: accepted inbound events not yet handled.
- `inbound_dead_letter`: inbound events that exhausted their attempts.
- `outbound_backlog`: sends not yet settled (queued, sending, retrying or
  awaiting reconcile).
- `outbound_retrying`: part of the backlog waiting for a retry.
- `outbound_reconcile`: part of the backlog that was in flight when the
  daemon stopped; it is never resent until the provider is checked.
- `outbound_dead_letter`: sends that exhausted their retries or failed
  permanently.

`augmentagent doctor` reports a `surface_delivery` warning when any surface
has dead letters or sends awaiting reconcile.

### interactive

Added in #1287 (additive; still schema `"1"`). Live health of each
interactive chat surface, from the report the running daemon writes
(`surface_listener_health`). `slack` is always present. This is separate
from `channels.slack`, which describes Composio ingestion only.

- `state` (string): `not_configured` (no app installed or no owner bound),
  `disabled` (`AUGMENTAGENT_SLACK_INTERACTIVE=0`), `misconfigured` (enabled
  but cannot start), `connecting`, `connected`, `reconnecting`,
  `disconnected` (the listener gave up, or the daemon stopped reporting),
  `stopped` (clean shutdown). Treat unknown values as not healthy.
- `healthy` (boolean): true only for `connected` with a fresh report. A
  live state whose `heartbeat_unix` is more than 60 s old, or (#1299) whose
  reporting daemon pid is no longer running, is reported as
  `disconnected`, never as the state it last claimed.
- `detail` (string or null): why, for the operator. Never a secret.
- `recovery` (string or null): what to do; surface it verbatim.
- `workspaces` (array of strings): Slack team IDs served.
- `dry_run` (boolean): sends are recorded, not made.
- `last_event_unix`, `last_send_unix`, `state_since_unix`,
  `heartbeat_unix` (integer or null): unix seconds.

A surface that is supposed to run (`state` not `not_configured` or
`disabled`) and is not healthy turns an otherwise `ok` summary into
`degraded`. `augmentagent doctor` reports `interactive.slack`: ok when
connected, not configured or disabled; error when misconfigured; warn
otherwise, with `recovery` as the suggested action.

Added in #1299 (additive; still schema `"1"`):

- `app_installed` (boolean or null): the Slack app's install index exists
  (`true`) or does not (`false`) in this process's credential store; `null`
  when this process cannot read the store (for example a macOS session with
  no access to the login Keychain: SSH, or a launchd job before login), so
  it is unknown. Never `true` from a failed read. On macOS the probe reads
  the item, which can show a Keychain prompt for a binary the item does not
  trust.
- `owner_bound` (boolean): an owner binding exists in the database.
- `credentials` (string): `missing` (no install visible to this process),
  `present` (stored, not yet proven readable by the running daemon),
  `unreadable` (this process cannot read the credential store; `detail`
  carries the store's reason and `recovery` points at
  `augmentagent doctor --keychain-probe` and the login-session requirement)
  or `usable` (the daemon's fresh `connected` report proves it read the
  tokens and Slack accepted them). Never `usable` without that proof.
- `reconnects` (integer or null): times the daemon that wrote the report
  entered `reconnecting`; resets when a new daemon process reports; `null`
  without a report.

Without a daemon report, `detail` and `recovery` name the first missing
setup step: `no interactive Slack app is installed` (install, then bind),
`the Slack app is installed but no owner is bound` (bind, then restart), an
owner bound without an install (install), or, with both done, the daemon
has not reported (restart). `doctor` adds `interactive.slack.credentials`
(ok when usable or nothing is installed; warn when present but unproven,
suggesting `augmentagent doctor --keychain-probe` on macOS or a restart on
Linux) and, with `--deep`, `slack_app.scopes` (granted versus required bot
scopes from the stored install record).

### credentials

Added in #1299 (additive). The credential backend of the process that ran
`status` (`augmentagent_auth::describe_default_store`).

- `backend` (string): `macos-keychain`, `platform-keyring`, `keyutils`,
  `keyring-mock` (keyring built without a persistent backend for this OS,
  today's Linux build, #1325) or `insecure-file`
  (`AUGMENTAGENT_INSECURE_CREDENTIAL_DIR`).
- `persistent` (boolean): credentials outlive the process that stored them.
- `insecure_file_store` (boolean): plaintext test store in use.
- `note` (string or null): caveat for the operator.

`doctor` reports the same as `credential_backend`: error for the plaintext
store, warn when not persistent.

### daemon_report

Added in #1299 (additive). `null` until a daemon records a start. What
`serve` recorded when it last started (`daemon_runtime_report` table):

- `pid` (integer), `started_unix` (integer), `dry_run` (boolean).
- `running` (boolean): that pid is alive now. When false the report is
  history and contributes no `config_issues`.
- `credential_backend`, `credential_persistent`, `insecure_file_store`: as
  in `credentials`, for the daemon's own environment.

### config_issues

Added in #1299 (additive). Configuration problems, each with a fix. An
array (possibly empty) of objects:

- `id` (string): `discord.approval_broker` (a `DISCORD_BOT_TOKEN` without a
  numeric `DISCORD_CHANNEL_ID` while approvals are routed to Discord, i.e.
  `AUGMENTAGENT_APPROVAL_SURFACES` unset, `auto` or naming `discord`: serve
  runs without the Discord approval broker), `credentials.insecure_file_store` (this process uses the
  plaintext store), `daemon.insecure_file_store` (the running daemon does),
  `credentials.unreadable` (warn: this process could not read one or more
  credential slots; `detail` lists each `augmentagent/<platform>/<account>`
  with the store's reason).
  Treat unknown ids as opaque.
- `source` (string): `cli` (found from this process's environment, which
  includes `.env` in the working directory, like the daemon's) or `daemon`
  (recorded by the running daemon at startup).
- `severity` (string): `warn` or `error`.
- `detail` (string): what is wrong. Never a secret.
- `recovery` (string or null): what to do; surface it verbatim.

Any entry turns an otherwise `ok` summary into `degraded`. `doctor`
reports one `config.<id>` finding per entry with `recovery` as the
suggested action, or a single ok `config_issues` finding when empty.

## Stability promise

`schema_version: "1"` means:

- Every top-level key listed above is present.
- Every field type stays put. Booleans stay booleans, integers stay
  integers (note: `schema_version` is a STRING, not an integer).
- Every channel name listed above is present in `channels`. The
  `--channel <name>` flag may narrow the map at runtime; the snapshot
  test covers the unfiltered case.
- Adding a new key to an existing object is NOT a breaking change —
  the skill must ignore unknown keys.
- Renaming a key, removing a key, changing a type, or removing a
  channel from `channels` IS a breaking change and the CLI must bump
  `schema_version` to `"2"`.

## How the skill consumes each field

- `schema_version` gates everything. Wrong version, bail.
- `summary` picks the Triage branch. `ok` → Maintenance; `daemon_down`
  / `dashboard_down` → Repair; `needs_setup` / `degraded` → Partial.
- `daemon`, `dashboard`, `updater` populate the systemd panel. The skill
  surfaces `unit` names verbatim when telling the user which unit to
  restart.
- `core_keys` drives the "credentials" checklist on the Partial branch.
- `channels` drives the Maintenance Menu's per-channel actions and the
  Partial branch's gap analysis. The skill trusts `configured` /
  `armed` rather than re-deriving them.
- `queue.pending` is informational unless the user explicitly asks
  about it.
- `interactive.<surface>` drives the "is Jarvis listening" answer: trust
  `healthy`, show `detail` and `recovery` when it is false.
- `config_issues` are listed with their `recovery` before anything else on
  the Partial branch; `credentials.insecure_file_store` or
  `daemon_report.insecure_file_store` true is always called out.

## Related issues

- Issue #1: `status` aggregator implementation, owns the schema
  producer (`crates/augmentagent-cli/src/status.rs`).
- Issue #5: this skill, owns the schema consumer.
- Issue #14: cross-cutting snapshot test
  (`crates/augmentagent-cli/tests/status_schema.rs`) that pins the
  producer against this document so the two stay in sync. The
  snapshot's `.snap` file is checked into the repo and reviewers
  must accept the diff whenever the producer changes.
