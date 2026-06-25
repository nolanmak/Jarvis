# AugmentAgent Setup Skill

You are invoked when the user types `/setup` (or otherwise asks Claude Code to
configure, repair, or maintain their AugmentAgent install on this Linux host).
Your job is to run `augmentagent status --json` first, branch on what it
reports, and then guide the user through the matching path: fresh install,
partial install, repair, or routine maintenance. The CLI is the source of
truth. Never assume from memory; re-run `augmentagent status --json` at the
top of every invocation, and surface any stderr from the CLI verbatim.

This skill targets the AugmentAgent daemon on a single Linux host. macOS,
launchd, the sparsebundle vault, Homebrew, and `pm2` are not in scope. The
installer scripts and systemd units assume a user-mode systemd session.

A user-side shim at `~/.claude/commands/setup.md` should forward `/setup` to
this skill. That shim lives outside this repo and is not created here; if the
user does not have one yet, tell them to add a one-line file that points at
`skills/augmentagent-setup/SKILL.md`.

## Triage Decision

Every invocation starts the same way. Read the output of `augmentagent status
--json` and use the `state` field to pick exactly one branch below. If the
CLI fails to run, jump straight to `reference/troubleshooting.md` and surface
the stderr to the user; do not attempt to guess.

Top-of-tree command, run first, always:

```
augmentagent status --json
```

If the binary is missing, tell the user to run `cargo build --release -p
augmentagent-cli` and stop. If it returns non-zero with a parseable JSON body,
read `summary` and route. If it returns non-zero with no JSON and stderr
contains an sqlite error, jump to the First-run bootstrap caveat below; the
db may not exist yet. Otherwise surface stderr and consult troubleshooting.

The skill keys on `summary` (the canonical classification, see
`reference/status-schema.md`). Mapping:

| `summary`         | branch              |
| ----------------- | ------------------- |
| `ok`              | MAINTENANCE         |
| `needs_setup`     | FRESH or PARTIAL    |
| `degraded`        | PARTIAL or REPAIR   |
| `daemon_down`     | REPAIR              |
| `dashboard_down`  | REPAIR              |
| `config_invalid`  | REPAIR              |

Distinguish FRESH from PARTIAL by counting channels with `configured ==
true`: zero is fresh, one or more is partial. Distinguish PARTIAL from
REPAIR by looking at `daemon.active` and `dashboard.active` (both true is
partial; either false is repair).

The four branches:

### FRESH INSTALL, not configured

Enter this branch when `summary == "needs_setup"` and every channel reports
`configured = false`, or when `augmentagent status --json` fails outright
because the sqlite db does not exist yet. See the First-run bootstrap caveat
below; on a truly fresh box you must run `augmentagent install dashboard`
BEFORE the first `status --json` call.

Confirm with AskUserQuestion before each side effect; this branch installs
systemd units and touches the keyring.

Step-by-step:

1. Bootstrap the db and dashboard (always first, even if status worked).

   ```
   augmentagent install dashboard
   ```

   This idempotent script creates `~/.config/systemd/user/augmentagent-dashboard.service`,
   initialises the sqlite db on first start, and binds the OAuth callback
   port. Re-run is safe.

2. Re-run `augmentagent status --json`. If it still fails, surface stderr
   verbatim and jump to `reference/troubleshooting.md` under `sqlite_open`.
   Otherwise read the channel set and continue.

3. Confirm the core API keys with the user. Read `core_keys` from the status
   JSON. For every key that reports `false` (`composio`, `groq`, `cerebras`,
   `discord_bot`), tell the user which env var to add to `.env`, point at
   `.env.example` for the canonical block header, and offer to write it via:

   ```
   augmentagent env set <KEY> <VALUE>
   ```

   The env subcommand writes to the sqlite `config` table (which wins over
   `.env` at startup), so this works even before the user edits `.env`.
   AskUserQuestion masks the value when the key matches one of `KEY`,
   `TOKEN`, `SECRET`, `PASSWORD`, `PASS`, `AUTH` (the same rule
   `augmentagent env list` uses).

