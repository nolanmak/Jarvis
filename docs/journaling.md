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

The daemon's journal channel polls `syncEntries` every 10 minutes and
ingests new/changed entries into the wiki (`journal poll-once [--dry-run]`
runs one pass manually). Ask about your journal through the normal wiki
query channel.

## Sync health and recovery

The ten-minute interval is a schedule, not evidence of success. The journal
importer and the private Git mirror are separate stages. A healthy Git mirror
can keep committing other wiki changes while the journal importer is stale.
`augmentagent journal status` reports the last completed source watermark and
whether a resumable backlog exists, without exposing pagination tokens.

An incomplete historical import can be repaired without sending every old entry
through the optional derived-memory model pipeline:

```sh
augmentagent --wiki-dir ./wiki journal backfill --archive-only --max-entries 200
```

This command writes to the private wiki and checkpoints successful entries.
Re-run until the pass reports a watermark and `journal status` has no cursor.
For an old backfill watermark, run another pass to catch changes since its start.
The ordinary daemon then follows new edits. `--dry-run true` reads/counts without
writing pages or advancing checkpoints. Failed decryption or durable writes stop
the live pass and retain the page for retry; previously acknowledged entries
with missing/stale pages are repaired. Archive-only recovery does not regenerate
derived memories for those entries; the readable journal remains available.

## Versioned backups and recovering an overwritten task

Use the existing **private** knowledge-base Git repository, not the public
application repository. Current readable entries are under
`journal/YYYY/YYYY-MM-DD-ID.md`. Immutable Markdown snapshots are under
`journal/history/ENTRY-HASH/REVISION-HASH.md`. Before replacing a current page,
the importer preserves both that page (including legacy content) and the incoming
revision. Repeated identical imports are idempotent; multiple observed changes
between Git syncs remain distinct snapshots. Temporary and writer lock files end
in `.lock` and must remain ignored by the private repository.

The mirror timer commits and pushes these snapshots every ten minutes. Verify
the configured remote is private before enabling publication. A successful source
poll followed by a successful mirror pass normally takes up to about twenty
minutes end-to-end; outages/backlogs can delay this. The public application's
ignored `wiki/` directory must remain untracked.

List saved revisions using the entry ID from the page's frontmatter or
`journal show`, then export a full revision hash to a separate file:

```sh
augmentagent --wiki-dir ./wiki journal history --entry-id ENTRY_ID
augmentagent --wiki-dir ./wiki journal history --entry-id ENTRY_ID --revision FULL_SHA256 > recovered.md
```

Listing includes creation/update/version metadata. Export verifies the checksum
and returns the exact saved Markdown bytes without replacing the current page.
Git history remains another recovery route for pages already committed before
this feature. Treat `journal/history/` as historical evidence, not today's task
list. Backups preserve versions **actually observed by a successful import**;
an edit overwritten in ShadowNote before any successful observation cannot be
reconstructed by this poller. Snapshots contain decrypted readable text and
metadata, not a full encrypted application/attachment backup.

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
