# status --json Schema, Version 1

The `augmentagent status --json` command emits a single JSON object on
stdout. This file locks the shape of that object at `schema_version: 1`.
The setup skill consumes this contract; any breaking change must bump the
version number, and the skill is allowed to refuse to parse a version it
does not recognize.

## Full shape

```
{
  "schema_version": 1,
  "generated_at": "2026-05-21T14:52:00Z",
  "host": {
    "os": "linux",
    "hostname": "augmentagent-box",
    "user": "nolanmak",
    "binary_path": "/home/nolanmak/.cargo/bin/augmentagent",
    "binary_version": "0.x.y",
    "build_stamp": "2026-05-21T13:00:00Z"
  },
  "state": "fresh" | "partial" | "ok" | "repair",
  "services": [
    {
      "unit": "augmentagent.service",
      "active": true,
      "sub_state": "running",
      "since": "2026-05-21T13:05:00Z",
      "restart_count_24h": 0
    }
  ],
  "channels": [
    {
      "name": "discord",
      "configured": true,
      "armed": true,
      "validated": true,
      "last_validated_at": "2026-05-21T14:00:00Z",
      "last_event_at": "2026-05-21T14:51:30Z",
      "gates": {
        "DISCORD_BOT_TOKEN": "set",
        "DISCORD_CHANNEL_ID": "set"
      },
      "notes": []
    }
  ],
  "dashboard": {
    "installed": true,
    "running": true,
    "port": 3000
  },
  "warnings": [],
  "errors": []
}
```

## Field-by-field meaning

### Top level

- `schema_version` (integer, required). Locked at `1`. The skill must read
  this before any other field and bail with a friendly "skill needs an
  update" message if it does not equal `1`.
- `generated_at` (ISO 8601 UTC string, required). Used only for display;
  the skill must not gate logic on this timestamp.
- `state` (enum, required). One of `fresh`, `partial`, `ok`, `repair`. This
  is the field the skill's Triage Decision branches on. The CLI computes
  this from the other fields; the skill must not re-derive it.

### host

- `os` is always `"linux"` on this deployment. If the skill sees anything
  else, it should refuse to proceed.
- `hostname`, `user`: cosmetic, used in confirmations only.
- `binary_path`: where the CLI was invoked from. Useful when the user has
  multiple builds on PATH.
- `binary_version`: the cargo package version of the CLI.
- `build_stamp`: when the binary was built. Older than 24 hours plus the
  auto-updater stamp older than that is a hint that updates are not flowing.

### services

An array of systemd user units the daemon owns. The setup skill cares about:

- `unit`: full unit name, including `.service`. Pass this to `augmentagent
  service restart --unit <name>` and `augmentagent logs --unit <name>`.
- `active`: boolean from `systemctl is-active`. `false` on a configured
  install is what tips `state` toward `repair`.
- `sub_state`: the systemd sub-state string (`running`, `dead`,
  `auto-restart`, and so on). The skill surfaces this verbatim.
- `since`: when the current run started. A `since` value newer than
  `generated_at` minus a few minutes means the unit just restarted.
- `restart_count_24h`: integer. Greater than zero on a unit that should be
  stable indicates flapping; the skill should route to Repair.

### channels

The list the user actually cares about. One entry per known channel; the
list is generated from the channel registry inside the daemon, not from
`.env.example`. If a channel is not listed, the daemon does not know about
it.

- `name`: short channel name, lowercase, matches what the CLI accepts as
  `--channel <name>` and `channel <name>`.
- `configured`: the daemon found enough env or stored credentials to
  consider this channel set up.
- `armed`: the user's arming gate is on. A channel can be `configured=true`
  and `armed=false`, which is the normal "off but ready" state.
- `validated`: the last live-credential check passed. `last_validated_at`
  is `null` until the channel has ever been validated.
- `last_validated_at`, `last_event_at`: ISO 8601 timestamps or `null`. The
  skill displays them in human form when reporting status.
- `gates`: object mapping env var name to `"set"`, `"unset"`, or
  `"invalid"`. The skill reads this to build the per-channel checklist
  without re-parsing `.env` itself.
- `notes`: array of free-form strings the daemon attaches. Surface them
  verbatim; do not try to interpret.

### dashboard

The optional dashboard sidecar. Present even when not installed so the
skill can branch on `installed`.

- `installed`: whether `install-dashboard.sh` has run.
- `running`: whether the dashboard unit is active.
- `port`: from `DASHBOARD_PORT`, default 3000.

### warnings, errors

Two arrays of objects. Each object has `code` (string), `message` (string),
and an optional `hint` (string). `warnings` should not affect `state`;
`errors` typically drives `state` to `repair`. The skill surfaces both
verbatim; it never re-wraps the message.

## How the skill consumes each field

The skill reads the JSON exactly once per invocation and threads the parsed
object into its decision tree:

- `schema_version` gates everything. Wrong version, bail.
- `state` picks the Triage branch (Fresh, Partial, Repair, Maintenance).
- `services` populates the "restart which unit" prompt when the user asks
  to restart, and feeds the Repair branch when any unit is inactive.
- `channels` drives the Maintenance Menu's per-channel actions and the
  Partial branch's gap analysis. The skill never re-derives `armed` or
  `validated` from env; it trusts these fields.
- `gates` is the bridge between `.env.example` (the docs) and the running
  daemon (the truth). When the skill needs to tell the user "set X in
  .env", it cross-references `gates` against the channel's documented
  requirements.
- `dashboard` is informational unless the user explicitly asks about it.
- `warnings` and `errors` are surfaced verbatim under a "Notes from the
  daemon" header.

## Stability promise

`schema_version: 1` means:

- The top-level keys listed above are present (or `null` for optional
  scalar fields like `last_validated_at`).
- The enums on `state`, `sub_state`, and `gates` values are append-only;
  new variants may appear, existing ones do not change meaning.
- Field types do not change. Strings stay strings, integers stay integers.
- Adding new keys to existing objects is allowed and is NOT a breaking
  change; the skill must ignore unknown keys.
- Renaming a key, removing a key, or changing a type IS a breaking change
  and the CLI must bump `schema_version` to `2`.

The skill is allowed to assume version 1 fields exist when version 1 is
reported. It must not assume any field outside this document exists.

## Related issues

- Issue #1: status aggregator implementation, owns the schema producer.
- Issue #5: this skill, owns the schema consumer.
- Issue #14: cross-cutting snapshot test that pins the schema against this
  document so the producer and consumer stay in sync.