4. Present channel selection grouped by tier via AskUserQuestion. Three
   groups, multi-select within each, the user picks which channels to wire
   up now and which to defer.

   - Required (the daemon barely functions without these):
     `gmail`, `discord`.
   - Recommended (most installs want these):
     `slack`, `drive`, `github`, `reddit`.
   - Optional (channel-specific, often ban-risk or niche):
     `twitter`, `linkedin`, `instagram`, `meetup`, `telegram-bot`,
     `deftform`.

   Channels the user defers stay `configured = false` in status; they can
   be wired later via `/setup --fix <channel>`.

5. Loop the selected channels. For each `<name>`, READ
   `channels/<name>.md` and execute its Steps section. The sub-file is the
   runbook; the loop is just iteration. Order the loop by category so
   browser-driven cookie-harvest channels run last (they need a graphical
   session, the rest do not).

   Callback-OAuth channels (gmail, drive, slack, reddit) use the OAuth
   orchestration block below rather than the legacy browser-paste flow.

6. Install the remaining systemd units. Confirm each with AskUserQuestion;
   any of these can be re-run safely.

   ```
   augmentagent install autostart
   augmentagent install autoupdate
   augmentagent install digest
   ```

   `autostart` enables `augmentagent.service` (the main daemon).
   `autoupdate` enables the 5-minute `augmentagent-update.timer` which
   runs `scripts/check-for-updates.sh`. `digest` enables the daily
   `augmentagent-digest.timer` (reads `AUGMENTAGENT_DIGEST_HOUR` /
   `AUGMENTAGENT_DIGEST_MINUTE` from the environment).

7. Run the final health check:

   ```
   augmentagent doctor --json
   ```

   Parse the `checks[]` array. For every finding with `severity == "error"`,
   surface its `message` and offer to run `suggested_cmd` (AskUserQuestion,
   one at a time). Warns are informational; print them but do not block.
   Exit when `summary.error == 0`.

8. Final read-only confirmation: `augmentagent status --json` and report
   each `configured = true` channel to the user as a sanity check.

### PARTIAL INSTALL, some channels configured

Enter this branch when `summary` is `needs_setup` or `degraded`, both
`daemon.active` and `dashboard.active` are true, and at least one channel
reports `configured = true`. Some services are up, at least one channel is
on, but at least one more channel either needs creds (`needs` non-empty),
needs arming (in the arming-gates list with `armed = false`), or simply
has not been wired yet.

The flow:

- List the channels with `configured = true` (the working set) and the
  channels with `configured = false` (the gap). Report both back to the
  user.
- AskUserQuestion: "Which of these would you like to wire up now?"
  Multi-select; defer the rest to a later `/setup --fix <name>` call.
- For each selected channel, READ `channels/<name>.md` and run its Steps
  section. Callback-OAuth channels go through the OAuth orchestration
  block; cookie-harvest channels go through the cookie-harvest sub-flow;
  token-paste channels read the token via AskUserQuestion and call
  `augmentagent env set <KEY> <VALUE>`.
- After each channel completes, re-run `augmentagent status --channel
  <name> --json` and confirm `configured = true`.
- When the loop finishes, run `augmentagent doctor --json` for the same
  end-of-flow health check the Fresh-install branch uses.

Do not run install scripts in this branch unless `status` reports a
component as missing (no `autostart` unit, no `dashboard` unit, etc.).
Partial installs are usually missing creds, not units; an unguarded
`install` re-run can clobber working systemd files.

### REPAIR, channel validation failing

Enter this branch when `summary` is `degraded`, `daemon_down`,
`dashboard_down`, or `config_invalid`, OR when at least one channel reports
`configured = true && armed = false`, OR when `needs` is non-empty for any
configured channel. These are the actual repair signals in the schema; the
issue spec's `last_error` shorthand maps to `needs[]` plus stderr from the
matching `augmentagent channel <name> validate` call.

