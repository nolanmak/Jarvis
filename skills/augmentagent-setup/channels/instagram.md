# Instagram Setup

## Category

cookie harvest

## When to use

User wants AugmentAgent to read their Instagram DMs, post feed/carousel/
reel/story, or comment via the private web API. Status JSON reports
`channels.instagram.configured = false` and the user wants Instagram on.

## Prereqs

- A logged-in Instagram session at `https://www.instagram.com/`.
- Chrome devtools (Application -> Cookies).
- For any live posting or DM, the arming gate
  `INSTAGRAM_REAL_ACCOUNT_ENABLED` must be set to `true` AND
  `instagram validate` must pass on a live session.
- Posting paths (feed images, reels) use the browser sidecar; on a pure
  SSH host without DISPLAY the sidecar cannot launch. Install the
  sidecar stack first (`augmentagent install browser-sidecar`).

## Steps

Run the in-skill cookie-harvest loop (see SKILL.md "Cookie-harvest
sub-flow") with `--channel instagram`. The mechanical sequence:

1. Parse the schema:
   ```
   augmentagent setup harvest instagram --non-interactive --json --creds-out /tmp/instagram-creds-$$.json
   ```
   Schema lists seven fields: `ds_user_id`, `username` (optional),
   `sessionid`, `csrftoken`, `mid`, `ig_did`, `rur` (optional). The
   `next_cmd` template is `augmentagent instagram login --cookies-json <path>`.
2. Echo `doc_steps` verbatim.
3. AskUserQuestion per field. Mask `sessionid`, `csrftoken`, `mid`,
   `ig_did`, `rur` (`secret = true`). `username` and `rur` are optional;
   accept empty values for those two.
4. Write the values as JSON to the `expected_creds_path`. Omit any
   optional fields the user left empty. Mode 0600.
5. Run:
   ```
   augmentagent instagram login --cookies-json /tmp/instagram-creds-$$.json
   ```
6. Delete the temp file:
   ```
   rm /tmp/instagram-creds-$$.json
   ```
7. Run the operator validation runbook. Read-only first:
   ```
   augmentagent channel instagram validate
   ```
   (If the channel does not yet expose `validate` via the router, use
   the protocol's `--dry-run` smoke as documented in
   `docs/instagram-protocol.md` §"Operator validation runbook".)
8. Arm the channel only after read-only validation passes:
   ```
   augmentagent channel instagram arm
   augmentagent service restart
   ```
   Then tell the user to flip `INSTAGRAM_REAL_ACCOUNT_ENABLED=true` in
   `.env` and restart again. The skill is read-only on `.env`.

## Validate

```
augmentagent status --channel instagram --json
augmentagent channel instagram validate
```

`status` should show `configured = true` and (after arming) `armed =
true`. Validate exercises the read endpoints without POSTing. See
`docs/instagram-protocol.md` §"Operator validation runbook" for the full
sign-off including the optional `--exercise-writes` step with a fixed
marker string.

## Common errors and fixes

- 403 "checkpoint_required" on any request. Instagram challenged the
  session. The user must log into instagram.com via Chrome on a residential
  IP, complete the challenge, then re-harvest cookies. Do not retry
  blindly; it counts against ban risk.
- "browser sidecar timeout" on a posting flow. Sidecar stack missing or
  no DISPLAY. Run `augmentagent install browser-sidecar` and retry from
  a graphical session. See `reference/troubleshooting.md`.
- "rate limit" on validate. Instagram's private API has a hard daily
  quota; do not loop validation. One run per change, then stop.
- "missing csrftoken" on POST. The `csrftoken` cookie and the
  `x-csrftoken` request header must match. Re-grab `csrftoken` and
  retry login.

## Disarm / undo

```
augmentagent channel instagram disarm
augmentagent service restart
```

Also tell the user to flip `INSTAGRAM_REAL_ACCOUNT_ENABLED=false` in
`.env`. To remove credentials, delete the keychain slot manually
(`augmentagent/instagram/<ds_user_id>`).
