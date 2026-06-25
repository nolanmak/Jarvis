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
    "socialapi": { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "telegram":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "twitter":   { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "voice":     { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] },
    "whatsapp":  { "configured": false, "armed": false, "accounts": 0, "last_poll_unix": null, "needs": ["login"] }
  },
  "queue": { "pending": 0 },
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

- `configured` (boolean): the daemon found enough creds to consider this
  channel set up. The probe is best-effort per channel — gmail wants a
  Composio key plus at least one row in the gmail-accounts table;
  gdrive counts active drive accounts; the rest probe their canonical
  sqlite config key (with env-var fallback).
- `armed` (boolean): the user's arming gate. Reads the per-channel
  arming gate from the sqlite `config` table (set by `augmentagent
  channel <name> arm`), falling back to the matching env var; sqlite
  wins on conflict. Channels without an arming gate report `false`. The
  skill must not write through this field — bumping it on the client
  side will be ignored by the daemon.
- `accounts` (integer): connected-account count. Populated for
  `gmail`, `gdrive`, and `socialapi`; `0` everywhere else until
  per-channel last-poll tables land.
- `last_poll_unix` (integer or null): unix-seconds timestamp of the most
  recent successful poll. Always `null` today; reserved for #7.
- `needs` (array of strings): what's missing. `["login"]` when
  `configured=false`, `[]` otherwise. The schema reserves room for
  richer entries (`"refresh_token"`, `"webhook_url"`, etc.) that future
  PRs may add — the skill must treat unknown strings as opaque and
  surface them verbatim.

### queue

- `pending` (integer): number of rows in `actions` with status
  `pending`. Comes from `Store::pending_reply_count()`.

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

## Related issues

- Issue #1: `status` aggregator implementation, owns the schema
  producer (`crates/augmentagent-cli/src/status.rs`).
- Issue #5: this skill, owns the schema consumer.
- Issue #14: cross-cutting snapshot test
  (`crates/augmentagent-cli/tests/status_schema.rs`) that pins the
  producer against this document so the two stay in sync. The
  snapshot's `.snap` file is checked into the repo and reviewers
  must accept the diff whenever the producer changes.