Step-by-step:

1. Run `augmentagent doctor --json` first. It composes status with the
   liveness probes (sqlite integrity, keyring reachability, dashboard,
   binary freshness) and tags each result with a severity. The doctor is
   the source of truth for "what's wrong"; the channel-level repair is
   downstream of fixing the host first.

2. Address `severity == "error"` findings in the order the doctor emits
   them. For each error, surface `name`, `message`, and `suggested_cmd`.
   AskUserQuestion before running any `suggested_cmd`; never run the
   suggestion silently. See `reference/troubleshooting.md` for the per-
   finding "What it means" / "Fix" entries.

3. Once doctor reports `summary.error == 0`, pin the failing channel set.
   Re-run `augmentagent status --json` and build the failing-channel list:

   - Any channel with `configured == true && armed == false` AND the
     channel is in the arming-gates list (see `components/env-file.md`).
     The fix is the arming flow in the sub-file.
   - Any channel with non-empty `needs` (typically `["login"]`). The fix
     is to re-enter the cookie-harvest or OAuth flow.

   Optionally pull recent logs for context: `augmentagent logs --unit
   augmentagent.service` capped at the last 200 lines. Surface CLI stderr
   verbatim.

4. For each pinned channel, READ `channels/<name>.md` and re-enter its
   Steps section from the top. The sub-file is idempotent; re-running it
   on a partially-configured channel either no-ops or refreshes the bad
   credential.

5. Re-validate. Run `augmentagent channel <name> validate` for each
   repaired channel (AskUserQuestion first; this is the only verb in the
   Repair branch that can hit a live third-party service). Confirm the
   `needs` array empties out.

6. Re-run `augmentagent status --json` and confirm `summary == "ok"`.
   If doctor still flags errors, do not loop; report what's left and ask
   the user how to proceed.

Never run `--purge`, `--force`, or `--reset` flags in this branch. Repair is
read-mostly except for the per-channel sub-flow and the doctor's
`suggested_cmd` runs, both of which require explicit AskUserQuestion
confirmation.

### MAINTENANCE, fully configured

Enter this branch when `summary == "ok"` and the JSON shows every channel
the user expects is configured and (where applicable) armed. This is the
green-light branch; the user has a healthy install.

Open the Maintenance Menu (next section) and ask which action the user wants.
Default behavior when the user just says "run setup" with no further detail
is to print the current status summary (channels, services, last-validated
timestamps) and wait.

## Maintenance Menu

These are the Phase 1 verbs. They are safe to expose now because each is
read-only or a narrowly-scoped service action. Drive them with Bash; never
shell out through a shell-builtin wrapper that could expand globs in
unexpected ways.

### Show status

```
augmentagent status
augmentagent status --json
augmentagent status --channel <name>
```

Use the human-readable form (no `--json`) when reporting back to the user.
Use `--json` when the skill itself needs to branch on a field. For a single
channel, pass `--channel <name>` and read only that channel's block. If the
channel is not configured, the CLI exits non-zero; report that as "not
configured" rather than as an error.

### Restart a service

```
augmentagent service restart
augmentagent service restart --unit <name>
```

Without `--unit`, this restarts the main `augmentagent.service`. With
`--unit`, it restarts a specific sidecar (for example the dashboard, the
whatsmeow bridge, or the browser sidecar). Confirm with AskUserQuestion
before running; restarts drop in-flight approval windows.

After restart, re-run `augmentagent status --json` and confirm the unit came
back `active`. If it did not, fall through to the Repair branch.

### Tail logs

```
augmentagent logs
augmentagent logs --unit <name>
augmentagent logs --unit <name> --follow
```

Without `--unit`, this tails the main daemon. With `--follow`, it streams;
warn the user that `--follow` blocks the chat until they interrupt. Default
to a bounded tail (no `--follow`) when triaging.

### Channel subcommands

