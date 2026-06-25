# .env File and Channel Arming Gates

`.env` at the repo root is the canonical source of channel arming gates,
feature flags, and the core API keys the daemon needs at startup. The
skill never hand-edits `.env`; writes go through `augmentagent env set
<KEY> <VALUE>` (issue #12, shipped), which persists to the sqlite
`config` table (which wins over `.env` at startup). The skill reads
`.env.example` as a checklist and uses `env set` to apply changes.

## Gate semantics

Every channel that can post, DM, or otherwise act on a real account is
INERT until two things are true:

1. The channel has live credentials (cookies / token / OAuth bundle).
2. The channel's arming gate in `.env` is set true.

A channel with creds but no arming gate logs a "channel inert" line on
startup and quietly skips its write path. A channel with the arming gate
on but no creds errors loudly. Both states are visible via
`augmentagent status --channel <name> --json`.

## Channels with arming gates today

The arming map lives in `crates/augmentagent-cli/src/channel_router.rs`
(`arming_keys_for`). Today's gated channels:

| Channel    | sqlite key                     | env var                                    |
| ---------- | ------------------------------ | ------------------------------------------ |
| instagram  | `instagram_real_account_enabled` | `INSTAGRAM_REAL_ACCOUNT_ENABLED`         |
| twitter    | `twitter_real_enabled`         | `AUGMENTAGENT_TWITTER_REAL_ENABLED`        |
| linkedin   | `linkedin_post_confirm`        | `AUGMENTAGENT_LINKEDIN_POST_CONFIRM`       |
| whatsapp   | `whatsapp_control_enabled`     | `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED`    |

Channels not in this map are on-by-default once credentials are in
place (gmail, gdrive, slack, discord, github, reddit, meetup,
telegram-bot). For the ones that are routable through `augmentagent
channel <name>` (the `ChannelName` variants), the arm/disarm CLI verbs
return "no arming gate"; that is expected.

Note: not every channel that appears in `arming_keys_for` or in the
docs is a `ChannelName` variant. `instagram` has an arming gate but is
NOT a `ChannelName` variant, so `augmentagent channel instagram
arm/disarm` fails today. `deftform`/`deft` and `socialapi` are also NOT
`ChannelName` variants; they use dedicated subcommands (`augmentagent
deft …`, `augmentagent socialapi …`) rather than `augmentagent channel
<name>`.

## arm / disarm verbs

The CLI router exposes per-channel arming via:

```
augmentagent channel <name> arm
augmentagent channel <name> disarm
```

These flip the sqlite config key directly (the same key the dashboard's
`getConfigStatus()` reads, see `src/dashboard.ts`). The JSON receipt
includes `restart_required: true` and `restart_cmd:
"augmentagent service restart"`. The skill should:

1. Run the arm/disarm command.
2. Read the JSON receipt.
3. AskUserQuestion: confirm the restart with the user (quote the
   command back).
4. Run `augmentagent service restart`.
5. Re-run `augmentagent status --channel <name> --json` and confirm
   `armed` flipped.

The sqlite write is the authoritative path; the env var is a fallback
the dashboard reads at startup. If the user has set both and they
disagree, sqlite wins.

## Boolean parse rules

The daemon parses arming gates via `is_truthy` in `channel_router.rs`:

- Truthy: any non-empty string except `0`, `false`, `off`, `no`
  (case-insensitive).
- Falsy: empty string, `0`, `false`, `off`, `no`.

So `1`, `true`, `yes`, `on`, `enabled` all arm. Don't paste fancy values
like `enabled (set to disable)`; the daemon parses them as truthy.

## Core API keys

Independent of channel arming, the daemon needs these in `.env` or in
the sqlite `config` table:

| Env var               | Purpose                                          |
| --------------------- | ------------------------------------------------ |
| `COMPOSIO_API_KEY`    | Gmail, Drive, Slack OAuth + LLM tool calls.      |
| `GROQ_API_KEY`        | Primary LLM provider.                            |
| `CEREBRAS_API_KEY`    | Secondary LLM provider.                          |
| `DISCORD_BOT_TOKEN`   | The bot side of Discord (approval surface).      |

`augmentagent status --json` surfaces these as booleans under
`core_keys`. A `false` value means both the env var and the sqlite row
are empty.

## Editing the file

The skill never hand-edits `.env`. Instead:

1. Identify the variable that needs to change (read `.env.example` for
   the canonical comment header for each block).
2. Run `augmentagent env set <KEY> <VALUE>`, which upserts the sqlite
   `config` row that wins over `.env` at startup.
3. Run `augmentagent service restart` so the daemon picks up the change.

`augmentagent env set` / `env unset` (issue #12) is the sanctioned
writer; the skill does not edit `.env` by hand.

## Pitfalls

- Arming stays sticky across restarts: if a channel was armed and the
  env var is later removed, sqlite still holds `true` and the daemon
  still arms. Use `augmentagent channel <name> disarm` to flip sqlite
  too, then restart.
- `.env` is loaded once at daemon start. Edits do not apply until
  restart. There is no SIGHUP support today.
- `.env.example` is the canonical comment header and gate list. If a
  channel name is missing from this file, the daemon does not know about
  it. The `ChannelName` enum in
  `crates/augmentagent-cli/src/channel_router.rs` lists only the
  channels routable through `augmentagent channel <name>`; it is NOT a
  complete enumeration of every channel — gated channels like
  `instagram` and documented ones like `deftform`/`socialapi` are absent
  from it and use dedicated subcommands instead.
