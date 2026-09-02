# Journaling over Discord (ShadowNote integration)

Epic: MyAgentAssistant#425. The agent reads the ShadowNote journal into the
wiki (#427), prompts for morning/night/weekly journaling on a schedule
(#429), and writes finished entries back to ShadowNote encrypted exactly
like the app's own (#428).

## Scheduled prompts (config only — `/loop` crons)

Create these from the Discord DM (adjust times/timezone to taste; the
parser accepts natural language and `for <duration>` expiry):

```
/loop 0 8 * * * Post my morning journaling prompt: greet me briefly, then ask (1) how I slept / how I feel, (2) top 1-3 intentions for today, (3) one thing I'm grateful for. Keep it to a short DM, no preamble.

/loop 0 21 * * * Post my evening journaling prompt: ask (1) what actually happened today vs. this morning's intentions, (2) highlight + lowlight, (3) anything on my mind before sleep. Short DM, warm tone.

/loop 0 18 * * 0 Post my weekly review journaling prompt: ask me to review the week — wins, misses, lessons, and top 3 priorities for next week. If the wiki has journal entries from this week, quote 2-3 short excerpts back to me as memory joggers.
```

Manage with `!loops` / `/loop list` / `/loop stop <id>`.

## Saving entries — `!journal`

- `!journal <text>` — save the text as today's entry, verbatim.
- `!journal done [title]` — compose an entry from the recent conversation
  (your replies to the prompt above) and save it. Optional title overrides
  the composed one.
- `!journal` / `!journal help` — usage.

Saved entries are envelope-encrypted (KMS data key + the app's CryptoJS
format) and created through the AppSync API, so they appear in the
ShadowNote app like any hand-written entry, and are ingested into the wiki
immediately.

If the box isn't configured (`SHADOWNOTE_*` keys absent), `!journal`
answers with a not-configured notice and saves nothing; normal replies
still reach the wiki through the regular Discord ingest.

## Reading the journal — nothing to do

The daemon's journal channel polls `syncEntries` every 30 minutes and
ingests new/changed entries into the wiki (`journal poll-once [--dry-run]`
runs one pass manually). Ask about your journal through the normal wiki
query channel.

## Configuration (see the private ShadowNoteReborn#20 issue for values)

| Key | What |
|---|---|
| `SHADOWNOTE_APPSYNC_URL` | AppSync GraphQL endpoint |
| `SHADOWNOTE_OWNER_ID` | `Entry.ownerId` partition-key value |
| `SHADOWNOTE_OWNER_FIELD` | Cognito `owner` value for created entries (defaults to owner id) |
| `SHADOWNOTE_KMS_KEY_ARN` | CMK for the write path |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` | the agent's IAM user |

Keys load keyring-first (`augmentagent/api-key` slot), env/`.env` fallback,
same as every other secret.