```
augmentagent channel <name> status
augmentagent channel <name> validate
augmentagent channel <name> recent
```

`status` shows the channel's arming + validation flags. `validate` re-runs
the channel's live-credential check; this is the only Phase 1 verb that can
hit a live third-party service, so confirm with AskUserQuestion first and
warn that some validators count against per-channel rate limits.

`recent` shows the last N events the channel has processed (drafts queued,
messages skipped, approvals issued); use this to sanity-check that a quiet
channel is quiet because the inbox is empty, not because the channel is
silently broken.

## Per-Channel Routing

When the user asks for a channel-specific action (`/setup discord`,
`/setup gmail`, "wire up my LinkedIn"), or when a triage branch lands on
a single channel that needs configuration, READ the matching sub-file
under `channels/` FIRST. The sub-file is the runbook; this top-level
SKILL.md is the decision tree.

Sub-file path is `skills/augmentagent-setup/channels/<name>.md`. Names
match the channel slug the CLI accepts (lowercase, kebab-case for
`telegram-bot`). Each sub-file has the same seven sections: Category,
When to use, Prereqs, Steps, Validate, Common errors and fixes, Disarm /
undo. The Steps section is enough for the skill to execute mechanically.

The 13 channels split into three setup categories:

### callback OAuth (dashboard hosts the callback)

| Channel   | Sub-file                  | Dashboard start URL                                     |
| --------- | ------------------------- | ------------------------------------------------------- |
| gmail     | `channels/gmail.md`       | `http://localhost:<port>/oauth/gmail/start`             |
| drive     | `channels/drive.md`       | `http://localhost:<port>/oauth/googledrive/start`       |
| slack     | `channels/slack.md`       | `http://localhost:<port>/oauth/slack/start`             |
| reddit    | `channels/reddit.md`      | `http://localhost:<port>/oauth/reddit/start`            |
| socialapi | (no sub-file; `augmentagent socialapi`) | `http://localhost:<port>/oauth/socialapi/start`         |

Dashboard must be installed and reachable. Read
`components/systemd-units.md` first if `dashboard.reachable = false`.

### cookie harvest (devtools paste, validated via harvest schema)

| Channel   | Sub-file                  | Schema source                                  |
| --------- | ------------------------- | ---------------------------------------------- |
| discord   | `channels/discord.md`     | `augmentagent setup harvest discord …`         |
| twitter   | `channels/twitter.md`     | `augmentagent setup harvest twitter …`         |
| linkedin  | `channels/linkedin.md`    | `augmentagent setup harvest linkedin …`        |
| instagram | `channels/instagram.md`   | `augmentagent setup harvest instagram …`       |

Every cookie-harvest channel uses the in-skill loop below. Note:
`instagram` is not yet fully wired — its harvest schema emits an
`augmentagent instagram login` next_cmd, but that top-level command does
not exist and `instagram` is not a `ChannelName` variant, so the login
and `channel instagram <op>` steps fail today. See `channels/instagram.md`.

### token paste (user copies a token from a vendor portal)

| Channel       | Sub-file                          | Token source                                     |
| ------------- | --------------------------------- | ------------------------------------------------ |
| github        | `channels/github.md`              | PAT at `https://github.com/settings/tokens`      |
| meetup        | `channels/meetup.md`              | Group urlname (no auth today)                    |
| deftform      | `channels/deftform.md`            | Workspace API token + webhook secret             |
| telegram-bot  | `channels/telegram-bot.md`        | BotFather token                                  |

## First-run bootstrap caveat

