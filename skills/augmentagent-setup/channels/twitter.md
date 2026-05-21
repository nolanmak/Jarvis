# Twitter / X Setup

## Category

cookie harvest

## When to use

User wants AugmentAgent to read their X DMs, post, or reply on X. Status
JSON reports `channels.twitter.configured = false` and the user wants
Twitter on.

## Prereqs

- A logged-in X session at `https://x.com/messages`.
- Chrome devtools (Application -> Cookies, plus Network for the
  `user_id` lookup).
- Approval-gated: every live write needs `--allow-live` and the arming
  gate `AUGMENTAGENT_TWITTER_REAL_ENABLED` set true.

## Steps

Run the in-skill cookie-harvest loop (see SKILL.md "Cookie-harvest
sub-flow") with `--channel twitter`. The mechanical sequence:

1. Parse the schema:
   ```
   augmentagent setup harvest twitter --non-interactive --json --creds-out /tmp/twitter-creds-$$.json
   ```
   Schema lists the `devtools_cookies` method, four fields (`user_id`,
   `screen_name`, `auth_token`, `ct0`), and the `next_cmd` template
   `augmentagent twitter login --session-json <path>`. Note this channel
   uses `--session-json`, not `--creds-json`.
2. Echo `doc_steps` verbatim so the user knows where to find each value.
3. AskUserQuestion per field. Mask `auth_token` and `ct0`
   (`secret = true`).
4. Write the values as JSON to the `expected_creds_path` printed by the
   schema. Mode 0600.
5. Run:
   ```
   augmentagent twitter login --session-json /tmp/twitter-creds-$$.json
   ```
   Persists to the keychain.
6. Delete the temp file:
   ```
   rm /tmp/twitter-creds-$$.json
   ```
7. Run the read-only validate, then ask before the live one:
   ```
   augmentagent channel twitter validate
   ```
   If the user wants the live sign-off (counts against rate quota; ask
   first via AskUserQuestion quoting the command back):
   ```
   augmentagent channel twitter validate --allow-live true
   ```
8. If the user wants AugmentAgent to actually post or DM on X, arm the
   channel:
   ```
   augmentagent channel twitter arm
   ```
   This flips `AUGMENTAGENT_TWITTER_REAL_ENABLED` true in sqlite and
   prints the restart-required JSON. Run
   `augmentagent service restart` to pick it up.

## Validate

```
augmentagent status --channel twitter --json
augmentagent channel twitter validate
augmentagent channel twitter validate --allow-live true   # live sign-off
```

The first prints the twitter block. The second is mock-only; it
exercises the harness wiring without touching x.com. The live variant
hits x.com once (auth, UserTweets, DM inbox) and is the only confirmation
the credentials work end to end. See `docs/twitter-protocol.md` for the
full operator runbook.

## Common errors and fixes

- 401 during validate. `auth_token` expired or `ct0` rotated. Re-harvest
  both from the same browser session and retry login.
- 403 with "missing csrf token". The `ct0` cookie and the `x-csrf-token`
  header must match. Re-grab `ct0` and retry; do not paste from two
  different sessions.
- "browser sidecar timeout" on a posting flow. X's posting path uses the
  browser sidecar; on a pure SSH host without DISPLAY the sidecar
  cannot launch. Run validation from a graphical session, or enable the
  sidecar's headless profile. See `reference/troubleshooting.md`.
- `validate --allow-live` returns "REQUIRES LIVE OPERATOR VALIDATION"
  without an HTTP call. The harness is in mock mode (default). Pass
  `--allow-live true` explicitly.

## Disarm / undo

Twitter has an arming gate (`AUGMENTAGENT_TWITTER_REAL_ENABLED`). To
disarm:

```
augmentagent channel twitter disarm
augmentagent service restart
```

To also remove credentials, delete the keychain slot manually
(`augmentagent/twitter/<screen_name>`).
