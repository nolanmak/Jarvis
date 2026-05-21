# Meetup Setup

## Category

token paste

## When to use

User wants AugmentAgent to follow a Meetup group and surface upcoming
events. Status JSON reports `channels.meetup.configured = false` and the
user wants Meetup on.

Note: today's Meetup channel is subscription-based (group urlname +
mode); there is no per-user auth token to paste. Meetup events are read
from public group endpoints. The "token paste" category applies because
the next iteration (private RSVP-aware mode) will require an API key;
for now the only paste is the group urlname itself.

## Prereqs

- A Meetup group urlname (the slug from the URL, e.g.
  `https://www.meetup.com/code-and-coffee` -> urlname `code-and-coffee`).
- No authentication is required for public group event reads today.

## Steps

1. AskUserQuestion: ask for the Meetup group urlname (no leading slash,
   no `https://`).
2. AskUserQuestion: ask for the subscription mode. Valid modes match
   the subscription enum in `augmentagent_store::SubscriptionMode`
   (typical values: `notify`, `digest`, `silent`). Default to whatever
   the user states; the CLI rejects unknown modes with a clear error.
3. Subscribe:
   ```
   augmentagent meetup subscribe --urlname <urlname> --mode <mode>
   ```
   This writes a row to the `subscriptions` table; the daemon picks up
   the new group on the next poll cycle.
4. (Optional) Trigger a one-shot poll to confirm events surface:
   ```
   augmentagent meetup poll-once --dry-run true
   ```

## Validate

```
augmentagent status --channel meetup --json
augmentagent meetup subscriptions --json
augmentagent meetup poll-once --dry-run true
```

`configured` should be `true` once at least one subscription exists.
`subscriptions --json` lists the active groups. The `poll-once --dry-run`
exercises the live HTTP call without persisting events; the printed
debug output should include event titles from the group.

## Common errors and fixes

- "invalid mode" on subscribe. The mode string does not match the
  channel's enum. Run `augmentagent meetup subscribe --help` to see
  valid values, or re-ask the user.
- `poll-once` returns zero events for a known-active group. The
  urlname is wrong (Meetup serves a 404 page for invalid slugs but the
  channel may swallow that). Have the user paste the full Meetup URL and
  re-extract the urlname.
- Polls 429 with rate-limit headers. Meetup's public endpoint has a
  shared rate limit; the daemon backs off automatically. If the user
  hits it repeatedly, reduce the subscription's mode or add a longer
  poll interval.

## Disarm / undo

Meetup has no arming gate. To stop following a group:

```
augmentagent meetup unsubscribe --id <id>
```

`id` comes from `augmentagent meetup subscriptions --json`. To stop
every Meetup poll, remove every subscription; with no active rows the
channel goes quiet.
