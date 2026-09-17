# Discord history → Jarvis

Syncs Discord messages you opt into into the agent's searchable conversation
history, the same way WhatsApp and iMessage history work. Wiki-ask can then
answer questions like "what did Alex and I talk about in DMs last month" or
"what was decided in #general" through `search_conversation_history` and
`read_conversation_thread`.

A sync never calls a model: it only stores messages. Rows are stored already
marked processed, so they are never triaged or answered.

## Risk

This uses your Discord **user** session (the same credentials as the live
Discord channel, see `docs/discord-protocol.md`). Automating a user account is
against Discord's Terms of Service and can get the account flagged. The sync
keeps the live channel's pace (every ~4 h with jitter, one request at a time)
and skips conversations with nothing new without making a request. If you
don't want that risk, leave the settings below unset.

## Setup

1. Log in once: `augmentagent discord login --creds-json <file>`, or use the
   dashboard's Subscriptions → Connect Discord. Credentials go to the OS
   keyring.
2. Choose what to include in `.env`. Nothing is included by default.

   ```dotenv
   # Your DMs and group DMs
   AUGMENTAGENT_DISCORD_EXPORT_DMS=1
   # Servers whose readable text channels are included (comma-separated ids)
   AUGMENTAGENT_DISCORD_EXPORT_GUILDS=123456789012345678
   ```

   Find server ids with `augmentagent discord list-guilds`. Servers not listed
   are never read. Channels you can't read are skipped.
3. Backfill now instead of waiting for the daemon:

   ```sh
   augmentagent discord history-sync --max-pages-per-channel 2000
   ```

   It prints a JSON report and resumes from saved per-conversation cursors, so
   re-running is safe. The daemon then keeps history current (default cap: 50
   pages = 5,000 messages per conversation per run).

## Behavior

- `message_id` is the Discord message id, the same key the live Discord
  channel uses, so a message both paths see is stored once.
- For a channel with an active `priority` subscription, history stops at the
  newest message the live channel has already processed, so new messages
  still get live triage.
- Stored fields: speaker (`me` for you), text, attachment name/type/URL (files
  aren't downloaded), timestamp. Rows use `platform = "discord"` and `kind`
  `dm`, `group`, or `guild_channel`.
- Cursors live in `discord_history_sync_state`; the next background run time
  is in `discord_history_meta`, so daemon restarts don't add syncs.