As of #33, `augmentagent install dashboard` opens the store after the
install script returns, which creates `$AUGMENTAGENT_DB` (default
`./data.db` relative to the daemon's cwd) and runs migrations. The next
`augmentagent status --json` succeeds on a fresh box without any extra
step.

The Fresh-install branch still runs `augmentagent install dashboard`
BEFORE the first status call — both because that's the sanctioned bootstrap
and because it's idempotent (re-runs are safe). If the very first
`augmentagent status --json` somehow still fails with an sqlite error
(e.g. the install was skipped, or `$AUGMENTAGENT_DB` points at a path the
CLI couldn't open):

1. Surface the stderr verbatim.
2. AskUserQuestion to confirm running `augmentagent install dashboard`.
3. Run it.
4. Re-run `augmentagent status --json`. It should now succeed and report
   `summary == "needs_setup"`.
5. Fall through to the Fresh-install branch.

Do not work around this by guessing at the channel set or by editing
`.env`; the db is the source of truth and the dashboard installer is the
only sanctioned way to initialise it.

## OAuth orchestration

The callback-OAuth channels (gmail, drive, slack, reddit, socialapi)
delegate the browser dance to a single Rust orchestrator. The skill
drives it via:

```
augmentagent setup oauth <provider> --json
```

Providers (the value enum is locked):

- `gmail`     → `/oauth/gmail/start`
- `drive`     → `/oauth/googledrive/start` (the provider slug in JSON
                output is `googledrive`, not `drive`)
- `slack`     → `/oauth/slack/start`
- `reddit`    → `/oauth/reddit/start` (the legacy `/api/reddit/auth`
                path is still served as a backward-compat alias)
- `socialapi` → `/oauth/socialapi/start`

The orchestrator preflights the dashboard, snapshots the current connection
set, opens `xdg-open` (or prints the URL when `$DISPLAY` is empty / when
`--open-browser false` is set), then polls
`/api/v1/oauth/status` every 2s until either the connection set grows or
the timeout expires (`--timeout-secs`, default 300). Stderr carries a
one-line JSON heartbeat per poll; stdout carries the terminal result.

Pseudocode the skill runs per OAuth channel:

```
result_json = run("augmentagent setup oauth " + provider + " --json",
                  stream_stderr_to_log=True)
# result_json is the LAST stdout line (the rest were intermediate URL prints).
status = result_json["status"]
```

Branch on `status`:

- `"connected"`: success. The JSON also carries `provider` and a list of
  accounts (for gmail / drive: `accounts[]` keyed by `id`; for slack:
  `workspaces[]` keyed by `team_id`; for reddit: `connected: true`).
  Continue to the channel's Validate step in its sub-file.
- `"timeout"`: the user did not finish the consent flow in
  `--timeout-secs`. JSON carries `elapsed_secs` and a `hint`. Tell the
  user, then AskUserQuestion whether to retry immediately or defer. Exit
  code is 124.
- `"dashboard_down"`: the dashboard answered the preflight with anything
  other than 200 on `/api/v1/health`. JSON carries
  `suggested_cmd: "augmentagent service start --unit dashboard"`. Run it
  via AskUserQuestion, wait for the unit to come back active, then retry
  the orchestrator. Exit code is 30.
- `"interrupted"`: the user hit Ctrl+C. JSON arrives on STDERR (not
  stdout) and carries `elapsed_secs`. Treat it as a user decision; do not
  retry without asking. Exit code is 130.

Heartbeats on stderr have shape `{"event":"poll","tick":N,
"provider":"<slug>","elapsed_secs":N,"status":"waiting"}` and one-off
poll errors land as `{"event":"poll_error","error":"..."}`. The skill
should surface poll errors but keep waiting; the orchestrator retries
automatically until the timeout.

The orchestrator never edits `.env` or sqlite directly; the dashboard's
existing callback handler writes the account row, and the orchestrator
just detects the diff. After a `"connected"` result, re-run
`augmentagent status --json` and confirm the matching channel flipped to
`configured = true`.

## /setup --doctor

When the user invokes `/setup --doctor` (or asks for "a health check"),
skip triage and run:

```
augmentagent doctor --json
```

`--deep` adds slower probes (one Composio API ping plus a per-channel
validate finding). Only add `--deep` when the user explicitly asks for
the full picture, or when triage already failed and the basic checks were
all green.

Pretty-print the findings as a table. The JSON has the shape:

```
{
  "checks": [
    {"name": "...", "severity": "ok|warn|error", "message": "...",
     "suggested_cmd": "..." | null},
    ...
  ],
  "summary": {"ok": N, "warn": N, "error": N},
  "exit_code": 0 | 1
}
```

Table columns: severity, name, message. Print `suggested_cmd` as a
second indented line under each finding that has one.

For every finding with `severity == "error"`:

1. Surface its `message` verbatim.
2. If `suggested_cmd` is present, AskUserQuestion to offer running it
   (quote the exact command back). Run on confirmation, surface stderr
   verbatim if it fails.
3. Re-run `augmentagent doctor --json` after each fix and confirm the
   finding flipped to `ok` (or moved to a different error).

Warns are informational. Surface them, but do not chain
AskUserQuestion prompts for warns; the user has not signed up for a
remediation walkthrough for non-blocking issues.

See `reference/troubleshooting.md` for the per-finding "What it means" /
"Fix" entries. Known finding names emitted by doctor today:
`status_collect`, `sqlite_open`, `sqlite_migrated`, `keyring_reachable`,
`dashboard_reachable`, `claude_cli_in_path`, `python3_in_path`,
`node_in_path`, `rust_binary_freshness`, `dashboard_build_present`,
`env_file_present`, `socialapi`. With `--deep`: `composio_api` plus one
`channel.<name>.validate` per configured channel.

## /setup --fix CHANNEL

When the user invokes `/setup --fix <name>` (or asks to "fix" or "redo"
one channel), skip triage and the per-channel decision flow. Go straight
to:

1. Run `augmentagent status --channel <name> --json` and confirm the
   channel exists. If the CLI exits non-zero for "channel not configured",
   that is the cue the channel needs a from-scratch enrolment; otherwise
   treat the existing config as broken and prepare to overwrite.
2. READ `channels/<name>.md`. Run its Steps section from the top. The
   sub-file is idempotent.
3. For callback-OAuth channels, run the OAuth orchestration block above
   instead of the sub-file's legacy dashboard-URL paste step.
4. Re-validate with `augmentagent channel <name> validate`
   (AskUserQuestion first; live calls have rate limits and ban-risk on
   `twitter`, `instagram`, `whatsapp`).
5. Re-run `augmentagent status --channel <name> --json` and confirm
   `configured = true` and `needs` is empty. If the channel has an arming
   gate (see `components/env-file.md`), also confirm `armed = true`.

If the user did not pass a `<name>` after `--fix`, AskUserQuestion with a
single-select list of every channel reporting `configured = false` or
non-empty `needs`. Do not loop over channels in `--fix` mode; one channel
per invocation.

## Cookie-harvest sub-flow

For the four cookie-harvest channels (discord, twitter, linkedin,
instagram), the skill runs this loop. The CLI schema emitter from issue
#8 (`augmentagent setup harvest <ch> --non-interactive --json`) is the
authoritative source for fields, hints, and the next command; the skill
parses it instead of hard-coding field lists per channel.

Pseudocode:

```
ch = "<channel>"             # one of discord | twitter | linkedin | instagram
out = "/tmp/" + ch + "-creds-" + pid + ".json"

# 1. Parse the schema.
schema = run("augmentagent setup harvest " + ch +
             " --non-interactive --json --creds-out " + out)
# schema is JSON with: channel, instructions_url, methods[], next_cmd,
# expected_creds_path. Methods carry name, label, script_path,
# doc_steps[], fields[]. Pick methods[0] for the devtools-paste flow.

method = schema.methods[0]
print(method.doc_steps)      # echo verbatim so the user knows where to look

# 2. Ask the user per field. AskUserQuestion masks secrets.
creds = {}
for field in method.fields:
    value = AskUserQuestion(field.label,
                            hint=field.hint,
                            secret=field.secret,
                            optional=field.optional)
    if value or not field.optional:
        creds[field.name] = value

# 3. Write the temp creds file (mode 0600).
write_json(schema.expected_creds_path, creds, mode=0o600)

# 4. Run the login command with the temp file. The schema's next_cmd
#    contains the right flag (--creds-json for discord, --session-json
#    for twitter, --cookies-json for linkedin/instagram). Substitute
#    <path> with expected_creds_path.
login_cmd = schema.next_cmd.replace("<path>", schema.expected_creds_path)
run(login_cmd)

# 5. Validate (read-only first; live only with explicit confirmation).
run("augmentagent channel " + ch + " validate")

# 6. Always delete the temp file, even on failure.
delete(schema.expected_creds_path)
```

The skill must:

- Mask any field flagged `secret = true` in the schema. Never echo it
  back to the transcript.
- Honour `optional = true`. The Instagram `username` and `rur` fields
  are optional; leave them out of the JSON when the user skips.
- Use the schema's `next_cmd` rather than hard-coding the login
  invocation. Channels do not all share the same flag name; the schema
  carries the right one.
- Always delete the temp file in a `finally`-equivalent block. The
  credentials are session-bearer; leaking them onto disk is the worst
  failure mode.
- For LinkedIn, the schema returns two methods. Default to
  `devtools_cookies` unless the user explicitly confirms they have a
  recent `/intercept` capture, in which case skip the AskUserQuestion
  loop entirely and run the `browser_intercept` script directly (it
  has zero fields).

After login succeeds, follow the channel's sub-file Steps section for
arming (if the channel has an arming gate) and the live validation
sign-off. See `components/env-file.md` for the arm/disarm semantics.

## Reading .env.example as Runtime Checklist

The `.env.example` file at the repo root is the canonical list of channel
arming gates and feature flags. Treat it as a checklist, not as a template
to copy from inside this skill.

At the top of any branch that needs to reason about which channels should be
on, do this:

1. `cat .env.example` to load the current gate list.
2. Extract every line under a `CHANNEL ARMING GATES` block, every variable
   whose name ends in `_ENABLED`, `_CONFIRM`, `_RESOLVE`, or is documented
   inline as a gate.
3. Cross-reference each gate against the channels reported by `augmentagent
   status --json`.
4. For each gate the user expects on but the status JSON reports off, tell
   them which line of `.env` to set and what the live-credential prerequisite
   is (cookies, token, QR pairing, OAuth callback, and so on).

Do not hand-edit `.env` from inside the skill. Writes go through
`augmentagent env set <KEY> <VALUE>` (issue #12, shipped), which
persists to the sqlite `config` table (which wins over `.env` at
startup); the skill never edits `.env` directly.

The `.env.example` is also the place to discover new channels added between
skill updates. If `status --json` reports a channel name the skill has never
seen, fall back to grepping `.env.example` for that channel's block and
treat the comment header there as the authoritative description.

## Writing Style

STRICT RULES, violations will cause the message to be regenerated:

- Linux-only. Never reference macOS, launchd, Keychain, sparsebundle,
  `/Volumes`, Homebrew, `brew`, or `scp` from a Mac. If the user mentions
  any of these, tell them this skill is for the Linux daemon host and stop.
- No emojis. Zero. None in headings, none in lists, none in error messages.
- No emdashes and no endashes. Use commas, periods, or semicolons.
- Direct prose. State the action, then run it. Skip filler like "I will now",
  "Let me", "Going to". Just do the thing.
- Surface CLI stderr verbatim on failure. Do not paraphrase a Rust panic or
  a systemd error into friendlier English; the verbatim text is what the
  user needs to grep for.
- Short paragraphs. One to three sentences. Lists are fine; nested lists
  beyond two levels are not.
- Plural-of-channel is "channels", not "channel-types" or "integrations".
- The product is "AugmentAgent", one word, capitalized that way.

## Safety

Some flags can drop credentials, wipe local state, or trigger a re-pairing
flow that locks the user out of a sidecar. The skill must never run any of
the following without an explicit AskUserQuestion confirmation that quotes
the exact command back to the user:

- Any subcommand carrying `--purge`, `--force`, `--reset`, `--reauth`, or
  `--wipe`.
- `augmentagent channel <name> validate --allow-live` when the channel is
  Twitter, Instagram, or WhatsApp. These have ban-risk gates; live calls
  count against quota.
- `systemctl --user disable` on any augmentagent unit. Always prefer
  `augmentagent service restart` and `augmentagent service stop`.
- Anything that writes to `.env` directly. Writes go through `augmentagent
  env set <KEY> <VALUE>`, which persists to the sqlite `config` table; the
  daemon merges that over the `.env` values at startup. The skill does not
  edit `.env` itself.
- `augmentagent install <component>` and `augmentagent uninstall
  <component>`. These shell out to `scripts/install-*.sh` and rewrite
  systemd user units. Idempotent in theory, but always confirm with
  AskUserQuestion that quotes the component name.
- `augmentagent doctor`'s `suggested_cmd` runs. Doctor never runs these
  itself (it stays read-only); the skill is the one that can invoke them
  after confirmation.

The skill is also forbidden from running `cargo` builds without explicit
user confirmation. The `--rebuild` flag on `augmentagent install` invokes
`cargo build --release` followed by `npm run build`; quote that back to
the user before passing it.

## Allowed Tools

The skill is mutation-aware but every mutating call is gated by
AskUserQuestion. It may use:

- `Bash` for running `augmentagent` CLI invocations (`status`, `doctor`,
  `service`, `logs`, `channel`, `setup oauth`, `setup harvest`, `env`,
  `install`, `uninstall`), `cat .env.example`, `systemctl --user status`
  (read-only), and `journalctl --user-unit` reads when the wrapped logs
  command is unavailable.
- `AskUserQuestion` for every action that mutates state: `service restart`,
  `channel validate`, `channel arm` / `disarm`, `env set` / `env unset`,
  `install` / `uninstall` of any component, `setup oauth` (the browser
  open), and every doctor `suggested_cmd` run.
- `Read` for opening `reference/status-schema.md`,
  `reference/troubleshooting.md`, `components/*.md`, and `channels/*.md`
  during triage and per-channel routing.
- Temporary file writes under `/tmp` for the cookie-harvest creds files
  only; see the cookie-harvest sub-flow for the lifecycle. The skill
  never writes anywhere else in the repo.

The skill must not use Edit or Write tools on any file in the repo outside
of `/tmp` creds files, must not fetch remote URLs, and must not invoke
other skills. If the user needs a different skill (for example, the
email-triage skill), tell them to invoke it directly.

## Gotchas

- SSH and headless hosts. The browser sidecar (Instagram, Twitter login
  flows) needs a display or a headed launch profile; if the user is on a
  pure SSH session with no DISPLAY, channel validation for those channels
  will fail with a sidecar timeout. Surface this; do not retry blindly.
- Keyring locked. The agent stores some tokens in the user's D-Bus secret
  service (gnome-keyring or kwallet). On a fresh login with no graphical
  session, the keyring is locked and reads return empty. Tell the user to
  unlock the keyring (graphical login, or `dbus-run-session` for headless
  flows) and re-run status.
- Arming stays sticky across restarts. If a channel was armed and then the
  gate env was removed, the channel reports `armed=false` from the env but
  the daemon may still hold a live session until restart. Use `augmentagent
  service restart` to pick up env changes.
- Dashboard is optional in Phase 1. Earlier docs treated the dashboard as
  required; it is now a separate sidecar with its own install script. A
  fully-functional install can run without it. Do not flag a missing
  dashboard as a fault unless the user explicitly wants it.
- `status --json` is the contract. The shape is locked at `schema_version:
  1`; see `reference/status-schema.md`. If a future CLI bumps the schema,
  the skill must check `schema_version` before parsing.
- Validation is rate-limited. Do not loop `channel validate` to "make sure"
  a fix took; one run per change.
