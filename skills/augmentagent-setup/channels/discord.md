# Discord Setup

## Category

cookie harvest

## When to use

User wants AugmentAgent to read Discord DMs, post approval cards, or
take any approval-gated action. Discord is the canonical approval
surface; without it every approval-gated feature is dark. Also runs when
status JSON shows `core_keys.discord_bot = false` paired with
`channels.discord.configured = false`.

## Prereqs

- A logged-in Discord browser session (`https://discord.com/app`).
- Chrome devtools available on the user's machine (the harvest values
  come from the Network tab).
- For approval routing you also need `DISCORD_BOT_TOKEN`,
  `DISCORD_CHANNEL_ID`, `DISCORD_ALLOWED_USER_ID` in `.env`. The cookie
  harvest below only covers the user-token credentials; the bot side is
  configured separately via the Discord developer portal and pasted
  into `.env`.

## Steps

Run the in-skill cookie-harvest loop (see SKILL.md "Cookie-harvest
sub-flow") with `--channel discord`. The mechanical sequence:

1. Parse the schema:
   ```
   augmentagent setup harvest discord --non-interactive --json --creds-out /tmp/discord-creds-$$.json
   ```
   The schema lists the `devtools_headers` method, four fields
   (`user_id`, `token`, `super_properties_b64`, `user_agent`), and the
   `next_cmd` template `augmentagent discord login --creds-json <path>`.
2. Echo `doc_steps` verbatim so the user knows where each value lives in
   devtools.
3. For each field, ask via AskUserQuestion. Mask `token` and
   `super_properties_b64` (`secret = true` in the schema).
4. Write the four values as JSON to the `expected_creds_path` printed by
   the schema. Mode 0600.
5. Run:
   ```
   augmentagent discord login --creds-json /tmp/discord-creds-$$.json
   ```
   This validates via `GET /users/@me`, persists to the keychain slot
   `augmentagent/discord/default`, and updates the sqlite row.
6. Delete the temp file:
   ```
   rm /tmp/discord-creds-$$.json
   ```
7. Run validation:
   ```
   augmentagent channel discord status
   ```

## Validate

```
augmentagent channel discord status
augmentagent discord list-guilds --json
```

`status` should print "logged in" with the resolved user id. `list-guilds`
is the live-credential probe; if it returns the user's guild list, the
token is good. See `docs/discord-protocol.md` for the full request shape.

## Common errors and fixes

- 401 on `GET /users/@me` during login. The `token` value is wrong or
  was pasted with a `Bearer ` prefix. The Discord internal API uses the
  raw token, no prefix. Re-harvest, retry.
- 403 "You need to verify your account". Discord flagged the session as
  bot-like. The `user_agent` and the `browser_user_agent` field inside
  the decoded `x-super-properties` must match exactly. Re-grab both
  from the same request, retry.
- Keychain save fails on a headless host. The user's D-Bus secret
  service is locked or missing. Have them unlock the gnome-keyring or
  wrap the daemon start in `dbus-run-session`. See
  `reference/troubleshooting.md`.
- "MESSAGE CONTENT intent required" when approval cards fail to parse
  replies. This is the bot side, not the user-token side. Toggle MESSAGE
  CONTENT intent in the Discord developer portal for the bot and run
  `augmentagent service restart`.

## Disarm / undo

Discord has no arming gate (it is the approval surface). To remove the
user-token credentials:

1. Delete the keychain slot `augmentagent/discord/default` manually
   (Seahorse or `secret-tool clear ...`).
2. Empty `DISCORD_BOT_TOKEN` in `.env` to disable the bot side, then
   `augmentagent service restart`.

Removing Discord disables approvals; warn the user before doing this.
