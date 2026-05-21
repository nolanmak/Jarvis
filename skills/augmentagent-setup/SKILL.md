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
read `state` and route. If it returns non-zero with no JSON, surface stderr
and consult troubleshooting.

The four branches:

### FRESH INSTALL, not configured

Enter this branch when `status.state == "fresh"` or when the JSON shows no
configured channels and no running services. Phase 2 (issue #9) will replace
this stub with a guided installer that runs the three install scripts in the
right order and harvests credentials with permission. Phase 3 (issue #13)
adds OAuth orchestration on top of that.

For now, print the manual fallback verbatim and stop:

```
./scripts/install-autostart.sh && ./scripts/install-dashboard.sh && ./scripts/install-autoupdate.sh
```

Then tell the user: after those three scripts finish, copy `.env.example` to
`.env`, fill the core API keys and the Discord block, and re-run `/setup`.
Do not attempt to edit `.env` yourself; that is a Phase 3 capability behind
issue #12.

### PARTIAL INSTALL, some channels configured

Enter this branch when `status.state == "partial"`, meaning some services are
up but at least one channel reports `armed=false` or `validated=false` where
the user appears to want it on. The Phase 2 setup orchestrator (issue #9)
will own this branch; for now it is a stub.

Print the same manual fallback as Fresh Install, then add:

- List the channels reported `armed=false` from the status JSON.
- For each, refer the user to `.env.example` (the channel arming gates block)
  and to the per-channel docs under `docs/`.
- Recommend running `/setup` again once they have flipped the gate envs.

Do not run install scripts in this branch yourself; partial installs can be
in a half-applied state, and an unguarded re-run can clobber working units.

### REPAIR, channel validation failing

Enter this branch when `status.state == "repair"` or when the JSON shows a
service in a failed state, a validation that has flipped from green to red,
or a unit that has been restarting in a loop. Phase 3 (issue #11) adds the
`augmentagent doctor` command that will own this branch end to end.

For now, stub:

1. Print the failing units and channels from the status JSON.
2. Pull recent logs for each via `augmentagent logs --unit <name>` (capped
   at the last 200 lines so the chat stays readable).
3. Consult `reference/troubleshooting.md` for the symptom table.
4. Surface CLI stderr verbatim; do not paraphrase.
5. Recommend a single `augmentagent service restart --unit <name>` only if
   the troubleshooting table maps the symptom to a clean restart fix.

Never run `--purge`, `--force`, or `--reset` flags in this branch. Repair is
read-mostly until the user explicitly asks for a destructive action and the
skill confirms via AskUserQuestion.

### MAINTENANCE, fully configured

Enter this branch when `status.state == "ok"` and the JSON shows every
channel the user expects is armed and validated. This is the wired branch;
the others are stubs until Phases 2 and 3 land.

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

The 12 channels split into three setup categories:

### callback OAuth (dashboard hosts the callback)

| Channel | Sub-file                  | Dashboard start URL                                     |
| ------- | ------------------------- | ------------------------------------------------------- |
| gmail   | `channels/gmail.md`       | `http://localhost:<port>/oauth/gmail/start`             |
| drive   | `channels/drive.md`       | `http://localhost:<port>/oauth/googledrive/start`       |
| slack   | `channels/slack.md`       | `http://localhost:<port>/oauth/slack/start`             |
| reddit  | `channels/reddit.md`      | `http://localhost:<port>/api/reddit/auth`               |

Dashboard must be installed and reachable. Read
`components/systemd-units.md` first if `dashboard.reachable = false`.

### cookie harvest (devtools paste, validated via harvest schema)

| Channel   | Sub-file                  | Schema source                                  |
| --------- | ------------------------- | ---------------------------------------------- |
| discord   | `channels/discord.md`     | `augmentagent setup harvest discord …`         |
| twitter   | `channels/twitter.md`     | `augmentagent setup harvest twitter …`         |
| linkedin  | `channels/linkedin.md`    | `augmentagent setup harvest linkedin …`        |
| instagram | `channels/instagram.md`   | `augmentagent setup harvest instagram …`       |

Every cookie-harvest channel uses the in-skill loop below.

### token paste (user copies a token from a vendor portal)

| Channel       | Sub-file                          | Token source                                     |
| ------------- | --------------------------------- | ------------------------------------------------ |
| github        | `channels/github.md`              | PAT at `https://github.com/settings/tokens`      |
| meetup        | `channels/meetup.md`              | Group urlname (no auth today)                    |
| deftform      | `channels/deftform.md`            | Workspace API token + webhook secret             |
| telegram-bot  | `channels/telegram-bot.md`        | BotFather token                                  |

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

Do not edit `.env` from inside the skill. The Phase 3 env subcommand
(issue #12) will own writes; until it lands, the skill is read-only on
configuration files.

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
- Anything that writes to `.env` directly. The Phase 3 env subcommand
  (issue #12) is the only sanctioned writer; until it ships, point the user
  at the file and let them edit by hand.

The skill is also forbidden from running `cargo` builds or installer scripts
without explicit user confirmation. Those scripts are idempotent in theory
but interact with systemd unit files; a silent re-run during a confused
session can mask state.

## Allowed Tools

This skill is read-mostly. It may use:

- `Bash` for running `augmentagent` CLI invocations, `cat .env.example`,
  `systemctl --user status` (read-only), and `journalctl --user-unit` reads
  when the wrapped logs command is unavailable.
- `AskUserQuestion` for every action that mutates state (service restart,
  channel validate, anything in the Safety list).
- `Read` for opening `reference/status-schema.md` and
  `reference/troubleshooting.md` when triaging.

The skill must not use Edit or Write tools on any file in the repo, must not
fetch remote URLs, and must not invoke other skills. If the user needs a
different skill (for example, the email-triage skill), tell them to invoke
it directly.

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
